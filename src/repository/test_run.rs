//! Test run data structures

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Unique identifier for a test
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestId(String);

impl TestId {
    pub fn new(id: impl Into<String>) -> Self {
        TestId(id.into())
    }

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

/// Status of a test
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Success,
    Failure,
    Error,
    Skip,
    ExpectedFailure,
    UnexpectedSuccess,
}

impl TestStatus {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess
        )
    }

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

/// Result of a single test
#[derive(Debug, Clone)]
pub struct TestResult {
    pub test_id: TestId,
    pub status: TestStatus,
    pub duration: Option<Duration>,
    pub message: Option<String>,
    pub details: Option<String>,
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

/// A complete test run containing results for multiple tests
#[derive(Debug, Clone)]
pub struct TestRun {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub results: HashMap<TestId, TestResult>,
    pub tags: Vec<String>,
}

impl TestRun {
    pub fn new(id: String) -> Self {
        TestRun {
            id,
            timestamp: Utc::now(),
            results: HashMap::new(),
            tags: Vec::new(),
        }
    }

    pub fn add_result(&mut self, result: TestResult) {
        self.results.insert(result.test_id.clone(), result);
    }

    pub fn count_failures(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status.is_failure())
            .count()
    }

    pub fn count_successes(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status.is_success())
            .count()
    }

    pub fn total_tests(&self) -> usize {
        self.results.len()
    }

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
}
