//! Test run data structures

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Unique identifier for a test
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct TestId(String);

impl TestId {
    /// Creates a new test identifier from a string.
    ///
    /// # Arguments
    /// * `id` - The test identifier string
    pub fn new(id: impl Into<String>) -> Self {
        TestId(id.into())
    }

    /// Returns the test identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for TestId {
    fn from(s: String) -> Self {
        TestId(s)
    }
}

impl From<&str> for TestId {
    fn from(s: &str) -> Self {
        TestId(s.to_string())
    }
}

impl AsRef<str> for TestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for TestId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Unique identifier for a test run.
///
/// Run IDs are opaque strings assigned by the repository. They cannot be
/// constructed outside the crate — use repository methods like
/// `begin_test_run_raw()` or `get_next_run_id()` to obtain them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct RunId(String);

impl RunId {
    /// Create a new run ID.
    pub fn new(id: impl Into<String>) -> Self {
        RunId(id.into())
    }

    /// Create a sub-run ID for a worker or isolated test (e.g. "0-1").
    pub(crate) fn sub_run(&self, suffix: impl fmt::Display) -> Self {
        RunId(format!("{}-{}", self.0, suffix))
    }

    /// Returns the run ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Status of a test execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TestStatus {
    /// Test passed successfully.
    Success,
    /// Test failed with an assertion or expectation error.
    Failure,
    /// Test encountered an unexpected error during execution.
    Error,
    /// Test was skipped or disabled.
    Skip,
    /// Test failed as expected (marked as expected to fail).
    ExpectedFailure,
    /// Test passed but was marked as expected to fail.
    UnexpectedSuccess,
}

impl TestStatus {
    /// Returns true if this status represents a failure condition.
    ///
    /// Failures include: Failure, Error, and UnexpectedSuccess.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess
        )
    }

    /// Returns true if this status represents a success condition.
    ///
    /// Successes include: Success, Skip, and ExpectedFailure.
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            TestStatus::Success | TestStatus::Skip | TestStatus::ExpectedFailure
        )
    }

    /// Parse a list of status filter strings into a set of `TestStatus` values.
    ///
    /// Accepts individual status names (`success`, `failure`, `error`, `skip`, `xfail`,
    /// `uxsuccess`) and group aliases (`failing` = failure+error+uxsuccess,
    /// `passing` = success+skip+xfail). Returns `Err` with an `Other` variant on
    /// any unknown token.
    pub fn parse_filters(filters: &[String]) -> crate::error::Result<Vec<TestStatus>> {
        let mut statuses = Vec::new();
        for f in filters {
            match f.to_lowercase().as_str() {
                "failing" => {
                    statuses.extend([
                        TestStatus::Failure,
                        TestStatus::Error,
                        TestStatus::UnexpectedSuccess,
                    ]);
                }
                "passing" => {
                    statuses.extend([
                        TestStatus::Success,
                        TestStatus::Skip,
                        TestStatus::ExpectedFailure,
                    ]);
                }
                "success" => statuses.push(TestStatus::Success),
                "failure" => statuses.push(TestStatus::Failure),
                "error" => statuses.push(TestStatus::Error),
                "skip" => statuses.push(TestStatus::Skip),
                "xfail" => statuses.push(TestStatus::ExpectedFailure),
                "uxsuccess" => statuses.push(TestStatus::UnexpectedSuccess),
                other => {
                    return Err(crate::error::Error::Other(format!(
                        "Unknown status filter: '{}'. Valid values: success, failure, error, skip, xfail, uxsuccess, failing, passing",
                        other
                    )));
                }
            }
        }
        Ok(statuses)
    }
}

impl fmt::Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestStatus::Success => write!(f, "success"),
            TestStatus::Failure => write!(f, "failure"),
            TestStatus::Error => write!(f, "error"),
            TestStatus::Skip => write!(f, "skip"),
            TestStatus::ExpectedFailure => write!(f, "xfail"),
            TestStatus::UnexpectedSuccess => write!(f, "uxsuccess"),
        }
    }
}

