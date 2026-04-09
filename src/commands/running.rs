//! Show currently in-progress test runs

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

fn format_duration_ago(duration: chrono::TimeDelta) -> String {
    let secs = duration.num_seconds();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Command to list currently running test runs.
pub struct RunningCommand {
    base_path: Option<String>,
}

impl RunningCommand {
    /// Creates a new running command.
    pub fn new(base_path: Option<String>) -> Self {
        RunningCommand { base_path }
    }
}

impl Command for RunningCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let run_ids = repo.get_running_run_ids()?;

        if run_ids.is_empty() {
            ui.output("No test runs currently in progress.")?;
            return Ok(0);
        }

        let now = chrono::Utc::now();
        for run_id in &run_ids {
            let test_run = repo.get_test_run(run_id)?;
            let ago = format_duration_ago(now - test_run.timestamp);
            ui.output(&format!(
                "Run {}: {} tests so far ({} passed, {} failed), started {}",
                run_id,
                test_run.total_tests(),
                test_run.count_successes(),
                test_run.count_failures(),
                ago,
            ))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "running"
    }

    fn help(&self) -> &str {
        "Show currently in-progress test runs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestRun};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn test_running_command_no_runs() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunningCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output[0], "No test runs currently in progress.");
    }

    #[test]
    fn test_running_command_completed_run_not_shown() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new(RunId::new("0"));
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunningCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output[0], "No test runs currently in progress.");
    }
}
