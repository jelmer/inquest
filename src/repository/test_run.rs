//! Test run data structures

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Unique identifier for a test
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

/// Status of a test execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
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

/// A complete test run containing results for multiple tests.
///
/// Represents a single execution of a test suite with all test results,
/// timing information, and metadata.
#[derive(Debug, Clone)]
pub struct TestRun {
    /// Unique identifier for this test run.
    pub id: String,
    /// When this test run was executed.
    pub timestamp: DateTime<Utc>,
    /// Map of test IDs to their results.
    pub results: HashMap<TestId, TestResult>,
    /// Tags associated with this test run.
    pub tags: Vec<String>,
}

impl TestRun {
    /// Creates a new test run with the given ID and current timestamp.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for this test run
    pub fn new(id: String) -> Self {
        TestRun {
            id,
            timestamp: Utc::now(),
            results: HashMap::new(),
            tags: Vec::new(),
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
        let durations: Vec<Duration> = self.results.values().filter_map(|r| r.duration).collect();

        if durations.is_empty() {
            None
        } else {
            Some(durations.into_iter().sum())
        }
    }

    /// Check if a result matches the given tag filter
    fn matches_filter(result: &TestResult, filter_tags: &[String]) -> bool {
        if filter_tags.is_empty() {
            return true;
        }
        // Result matches if it has any of the filter tags
        result.tags.iter().any(|tag| filter_tags.contains(tag))
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
        let mut run = TestRun::new("0".to_string());

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
        let mut run = TestRun::new("0".to_string());
        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::success("test2"));

        assert_eq!(run.total_duration(), None);
    }

    #[test]
    fn test_total_duration_with_timing() {
        let mut run = TestRun::new("0".to_string());
        run.add_result(TestResult::success("test1").with_duration(Duration::from_millis(100)));
        run.add_result(TestResult::success("test2").with_duration(Duration::from_millis(200)));
        run.add_result(TestResult::success("test3").with_duration(Duration::from_millis(300)));

        assert_eq!(run.total_duration(), Some(Duration::from_millis(600)));
    }

    #[test]
    fn test_total_duration_partial_timing() {
        let mut run = TestRun::new("0".to_string());
        run.add_result(TestResult::success("test1").with_duration(Duration::from_millis(100)));
        run.add_result(TestResult::success("test2")); // No duration

        // Should sum only tests with duration
        assert_eq!(run.total_duration(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn test_filtered_counts_empty_filter() {
        let mut run = TestRun::new("0".to_string());

        run.add_result(TestResult::success("test1").with_tag("worker-0"));
        run.add_result(TestResult::failure("test2", "Failed").with_tag("worker-1"));

        // Empty filter should match all results
        assert_eq!(run.total_tests_filtered(&[]), 2);
        assert_eq!(run.count_successes_filtered(&[]), 1);
        assert_eq!(run.count_failures_filtered(&[]), 1);
    }

    #[test]
    fn test_filtered_counts_with_tags() {
        let mut run = TestRun::new("0".to_string());

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
        let mut run = TestRun::new("0".to_string());

        run.add_result(TestResult::success("test1").with_tag("worker-0"));

        // Filter by non-existent tag
        let filter = vec!["worker-99".to_string()];
        assert_eq!(run.total_tests_filtered(&filter), 0);
        assert_eq!(run.count_successes_filtered(&filter), 0);
        assert_eq!(run.count_failures_filtered(&filter), 0);
    }

    #[test]
    fn test_filtered_counts_multiple_tags() {
        let mut run = TestRun::new("0".to_string());

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
}
