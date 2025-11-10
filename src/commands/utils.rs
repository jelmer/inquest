//! Utility functions for command implementation

use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, TestRun};
use crate::ui::UI;
use std::path::Path;

/// Open a repository at the given path (or current directory if None)
pub fn open_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    let factory = FileRepositoryFactory;
    factory.open(base)
}

/// Initialize a repository at the given path (or current directory if None)
pub fn init_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    let factory = FileRepositoryFactory;
    factory.initialise(base)
}

/// Display detailed test results to the UI
///
/// This shows test results with details/tracebacks based on the filter:
/// - all_results: true = show all test results (passing and failing)
/// - all_results: false = show only failed/unexpected success test results
pub fn display_test_results(ui: &mut dyn UI, test_run: &TestRun, all_results: bool) -> Result<()> {
    if all_results {
        // Show all test results (both passing and failing)
        if test_run.total_tests() > 0 {
            ui.output("")?;
            ui.output("Test results:")?;
            for (test_id, result) in &test_run.results {
                ui.output("")?;
                let status_str = match result.status {
                    crate::repository::TestStatus::Success => "PASSED",
                    crate::repository::TestStatus::Failure => "FAILED",
                    crate::repository::TestStatus::Skip => "SKIPPED",
                    crate::repository::TestStatus::ExpectedFailure => "XFAIL",
                    crate::repository::TestStatus::UnexpectedSuccess => "XPASS",
                    crate::repository::TestStatus::Error => "ERROR",
                };
                ui.output(&format!("{}: {}", status_str, test_id))?;

                if let Some(ref details) = result.details {
                    for line in details.lines() {
                        ui.output(&format!("  {}", line))?;
                    }
                } else if let Some(ref message) = result.message {
                    ui.output(&format!("  {}", message))?;
                }
            }
        }
    } else {
        // Show only failed test results
        let failing_tests = test_run.get_failing_tests();
        if !failing_tests.is_empty() {
            ui.output("")?;
            ui.output("Failed tests:")?;
            for test_id in failing_tests {
                ui.output("")?;
                ui.output(&format!("{}:", test_id))?;

                if let Some(result) = test_run.results.get(test_id) {
                    if let Some(ref details) = result.details {
                        for line in details.lines() {
                            ui.output(&format!("  {}", line))?;
                        }
                    } else if let Some(ref message) = result.message {
                        ui.output(&format!("  {}", message))?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_repository() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let result = init_repository(Some(&path));
        assert!(result.is_ok());
    }

    #[test]
    fn test_open_repository_nonexistent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let result = open_repository(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn test_open_repository_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        init_repository(Some(&path)).unwrap();
        let result = open_repository(Some(&path));
        assert!(result.is_ok());
    }
}
