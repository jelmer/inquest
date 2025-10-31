//! Repository abstraction for storing test results
//!
//! This module provides traits and implementations for storing and retrieving
//! test results. The on-disk format is compatible with the Python version.

use crate::error::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

pub mod test_run;
pub mod file;

pub use test_run::{TestRun, TestId, TestStatus, TestResult};

/// Abstract repository trait for test result storage
pub trait Repository {
    /// Get a specific test run by ID
    fn get_test_run(&self, run_id: &str) -> Result<TestRun>;

    /// Insert a test run, returning the assigned run ID
    fn insert_test_run(&mut self, run: TestRun) -> Result<String>;

    /// Get the latest test run
    fn get_latest_run(&self) -> Result<TestRun>;

    /// Get the list of currently failing tests
    fn get_failing_tests(&self) -> Result<Vec<TestId>>;

    /// Get test execution times
    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>>;

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
