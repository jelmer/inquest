//! Repository abstraction for storing test results
//!
//! This module provides traits and implementations for storing and retrieving
//! test results. The on-disk format is compatible with the Python version.

use crate::error::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

pub mod inquest;
pub mod test_run;
#[cfg(feature = "testr")]
pub mod testr;

pub use test_run::{
    estimate_progress, RunId, RunMetadata, StreamInterruption, TestFlakiness, TestId, TestResult,
    TestRun, TestStatus,
};

/// Abstract repository trait for test result storage
///
/// # Examples
///
/// ```
/// use inquest::repository::{Repository, RepositoryFactory};
/// use inquest::repository::inquest::InquestRepositoryFactory;
/// use tempfile::TempDir;
///
/// # fn main() -> inquest::error::Result<()> {
/// // Create a temporary directory for the repository
/// let temp = TempDir::new().unwrap();
///
/// // Initialize a new repository
/// let factory = InquestRepositoryFactory;
/// let mut repo = factory.initialise(temp.path())?;
///
/// // Begin a test run — the repository assigns the run ID
/// let (run_id, writer) = repo.begin_test_run_raw()?;
/// println!("Started test run with ID: {}", run_id);
/// drop(writer); // finish the run
///
/// // Retrieve the latest run
/// let latest = repo.get_latest_run()?;
/// println!("Latest run has {} tests", latest.total_tests());
///
/// // Get failing tests
/// let failing = repo.get_failing_tests()?;
/// println!("Found {} failing tests", failing.len());
/// # Ok(())
/// # }
/// ```
pub trait Repository {
    /// Get a specific test run by ID
    fn get_test_run(&self, run_id: &RunId) -> Result<TestRun>;

    /// Begin inserting a raw test run stream, returning (run_id, writer)
    /// This preserves the original stream byte-for-byte including non-subunit output
    /// The caller should write the raw subunit bytes to the returned writer
    fn begin_test_run_raw(&mut self) -> Result<(RunId, Box<dyn std::io::Write + Send>)>;

    /// Insert a test run (convenience method for tests - prefer begin_test_run_raw in production)
    ///
    /// This is a convenience wrapper around begin_test_run_raw() for test code.
    /// Production code should prefer the streaming API for better performance.
    fn insert_test_run(&mut self, run: TestRun) -> Result<RunId> {
        use std::io::Write;

        let (run_id, mut writer) = self.begin_test_run_raw()?;
        crate::subunit_stream::write_stream(&run, &mut *writer)?;
        writer.flush()?;
        drop(writer);

        // Update failing tests and times
        self.replace_failing_tests(&run)?;

        let mut times = std::collections::HashMap::new();
        for result in run.results.values() {
            if let Some(duration) = result.duration {
                times.insert(result.test_id.clone(), duration);
            }
        }
        if !times.is_empty() {
            self.update_test_times(&times)?;
        }

        Ok(run_id)
    }

    /// Insert a partial test run (convenience method for tests - prefer begin_test_run_raw in production)
    ///
    /// In partial mode, the failing test tracking is additive:
    /// - Keeps existing failures
    /// - Adds new failures from this run
    /// - Removes tests that now pass
    ///
    /// In full (non-partial) mode, all previous failures are cleared.
    fn insert_test_run_partial(&mut self, run: TestRun, partial: bool) -> Result<RunId> {
        use std::io::Write;

        let (run_id, mut writer) = self.begin_test_run_raw()?;
        crate::subunit_stream::write_stream(&run, &mut *writer)?;
        writer.flush()?;
        drop(writer);

        // Update failing tests based on mode
        if partial {
            self.update_failing_tests(&run)?;
        } else {
            self.replace_failing_tests(&run)?;
        }

        // Update times
        let mut times = std::collections::HashMap::new();
        for result in run.results.values() {
            if let Some(duration) = result.duration {
                times.insert(result.test_id.clone(), duration);
            }
        }
        if !times.is_empty() {
            self.update_test_times(&times)?;
        }

        Ok(run_id)
    }

    /// Update failing tests additively (for partial runs)
    fn update_failing_tests(&mut self, run: &TestRun) -> Result<()>;

    /// Replace all failing tests (for full runs)
    fn replace_failing_tests(&mut self, run: &TestRun) -> Result<()>;

    /// Get the latest test run
    fn get_latest_run(&self) -> Result<TestRun>;

    /// Get the raw subunit stream for a test run as a reader
    fn get_test_run_raw(&self, run_id: &RunId) -> Result<Box<dyn std::io::Read>>;

    /// Get the list of currently failing tests
    fn get_failing_tests(&self) -> Result<Vec<TestId>>;

    /// Get the raw subunit stream for failing tests as a reader
    fn get_failing_tests_raw(&self) -> Result<Box<dyn std::io::Read>>;

    /// Get test execution times
    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>>;

    /// Get test execution times for specific test IDs
    fn get_test_times_for_ids(&self, test_ids: &[TestId]) -> Result<HashMap<TestId, Duration>>;

    /// Update test execution times
    fn update_test_times(&mut self, times: &HashMap<TestId, Duration>) -> Result<()>;

    /// Get the next run ID that will be assigned
    fn get_next_run_id(&self) -> Result<RunId>;

    /// List all run IDs in the repository
    fn list_run_ids(&self) -> Result<Vec<RunId>>;

    /// Get the number of test runs in the repository
    fn count(&self) -> Result<usize>;

    /// Check whether a test run is currently in progress.
    fn is_run_in_progress(&self, _run_id: &RunId) -> Result<bool> {
        Ok(false)
    }