/// Result of a single test execution.
///
/// Contains all information about a test's outcome including status,
/// timing, error messages, and associated metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestResult {
    /// Unique identifier for the test.
    pub test_id: TestId,
    /// Execution status (success, failure, error, etc.).
    pub status: TestStatus,
    /// Time taken to execute the test, if available.
    pub duration: Option<Duration>,
    /// Brief message describing the result (e.g., error message).
    pub message: Option<String>,
    /// Detailed output or traceback from the test.
    pub details: Option<String>,
    /// Tags or metadata associated with this test result.
    pub tags: Vec<String>,
}

impl TestResult {
    /// Create a successful test result
    pub fn success(test_id: impl Into<TestId>) -> Self {
        TestResult {
            test_id: test_id.into(),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        }
    }

    /// Create a failed test result
    pub fn failure(test_id: impl Into<TestId>, message: impl Into<String>) -> Self {
        TestResult {
            test_id: test_id.into(),
            status: TestStatus::Failure,
            message: Some(message.into()),
            duration: None,
            details: None,
            tags: vec![],
        }
    }

    /// Create a skipped test result
    pub fn skip(test_id: impl Into<TestId>) -> Self {
        TestResult {
            test_id: test_id.into(),
            status: TestStatus::Skip,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        }
    }

    /// Create an error test result
    pub fn error(test_id: impl Into<TestId>, message: impl Into<String>) -> Self {
        TestResult {
            test_id: test_id.into(),
            status: TestStatus::Error,
            message: Some(message.into()),
            duration: None,
            details: None,
            tags: vec![],
        }
    }

    /// Set the duration
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Set the details
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    /// Add a tag
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }
}

/// Reason why a subunit stream was interrupted before completion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum StreamInterruption {
    /// Too many consecutive parse errors in the stream.
    ParseErrors(usize),
    /// Too many consecutive unknown/corrupted items in the stream.
    UnknownItems(usize),
}

impl fmt::Display for StreamInterruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamInterruption::ParseErrors(n) => {
                write!(f, "{} consecutive parse errors", n)
            }
            StreamInterruption::UnknownItems(n) => {
                write!(f, "{} consecutive unknown items", n)
            }
        }
    }
}

/// Per-test flakiness statistics aggregated across the run history.
///
/// "Flakiness" here means a test that produces inconsistent results without
/// the code under it changing — pass↔fail flips, not chronic failures. The
/// `transitions` field counts how many times the status flipped between
/// pass and fail in consecutive runs in which the test was recorded; a
/// chronically broken test has 0 transitions, while a flapping one has many.
/// `flakiness_score` normalises that to `[0, 1]` so it's comparable across
/// tests with different amounts of history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestFlakiness {
    /// Test identifier.
    pub test_id: TestId,
    /// Number of recorded runs in which this test ran (any status).
    pub runs: u32,
    /// Number of those runs in which the test failed (failure/error/uxsuccess).
    pub failures: u32,
    /// Number of pass↔fail transitions across consecutive runs in which the
    /// test was recorded. The marker for true flakiness.
    pub transitions: u32,
    /// `transitions / max(1, runs - 1)` — the share of consecutive run pairs
    /// where the status flipped, in `[0, 1]`. Higher means more unstable.
    pub flakiness_score: f64,
    /// `failures / runs` — the share of runs where the test failed, in
    /// `[0, 1]`. High failure rate with low transitions means "broken",
    /// not "flaky".
    pub failure_rate: f64,
}

/// Metadata about a test run's execution context.
///
/// Captures information about the environment and configuration
/// used when executing the test run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunMetadata {
    /// Git commit hash at the time of the run.
    pub git_commit: Option<String>,
    /// Whether the git working tree had uncommitted changes.
    pub git_dirty: Option<bool>,
    /// The test command that was executed.
    pub command: Option<String>,
    /// Number of parallel workers used.
    pub concurrency: Option<u32>,
    /// Wall-clock duration of the run in seconds.
    pub duration_secs: Option<f64>,
    /// Exit code of the test command.
    pub exit_code: Option<i32>,
    /// Extra arguments passed to the test command after `--`. Captured so
    /// `inq rerun` can reproduce the original invocation.
    pub test_args: Option<Vec<String>>,
}

