//! Show currently failing tests

use crate::commands::Command;
use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::ui::UI;
use std::path::Path;

pub struct FailingCommand {
    base_path: Option<String>,
    list_only: bool,
}

impl FailingCommand {
    pub fn new(base_path: Option<String>) -> Self {
        FailingCommand {
            base_path,
            list_only: false,
        }
    }

    pub fn with_list_only(base_path: Option<String>) -> Self {
        FailingCommand {
            base_path,
            list_only: true,
        }
    }
}

impl Command for FailingCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        let factory = FileRepositoryFactory;
        let repo = factory.open(base)?;

        // Get failing tests from the latest run
        let test_run = repo.get_latest_run()?;
        let failing = test_run.get_failing_tests();

        if failing.is_empty() {
            if !self.list_only {
                ui.output("No failing tests")?;
            }
            Ok(0)
        } else {
            if self.list_only {
                // List mode: just output test IDs, one per line
                for test_id in failing {
                    ui.output(test_id.as_str())?;
                }
            } else {
                // Normal mode: output with header
                ui.output(&format!("{} failing test(s):", failing.len()))?;
                for test_id in failing {
                    ui.output(&format!("  {}", test_id))?;
                }
            }
            Ok(1)
        }
    }

    fn name(&self) -> &str {
        "failing"
    }

    fn help(&self) -> &str {
        "Show tests that failed in the last run"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{TestId, TestResult, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn test_failing_command_no_failures() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = FailingCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(ui.output.iter().any(|s| s.contains("No failing tests")));
    }

    #[test]
    fn test_failing_command_with_failures() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        test_run.add_result(TestResult {
            test_id: TestId::new("test2"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("Failed".to_string()),
            details: None,
            tags: vec![],
        });
        test_run.add_result(TestResult {
            test_id: TestId::new("test3"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("Also failed".to_string()),
            details: None,
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = FailingCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        assert!(ui.output.iter().any(|s| s.contains("2 failing")));
        assert!(ui.output.iter().any(|s| s.contains("test2")));
        assert!(ui.output.iter().any(|s| s.contains("test3")));
    }

    #[test]
    fn test_failing_command_list_mode() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult::failure("test1", "Failed"));
        test_run.add_result(TestResult::failure("test2", "Also failed"));
        test_run.add_result(TestResult::success("test3"));

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = FailingCommand::with_list_only(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        // In list mode, output should be just test IDs, no header
        assert_eq!(ui.output.len(), 2);
        assert!(ui.output.contains(&"test1".to_string()));
        assert!(ui.output.contains(&"test2".to_string()));
        // No header in list mode
        assert!(!ui.output.iter().any(|s| s.contains("failing test(s):")));
    }
}
