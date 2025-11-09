//! Show the last test run

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

pub struct LastCommand {
    base_path: Option<String>,
    subunit: bool,
    show_output: bool,
}

impl LastCommand {
    pub fn new(base_path: Option<String>) -> Self {
        LastCommand {
            base_path,
            subunit: false,
            show_output: true, // By default, show output for failed tests (matches Python behavior)
        }
    }

    pub fn with_subunit(base_path: Option<String>) -> Self {
        LastCommand {
            base_path,
            subunit: true,
            show_output: false, // Subunit mode doesn't show formatted output
        }
    }

    pub fn with_output_control(base_path: Option<String>, show_output: bool) -> Self {
        LastCommand {
            base_path,
            subunit: false,
            show_output,
        }
    }
}

impl Command for LastCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let test_run = repo.get_latest_run()?;

        if self.subunit {
            // Output the test run as a subunit stream
            let mut buffer = Vec::new();
            crate::subunit_stream::write_stream(&test_run, &mut buffer)?;
            ui.output_bytes(&buffer)?;
            return Ok(0); // Exit code 0 if we successfully wrote the stream
        }

        ui.output(&format!("Test run: {}", test_run.id))?;
        ui.output(&format!("Timestamp: {}", test_run.timestamp))?;
        ui.output(&format!("Total tests: {}", test_run.total_tests()))?;
        ui.output(&format!("Passed: {}", test_run.count_successes()))?;
        ui.output(&format!("Failed: {}", test_run.count_failures()))?;

        // Show total duration if available
        if let Some(duration) = test_run.total_duration() {
            ui.output(&format!("Total time: {:.3}s", duration.as_secs_f64()))?;
        }

        if test_run.count_failures() > 0 {
            ui.output("")?;
            ui.output("Failed tests:")?;

            if self.show_output {
                // Show detailed output for each failed test
                for test_id in test_run.get_failing_tests() {
                    ui.output("")?;
                    ui.output(&format!("{}:", test_id))?;

                    // Get the test result to show details
                    if let Some(result) = test_run.results.get(test_id) {
                        if let Some(ref details) = result.details {
                            // Show the traceback/details
                            for line in details.lines() {
                                ui.output(&format!("  {}", line))?;
                            }
                        } else if let Some(ref message) = result.message {
                            // Show just the message if no details
                            ui.output(&format!("  {}", message))?;
                        }
                    }
                }
            } else {
                // Just list the test IDs
                for test_id in test_run.get_failing_tests() {
                    ui.output(&format!("  {}", test_id))?;
                }
            }
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn name(&self) -> &str {
        "last"
    }

    fn help(&self) -> &str {
        "Show the results from the last test run"
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
    fn test_last_command() {
        let temp = TempDir::new().unwrap();

        // Initialize repository and add a test run
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

        // Execute last command
        let mut ui = TestUI::new();
        let cmd = LastCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(ui.output.iter().any(|s| s.contains("Test run: 0")));
        assert!(ui.output.iter().any(|s| s.contains("Total tests: 1")));
        assert!(ui.output.iter().any(|s| s.contains("Passed: 1")));
        assert!(ui.output.iter().any(|s| s.contains("Failed: 0")));
    }

    #[test]
    fn test_last_command_with_failures() {
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

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = LastCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1); // Non-zero exit code for failures
        assert!(ui.output.iter().any(|s| s.contains("Failed: 1")));
        assert!(ui.output.iter().any(|s| s.contains("test2")));
    }
}