/// A complete test run containing results for multiple tests.
///
/// Represents a single execution of a test suite with all test results,
/// timing information, and metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestRun {
    /// Unique identifier for this test run.
    pub id: RunId,
    /// When this test run was executed.
    pub timestamp: DateTime<Utc>,
    /// Map of test IDs to their results.
    pub results: HashMap<TestId, TestResult>,
    /// Tags associated with this test run.
    pub tags: Vec<String>,
    /// If the subunit stream was interrupted, describes why/how.
    pub interruption: Option<StreamInterruption>,
}

impl TestRun {
    /// Creates a new test run with the given ID and current timestamp.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for this test run
    pub fn new(id: RunId) -> Self {
        TestRun {
            id,
            timestamp: Utc::now(),
            results: HashMap::new(),
            tags: Vec::new(),
            interruption: None,
        }
    }

    /// Adds a test result to this run, replacing any existing result for the same test.
    ///
    /// # Arguments
    /// * `result` - The test result to add
    pub fn add_result(&mut self, result: TestResult) {
        self.results.insert(result.test_id.clone(), result);
    }

    /// Returns the number of failed tests in this run.
    pub fn count_failures(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status.is_failure())
            .count()
    }

    /// Returns the number of successful tests in this run.
    pub fn count_successes(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status.is_success())
            .count()
    }

    /// Returns the total number of tests in this run.
    pub fn total_tests(&self) -> usize {
        self.results.len()
    }

    /// Calculate total duration of all tests with timing information
    pub fn total_duration(&self) -> Option<Duration> {
        let mut durations = self.results.values().filter_map(|r| r.duration);
        // Get the first duration, then fold the rest
        durations
            .next()
            .map(|first| durations.fold(first, |acc, d| acc + d))
    }

    /// Check if a result matches the given tag filter.
    ///
    /// Each entry in `filter_tags` is either a positive tag (the result must
    /// carry one of them) or an exclusion (`!tag`, the result must not carry
    /// that tag). When no positive entries are supplied, any result that
    /// avoids all exclusions matches.
    fn matches_filter(result: &TestResult, filter_tags: &[String]) -> bool {
        if filter_tags.is_empty() {
            return true;
        }

        let (excludes, includes): (Vec<&str>, Vec<&str>) = filter_tags
            .iter()
            .map(|t| t.as_str())
            .partition(|t| t.starts_with('!'));
        let excludes: Vec<&str> = excludes.iter().map(|t| &t[1..]).collect();

        if result
            .tags
            .iter()
            .any(|tag| excludes.contains(&tag.as_str()))
        {
            return false;
        }

        if includes.is_empty() {
            return true;
        }

        result
            .tags
            .iter()
            .any(|tag| includes.contains(&tag.as_str()))
    }

    /// Count failures matching the given tags
    pub fn count_failures_filtered(&self, filter_tags: &[String]) -> usize {
        self.results
            .values()
            .filter(|r| Self::matches_filter(r, filter_tags) && r.status.is_failure())
            .count()
    }

    /// Count successes matching the given tags
    pub fn count_successes_filtered(&self, filter_tags: &[String]) -> usize {
        self.results
            .values()
            .filter(|r| Self::matches_filter(r, filter_tags) && r.status.is_success())
            .count()
    }

    /// Count total tests matching the given tags
    pub fn total_tests_filtered(&self, filter_tags: &[String]) -> usize {
        self.results
            .values()
            .filter(|r| Self::matches_filter(r, filter_tags))
            .count()
    }

    /// Returns a list of test IDs for all tests that failed in this run.
    pub fn get_failing_tests(&self) -> Vec<&TestId> {
        self.results
            .values()
            .filter(|r| r.status.is_failure())
            .map(|r| &r.test_id)
            .collect()
    }
}