    /// List all currently in-progress run IDs.
    fn get_running_run_ids(&self) -> Result<Vec<RunId>> {
        Ok(vec![])
    }

    /// Set metadata for a test run (git commit, command, concurrency, etc.)
    ///
    /// The default implementation is a no-op for backends that don't support
    /// extended metadata.
    fn set_run_metadata(&mut self, _run_id: &RunId, _metadata: RunMetadata) -> Result<()> {
        Ok(())
    }

    /// Get metadata for a test run.
    ///
    /// The default implementation returns empty metadata for backends that
    /// don't support extended metadata.
    fn get_run_metadata(&self, _run_id: &RunId) -> Result<RunMetadata> {
        Ok(RunMetadata::default())
    }

    /// Return the wall-clock timestamp when a run was started, if known.
    ///
    /// Distinct from [`TestRun::timestamp`], which is only populated when a
    /// run is constructed in memory and is `Utc::now()` for runs read back
    /// from disk. Backends that record a start time should override this so
    /// callers can compute elapsed time for in-progress runs.
    fn get_run_started_at(&self, _run_id: &RunId) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        Ok(None)
    }

    /// Persist the captured stderr output of a test run.
    ///
    /// Backends that don't support stderr capture may discard the bytes.
    fn set_run_stderr(&mut self, _run_id: &RunId, _stderr: &[u8]) -> Result<()> {
        Ok(())
    }

    /// Read back the stderr output captured for a test run, if any.
    ///
    /// Returns `Ok(None)` when no stderr was captured (or when the backend
    /// doesn't store stderr at all).
    fn get_run_stderr(&self, _run_id: &RunId) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Delete the given test runs from the repository.
    ///
    /// Removes the runs' on-disk data (subunit streams, captured stderr) and
    /// any associated metadata. In-progress runs are skipped — callers that
    /// need to drop them must cancel them first.
    ///
    /// Returns the run IDs that were actually pruned. Run IDs that did not
    /// exist or were skipped because they were in progress are silently
    /// dropped from the result.
    fn prune_runs(&mut self, run_ids: &[RunId]) -> Result<Vec<RunId>>;

    /// Compute per-test flakiness statistics across all recorded runs.
    ///
    /// Tests are considered in the order returned by [`Self::list_run_ids`]
    /// (chronological for both built-in backends). Tests that ran in fewer
    /// than `min_runs` recorded runs are filtered out — without enough
    /// history, transition counts aren't meaningful.
    ///
    /// The default implementation walks every run via [`Self::get_test_run`],
    /// which is `O(runs × tests)` in I/O. Backends that store per-run results
    /// in a structured store should override this with a single query.
    ///
    /// Returns tests sorted by `transitions` (desc), then `failure_rate`
    /// (desc), then `test_id` (asc) for stable output.
    fn get_flakiness(&self, min_runs: usize) -> Result<Vec<TestFlakiness>> {
        let run_ids = self.list_run_ids()?;
        let mut history: HashMap<TestId, Vec<bool>> = HashMap::new();
        for run_id in &run_ids {
            let run = match self.get_test_run(run_id) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("skipping run {} while computing flakiness: {}", run_id, e);
                    continue;
                }
            };
            for (test_id, result) in &run.results {
                history
                    .entry(test_id.clone())
                    .or_default()
                    .push(result.status.is_failure());
            }
        }
        Ok(summarise_flakiness(history, min_runs))
    }
}

/// Convert a per-test sequence of pass/fail booleans into [`TestFlakiness`]
/// entries. Shared between the default trait fallback and backend-specific
/// implementations so the metric definition stays in one place.
pub fn summarise_flakiness(
    history: HashMap<TestId, Vec<bool>>,
    min_runs: usize,
) -> Vec<TestFlakiness> {
    let mut out: Vec<TestFlakiness> = history
        .into_iter()
        .filter_map(|(test_id, statuses)| {
            let runs = statuses.len() as u32;
            if (runs as usize) < min_runs {
                return None;
            }
            let failures = statuses.iter().filter(|&&f| f).count() as u32;
            // Tests that have never failed aren't flaky by any definition —
            // exclude them so the report focuses on tests that actually flap.
            if failures == 0 {
                return None;
            }
            let transitions = statuses.windows(2).filter(|w| w[0] != w[1]).count() as u32;
            let denom = runs.saturating_sub(1).max(1) as f64;
            let flakiness_score = transitions as f64 / denom;
            let failure_rate = if runs == 0 {
                0.0
            } else {
                failures as f64 / runs as f64
            };
            Some(TestFlakiness {
                test_id,
                runs,
                failures,
                transitions,
                flakiness_score,
                failure_rate,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.transitions
            .cmp(&a.transitions)
            .then_with(|| {
                b.failure_rate
                    .partial_cmp(&a.failure_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.test_id.as_str().cmp(b.test_id.as_str()))
    });
    out
}

/// Factory trait for creating and opening repositories
pub trait RepositoryFactory {
    /// Create a new repository at the given base path
    fn initialise(&self, base: &Path) -> Result<Box<dyn Repository>>;

    /// Open an existing repository at the given base path
    fn open(&self, base: &Path) -> Result<Box<dyn Repository>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_id_creation() {
        let id = TestId::new("test.module.TestCase.test_method");
        assert_eq!(id.as_str(), "test.module.TestCase.test_method");
    }

    #[test]
    fn test_test_status_ordering() {
        // Tests that status enum can be compared
        assert_eq!(TestStatus::Success, TestStatus::Success);
        assert_ne!(TestStatus::Success, TestStatus::Failure);
    }
}
