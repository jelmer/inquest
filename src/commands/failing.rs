//! Show currently failing tests

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;
use std::path::Path;

pub struct FailingCommand {
    base_path: Option<String>,
    list_only: bool,
    subunit: bool,
    show_output: bool,
}

impl FailingCommand {
    pub fn new(base_path: Option<String>) -> Self {
        FailingCommand {
            base_path,
            list_only: false,
            subunit: false,
            show_output: true, // By default, show output for failed tests
        }
    }

    pub fn with_list_only(base_path: Option<String>) -> Self {
        FailingCommand {
            base_path,
            list_only: true,
            subunit: false,
            show_output: false, // List mode doesn't show output
        }
    }

    pub fn with_subunit(base_path: Option<String>) -> Self {
        FailingCommand {
            base_path,
            list_only: false,
            subunit: true,
            show_output: false, // Subunit mode doesn't show formatted output
        }
    }

    pub fn with_output_control(base_path: Option<String>, show_output: bool) -> Self {
        FailingCommand {
            base_path,
            list_only: false,
            subunit: false,
            show_output,
        }
    }
}

impl Command for FailingCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));
        let repo = open_repository(self.base_path.as_deref())?;

        // Get failing tests from the repository's failing file
        let failing_tests = repo.get_failing_tests()?;

        if failing_tests.is_empty() {
            if !self.list_only && !self.subunit {
                ui.output("No failing tests")?;
            }
            return Ok(0);
        }

        if self.subunit {
            // Output the failing tests as a subunit stream
            // We need to reconstruct a TestRun from the failing file
            use std::fs::File;
            let failing_path = base.join(".testrepository").join("failing");
            if failing_path.exists() {
                let file = File::open(&failing_path)?;
                let test_run = crate::subunit_stream::parse_stream(file, "failing".to_string())?;
                let mut buffer = Vec::new();
                crate::subunit_stream::write_stream(&test_run, &mut buffer)?;
                ui.output_bytes(&buffer)?;
            }
            return Ok(0); // Exit code 0 if we successfully wrote the stream
        }

        if self.list_only {
            // List mode: just output test IDs, one per line
            for test_id in failing_tests {
                ui.output(test_id.as_str())?;
            }
        } else {
            // Normal mode: output with header
            ui.output(&format!("{} failing test(s):", failing_tests.len()))?;
            for test_id in failing_tests {
                ui.output(&format!("  {}", test_id))?;
            }
        }
        Ok(1)
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
    use crate::repository::file::FileRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestId, TestResult, TestRun, TestStatus};
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

        // Use insert_test_run_partial with partial=false to populate the failing file
        repo.insert_test_run_partial(test_run, false).unwrap();

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

        // Use insert_test_run_partial with partial=false to populate the failing file
        repo.insert_test_run_partial(test_run, false).unwrap();

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
