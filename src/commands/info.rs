//! Show detailed information about a test run

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

/// Command to display detailed information about a test run,
/// including metadata like git commit, command, and timing.
pub struct InfoCommand {
    base_path: Option<String>,
    run_id: Option<String>,
}

impl InfoCommand {
    /// Creates a new info command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `run_id` - Run ID to show (None = latest, supports negative indices)
    pub fn new(base_path: Option<String>, run_id: Option<String>) -> Self {
        InfoCommand { base_path, run_id }
    }
}

impl Command for InfoCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let run_id = resolve_run_id(&*repo, self.run_id.as_deref())?;
        let test_run = repo.get_test_run(&run_id)?;
        let metadata = repo.get_run_metadata(&run_id)?;

        ui.output(&format!("Run: {}", run_id))?;
        ui.output(&format!("Timestamp: {}", test_run.timestamp))?;

        if let Some(ref commit) = metadata.git_commit {
            let dirty_suffix = match metadata.git_dirty {
                Some(true) => " (dirty)",
                Some(false) => "",
                None => "",
            };
            ui.output(&format!("Git commit: {}{}", commit, dirty_suffix))?;
        }

        if let Some(ref command) = metadata.command {
            ui.output(&format!("Command: {}", command))?;
        }

        if let Some(concurrency) = metadata.concurrency {
            ui.output(&format!("Concurrency: {}", concurrency))?;
        }

        if let Some(duration_secs) = metadata.duration_secs {
            ui.output(&format!("Duration: {:.3}s", duration_secs))?;
        }

        if let Some(exit_code) = metadata.exit_code {
            ui.output(&format!("Exit code: {}", exit_code))?;
        }

        ui.output(&format!("Total tests: {}", test_run.total_tests()))?;
        ui.output(&format!("Passed: {}", test_run.count_successes()))?;
        ui.output(&format!("Failed: {}", test_run.count_failures()))?;

        if let Some(duration) = test_run.total_duration() {
            ui.output(&format!("Total test time: {:.3}s", duration.as_secs_f64()))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "info"
    }

    fn help(&self) -> &str {
        "Show detailed information about a test run"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, RunMetadata, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn test_info_command_with_metadata() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::success("test1").with_duration(std::time::Duration::from_secs(1)),
        );
        run.add_result(TestResult::failure("test2", "failed"));
        repo.insert_test_run(run).unwrap();

        repo.set_run_metadata(
            &RunId::new("0"),
            RunMetadata {
                git_commit: Some("abc123".to_string()),
                git_dirty: Some(true),
                command: Some("cargo test".to_string()),
                concurrency: Some(4),
                duration_secs: Some(12.5),
                exit_code: Some(1),
            },
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = InfoCommand::new(Some(temp.path().to_string_lossy().to_string()), None);
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        assert_eq!(ui.output[0], "Run: 0");
        assert!(ui.output[1].starts_with("Timestamp: "));
        assert_eq!(ui.output[2], "Git commit: abc123 (dirty)");
        assert_eq!(ui.output[3], "Command: cargo test");
        assert_eq!(ui.output[4], "Concurrency: 4");
        assert_eq!(ui.output[5], "Duration: 12.500s");
        assert_eq!(ui.output[6], "Exit code: 1");
        assert_eq!(ui.output[7], "Total tests: 2");
        assert_eq!(ui.output[8], "Passed: 1");
        assert_eq!(ui.output[9], "Failed: 1");
        assert_eq!(ui.output[10], "Total test time: 1.000s");
    }

    #[test]
    fn test_info_command_clean_commit() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        repo.set_run_metadata(
            &RunId::new("0"),
            RunMetadata {
                git_commit: Some("def456".to_string()),
                git_dirty: Some(false),
                command: None,
                concurrency: None,
                duration_secs: None,
                exit_code: Some(0),
            },
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = InfoCommand::new(Some(temp.path().to_string_lossy().to_string()), None);
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        assert_eq!(ui.output[2], "Git commit: def456");
    }

    #[test]
    fn test_info_command_no_metadata() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = InfoCommand::new(Some(temp.path().to_string_lossy().to_string()), None);
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        // Should show run info and test counts but no metadata fields
        assert_eq!(ui.output[0], "Run: 0");
        assert!(ui.output[1].starts_with("Timestamp: "));
        assert_eq!(ui.output[2], "Total tests: 1");
        assert_eq!(ui.output[3], "Passed: 1");
        assert_eq!(ui.output[4], "Failed: 0");
    }
}