/// Estimate progress for an in-progress test run using historical timings.
///
/// Returns `(total_expected, percent_complete, estimated_remaining_secs)`
/// where `percent_complete` is in `[0, 1]`. All three are `None` when no
/// historical data is available (first run in a new repo).
///
/// `estimated_remaining_secs` projects wall-clock time remaining by scaling
/// the observed pace: if the tests completed so far account for fraction `f`
/// of the historical total test-time, and `elapsed` wall-clock seconds have
/// passed, the run is projected to take `elapsed / f` total wall-clock
/// seconds. This automatically captures parallelism, machine speed, and
/// per-run overhead — the same algorithm `inq run` uses for its live ETA.
///
/// When `elapsed` is `None` or zero, falls back to the sum of historical
/// durations of tests not yet observed (test-time, not wall-time), which is
/// only meaningful for serial runs but is better than nothing for callers
/// that don't know elapsed time.
pub fn estimate_progress(
    historical: &HashMap<TestId, Duration>,
    test_run: &TestRun,
    elapsed: Option<Duration>,
) -> (Option<usize>, Option<f64>, Option<f64>) {
    if historical.is_empty() {
        return (None, None, None);
    }
    let total_expected = historical.len();
    let observed = test_run.total_tests();
    let percent = if total_expected > 0 {
        Some((observed as f64 / total_expected as f64).min(1.0))
    } else {
        None
    };

    // Sum historical durations of tests already completed in this run, and
    // of tests still outstanding. We use historical durations on both sides
    // so the ratio is meaningful even when the current run hasn't recorded
    // its own per-test timings yet.
    let mut completed_test_time = Duration::ZERO;
    let mut total_test_time = Duration::ZERO;
    for (id, dur) in historical {
        total_test_time += *dur;
        if test_run.results.contains_key(id) {
            completed_test_time += *dur;
        }
    }

    let remaining_secs = match elapsed {
        Some(elapsed) if !elapsed.is_zero() && !total_test_time.is_zero() => {
            let fraction_done = completed_test_time.as_secs_f64() / total_test_time.as_secs_f64();
            if fraction_done > 0.0 && fraction_done < 1.0 {
                let projected_total = elapsed.as_secs_f64() / fraction_done;
                Some((projected_total - elapsed.as_secs_f64()).max(0.0))
            } else if fraction_done >= 1.0 {
                Some(0.0)
            } else {
                // No completed tests yet — can't project from observed pace.
                Some(total_test_time.as_secs_f64())
            }
        }
        _ => {
            // No elapsed time available: fall back to summing historical
            // durations of unobserved tests. Pessimistic for parallel runs.
            let remaining: Duration = historical
                .iter()
                .filter(|(id, _)| !test_run.results.contains_key(*id))
                .map(|(_, d)| *d)
                .sum();
            Some(remaining.as_secs_f64())
        }
    };

    (Some(total_expected), percent, remaining_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_id_equality() {
        let id1 = TestId::new("test1");
        let id2 = TestId::new("test1");
        let id3 = TestId::new("test2");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_test_status_is_failure() {
        assert!(TestStatus::Failure.is_failure());
        assert!(TestStatus::Error.is_failure());
        assert!(TestStatus::UnexpectedSuccess.is_failure());
        assert!(!TestStatus::Success.is_failure());
        assert!(!TestStatus::Skip.is_failure());
    }

    #[test]
    fn test_test_status_is_success() {
        assert!(TestStatus::Success.is_success());
        assert!(TestStatus::Skip.is_success());
        assert!(TestStatus::ExpectedFailure.is_success());
        assert!(!TestStatus::Failure.is_success());
        assert!(!TestStatus::Error.is_success());
    }

    #[test]
    fn test_test_run_counts() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });

        run.add_result(TestResult {
            test_id: TestId::new("test2"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("Failed".to_string()),
            details: None,
            tags: vec![],
        });

        run.add_result(TestResult {
            test_id: TestId::new("test3"),
            status: TestStatus::Skip,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });

        assert_eq!(run.total_tests(), 3);
        assert_eq!(run.count_successes(), 2); // Success + Skip
        assert_eq!(run.count_failures(), 1);
        assert_eq!(run.get_failing_tests().len(), 1);
    }

    #[test]
    fn test_test_status_display() {
        assert_eq!(TestStatus::Success.to_string(), "success");
        assert_eq!(TestStatus::Failure.to_string(), "failure");
        assert_eq!(TestStatus::Error.to_string(), "error");
        assert_eq!(TestStatus::Skip.to_string(), "skip");
        assert_eq!(TestStatus::ExpectedFailure.to_string(), "xfail");
        assert_eq!(TestStatus::UnexpectedSuccess.to_string(), "uxsuccess");
    }

    #[test]
    fn test_result_success_constructor() {
        let result = TestResult::success("test1");
        assert_eq!(result.test_id.as_str(), "test1");
        assert_eq!(result.status, TestStatus::Success);
        assert!(result.message.is_none());
        assert!(result.duration.is_none());
    }

    #[test]
    fn test_result_failure_constructor() {
        let result = TestResult::failure("test1", "Failed!");
        assert_eq!(result.test_id.as_str(), "test1");
        assert_eq!(result.status, TestStatus::Failure);
        assert_eq!(result.message, Some("Failed!".to_string()));
    }

    #[test]
    fn test_result_with_duration() {
        let result = TestResult::success("test1").with_duration(Duration::from_millis(100));
        assert_eq!(result.duration, Some(Duration::from_millis(100)));
    }

    #[test]
    fn test_result_with_details() {
        let result = TestResult::failure("test1", "Failed").with_details("Stack trace here");
        assert_eq!(result.details, Some("Stack trace here".to_string()));
    }

    #[test]
    fn test_result_with_tag() {
        let result = TestResult::success("test1").with_tag("slow");
        assert_eq!(result.tags, vec!["slow"]);
    }

    #[test]
    fn test_total_duration_no_timing() {
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::success("test2"));

        assert_eq!(run.total_duration(), None);
    }

    #[test]
    fn test_total_duration_with_timing() {
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test1").with_duration(Duration::from_millis(100)));
        run.add_result(TestResult::success("test2").with_duration(Duration::from_millis(200)));
        run.add_result(TestResult::success("test3").with_duration(Duration::from_millis(300)));

        assert_eq!(run.total_duration(), Some(Duration::from_millis(600)));
    }

    #[test]
    fn test_total_duration_partial_timing() {
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test1").with_duration(Duration::from_millis(100)));
        run.add_result(TestResult::success("test2")); // No duration

        // Should sum only tests with duration
        assert_eq!(run.total_duration(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn test_filtered_counts_empty_filter() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult::success("test1").with_tag("worker-0"));
        run.add_result(TestResult::failure("test2", "Failed").with_tag("worker-1"));

        // Empty filter should match all results
        assert_eq!(run.total_tests_filtered(&[]), 2);
        assert_eq!(run.count_successes_filtered(&[]), 1);
        assert_eq!(run.count_failures_filtered(&[]), 1);
    }

    #[test]
    fn test_filtered_counts_with_tags() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult::success("test1").with_tag("worker-0"));
        run.add_result(TestResult::failure("test2", "Failed").with_tag("worker-0"));
        run.add_result(TestResult::success("test3").with_tag("worker-1"));
        run.add_result(TestResult::failure("test4", "Failed").with_tag("worker-1"));

        // Filter by worker-0
        let filter = vec!["worker-0".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 2);
        assert_eq!(run.count_successes_filtered(&filter), 1);
        assert_eq!(run.count_failures_filtered(&filter), 1);

        // Filter by worker-1
        let filter = vec!["worker-1".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 2);
        assert_eq!(run.count_successes_filtered(&filter), 1);
        assert_eq!(run.count_failures_filtered(&filter), 1);
    }

    #[test]
    fn test_filtered_counts_no_match() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult::success("test1").with_tag("worker-0"));

        // Filter by non-existent tag
        let filter = vec!["worker-99".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 0);
        assert_eq!(run.count_successes_filtered(&filter), 0);
        assert_eq!(run.count_failures_filtered(&filter), 0);
    }

    #[test]
    fn test_filtered_counts_multiple_tags() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(
            TestResult::success("test1")
                .with_tag("worker-0")
                .with_tag("slow"),
        );
        run.add_result(TestResult::success("test2").with_tag("worker-1"));

        // Filter should match if result has ANY of the filter tags
        let filter = vec!["slow".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 1);
    }

    #[test]
    fn test_filtered_counts_exclude_tag() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult::success("test1").with_tag("slow"));
        run.add_result(TestResult::success("test2").with_tag("fast"));
        run.add_result(TestResult::failure("test3", "Failed").with_tag("slow"));

        let filter = vec!["!slow".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 1);
        assert_eq!(run.count_successes_filtered(&filter), 1);
        assert_eq!(run.count_failures_filtered(&filter), 0);
    }

    #[test]
    fn test_filtered_counts_include_and_exclude() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(
            TestResult::success("test1")
                .with_tag("worker-0")
                .with_tag("slow"),
        );
        run.add_result(TestResult::success("test2").with_tag("worker-0"));
        run.add_result(TestResult::success("test3").with_tag("worker-1"));

        let filter = vec!["worker-0".to_string(), "!slow".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 1);
    }

    #[test]
    fn test_filtered_counts_exclude_only_untagged_passes() {
        let mut run = TestRun::new(RunId::new("0"));

        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::success("test2").with_tag("slow"));

        let filter = vec!["!slow".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 1);
    }

    fn historical_2s(n: usize) -> HashMap<TestId, Duration> {
        (0..n)
            .map(|i| (TestId::new(format!("test_{i}")), Duration::from_secs(2)))
            .collect()
    }

    #[test]
    fn test_estimate_progress_no_history() {
        let run = TestRun::new(RunId::new("0"));
        let (total, pct, eta) =
            estimate_progress(&HashMap::new(), &run, Some(Duration::from_secs(10)));
        assert_eq!(total, None);
        assert_eq!(pct, None);
        assert_eq!(eta, None);
    }

    #[test]
    fn test_estimate_progress_projects_from_observed_pace() {
        // 4 tests in history, each 2s historical. We've completed 1 in 5
        // wall-clock seconds. Fraction done by historical test-time = 1/4 =
        // 0.25. Projected total wall = 5 / 0.25 = 20s. Remaining = 15s.
        let historical = historical_2s(4);
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test_0"));

        let (total, pct, eta) = estimate_progress(&historical, &run, Some(Duration::from_secs(5)));
        assert_eq!(total, Some(4));
        assert!((pct.unwrap() - 0.25).abs() < 1e-9);
        assert!((eta.unwrap() - 15.0).abs() < 1e-6, "got {:?}", eta);
    }

    #[test]
    fn test_estimate_progress_parallel_speedup_visible_in_eta() {
        // 4 tests × 2s = 8s historical test-time. If 2 tests completed in
        // only 1s wall-clock (parallel run), fraction done by test-time =
        // 4/8 = 0.5. Projected total wall = 1 / 0.5 = 2s. Remaining = 1s.
        // The naive sum-of-unobserved approach would say 4s remaining.
        let historical = historical_2s(4);
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test_0"));
        run.add_result(TestResult::success("test_1"));

        let (_, _, eta) = estimate_progress(&historical, &run, Some(Duration::from_secs(1)));
        assert!((eta.unwrap() - 1.0).abs() < 1e-6, "got {:?}", eta);
    }

    #[test]
    fn test_estimate_progress_no_elapsed_falls_back() {
        // Without elapsed time, fall back to summing unobserved historical
        // durations: 3 unobserved × 2s = 6s.
        let historical = historical_2s(4);
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test_0"));

        let (_, _, eta) = estimate_progress(&historical, &run, None);
        assert!((eta.unwrap() - 6.0).abs() < 1e-6, "got {:?}", eta);
    }

    #[test]
    fn test_estimate_progress_zero_elapsed_falls_back() {
        // Zero elapsed should also use the fallback rather than divide by
        // zero or return a meaningless projection.
        let historical = historical_2s(4);
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::success("test_0"));

        let (_, _, eta) = estimate_progress(&historical, &run, Some(Duration::ZERO));
        assert!((eta.unwrap() - 6.0).abs() < 1e-6, "got {:?}", eta);
    }

    #[test]
    fn test_estimate_progress_no_completed_tests_yet() {
        // With elapsed time but no tests completed, we can't project from
        // observed pace. Return the total historical test-time as a rough
        // upper-bound estimate.
        let historical = historical_2s(4);
        let run = TestRun::new(RunId::new("0"));

        let (_, pct, eta) = estimate_progress(&historical, &run, Some(Duration::from_secs(3)));
        assert_eq!(pct, Some(0.0));
        assert!((eta.unwrap() - 8.0).abs() < 1e-6, "got {:?}", eta);
    }
}
