//! Run tests and load results into the repository

use crate::commands::Command;
use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::path::Path;

pub struct RunCommand {
    base_path: Option<String>,
    failing_only: bool,
}

impl RunCommand {
    pub fn new(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            failing_only: false,
        }
    }

    pub fn with_failing_only(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            failing_only: true,
        }
    }
}

impl Command for RunCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        // Open repository
        let factory = FileRepositoryFactory;
        let mut repo = factory.open(base)?;

        // Load test command configuration
        let test_cmd = TestCommand::from_directory(base)?;

        // Determine which tests to run
        let test_ids = if self.failing_only {
            let latest = repo.get_latest_run()?;
            let failing = latest.get_failing_tests();
            if failing.is_empty() {
                ui.output("No failing tests to run")?;
                return Ok(0);
            }
            Some(failing.iter().map(|id| (*id).clone()).collect::<Vec<_>>())
        } else {
            None
        };

        // Run tests
        ui.output("Running tests...")?;
        let mut child = test_cmd.run_tests(test_ids.as_deref())?;

        // Read subunit output from stdout
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| crate::error::Error::CommandExecution("No stdout".to_string()))?;

        // Get the next run ID
        let run_id = repo.get_next_run_id()?.to_string();

        // Parse the subunit stream
        let test_run = subunit_stream::parse_stream(stdout, run_id.clone())?;

        // Wait for the process to complete
        let status = child
            .wait()
            .map_err(|e| crate::error::Error::CommandExecution(format!("Failed to wait: {}", e)))?;

        // Insert into repository
        let inserted_id = repo.insert_test_run(test_run.clone())?;

        ui.output(&format!(
            "Ran {} test(s) as run {}",
            test_run.total_tests(),
            inserted_id
        ))?;

        if test_run.count_failures() > 0 {
            ui.output(&format!("{} test(s) failed", test_run.count_failures()))?;
            Ok(1)
        } else if !status.success() {
            ui.output("Test runner failed")?;
            Ok(status.code().unwrap_or(1))
        } else {
            ui.output("All tests passed")?;
            Ok(0)
        }
    }

    fn name(&self) -> &str {
        "run"
    }

    fn help(&self) -> &str {
        "Run tests and load results into the repository"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_run_command_no_config() {
        let temp = TempDir::new().unwrap();

        // Initialize repo but no .testr.conf
        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        // Should fail due to missing config
        assert!(result.is_err());
    }

    #[test]
    fn test_run_command_with_failing_only_no_failures() {
        let temp = TempDir::new().unwrap();

        // Initialize repo
        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Add a passing test run
        let mut test_run = crate::repository::TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(crate::repository::TestResult {
            test_id: crate::repository::TestId::new("test1"),
            status: crate::repository::TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        repo.insert_test_run(test_run).unwrap();

        // Need a .testr.conf file
        let config = r#"
[DEFAULT]
test_command=echo "test1"
"#;
        fs::write(temp.path().join(".testr.conf"), config).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::with_failing_only(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        // Should succeed with "No failing tests to run"
        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 1);
        assert_eq!(ui.output[0], "No failing tests to run");
    }

    #[test]
    fn test_run_command_name() {
        let cmd = RunCommand::new(None);
        assert_eq!(cmd.name(), "run");
    }
}
