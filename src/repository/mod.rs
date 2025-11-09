//! Repository abstraction for storing test results
//!
//! This module provides traits and implementations for storing and retrieving
//! test results. The on-disk format is compatible with the Python version.

use crate::error::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

pub mod file;
pub mod test_run;

pub use test_run::{TestId, TestResult, TestRun, TestStatus};

/// Abstract repository trait for test result storage
///
/// # Examples
///
/// ```
/// use testrepository::repository::{Repository, RepositoryFactory, TestResult, TestRun};
/// use testrepository::repository::file::FileRepositoryFactory;
/// use tempfile::TempDir;
///
/// # fn main() -> testrepository::error::Result<()> {
/// // Create a temporary directory for the repository
/// let temp = TempDir::new().unwrap();
///
/// // Initialize a new repository
/// let factory = FileRepositoryFactory;
/// let mut repo = factory.initialise(temp.path())?;
///
/// // Create a test run with results
/// let mut test_run = TestRun::new("0".to_string());
/// test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
/// test_run.add_result(TestResult::success("test_example::test_passing"));
/// test_run.add_result(TestResult::failure("test_example::test_failing", "assertion failed"));
///
/// // Insert the test run
/// let run_id = repo.insert_test_run(test_run)?;
/// println!("Inserted test run with ID: {}", run_id);
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
    fn get_test_run(&self, run_id: &str) -> Result<TestRun>;

    /// Begin inserting a raw test run stream, returning (run_id, writer)
    /// This preserves the original stream byte-for-byte including non-subunit output
    /// The caller should write the raw subunit bytes to the returned writer
    fn begin_test_run_raw(&mut self) -> Result<(String, Box<dyn std::io::Write + Send>)>;

    /// Insert a test run (convenience method for tests - prefer begin_test_run_raw in production)
    ///
    /// This is a convenience wrapper around begin_test_run_raw() for test code.
    /// Production code should prefer the streaming API for better performance.
    fn insert_test_run(&mut self, run: TestRun) -> Result<String> {
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
    fn insert_test_run_partial(&mut self, run: TestRun, partial: bool) -> Result<String> {
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

    /// Get the list of currently failing tests
    fn get_failing_tests(&self) -> Result<Vec<TestId>>;

    /// Get test execution times
    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>>;

    /// Get test execution times for specific test IDs
    fn get_test_times_for_ids(&self, test_ids: &[TestId]) -> Result<HashMap<TestId, Duration>>;

    /// Update test execution times
    fn update_test_times(&mut self, times: &HashMap<TestId, Duration>) -> Result<()>;

    /// Get the next run ID that will be assigned
    fn get_next_run_id(&self) -> Result<u64>;

    /// List all run IDs in the repository
    fn list_run_ids(&self) -> Result<Vec<String>>;

    /// Get the number of test runs in the repository
    fn count(&self) -> Result<usize>;
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
