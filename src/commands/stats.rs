//! Show repository statistics

use crate::commands::Command;
use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::ui::UI;
use std::path::Path;

pub struct StatsCommand {
    base_path: Option<String>,
}

impl StatsCommand {
    pub fn new(base_path: Option<String>) -> Self {
        StatsCommand { base_path }
    }
}

impl Command for StatsCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        let factory = FileRepositoryFactory;
        let repo = factory.open(base)?;

        let run_count = repo.count()?;
        let run_ids = repo.list_run_ids()?;

        ui.output("Repository Statistics:")?;
        ui.output(&format!("  Total test runs: {}", run_count))?;

        if !run_ids.is_empty() {
            let latest_run = repo.get_latest_run()?;
            ui.output(&format!("  Latest run: {}", latest_run.id))?;
            ui.output(&format!(
                "  Tests in latest run: {}",
                latest_run.total_tests()
            ))?;
            ui.output(&format!(
                "  Failures in latest run: {}",
                latest_run.count_failures()
            ))?;

            // Calculate total tests across all runs
            let mut total_tests = 0;
            for run_id in &run_ids {
                if let Ok(run) = repo.get_test_run(run_id) {
                    total_tests += run.total_tests();
                }
            }
            ui.output(&format!("  Total tests executed: {}", total_tests))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "stats"
    }

    fn help(&self) -> &str {
        "Show repository statistics"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{TestId, TestResult, TestRun, TestStatus};
    use crate::ui::UI;
    use tempfile::TempDir;

    struct TestUI {
        output: Vec<String>,
        errors: Vec<String>,
    }

    impl TestUI {
        fn new() -> Self {
            TestUI {
                output: Vec::new(),
                errors: Vec::new(),
            }
        }
    }

    impl UI for TestUI {
        fn output(&mut self, message: &str) -> Result<()> {
            self.output.push(message.to_string());
            Ok(())
        }

        fn error(&mut self, message: &str) -> Result<()> {
            self.errors.push(message.to_string());
            Ok(())
        }

        fn warning(&mut self, message: &str) -> Result<()> {
            self.errors.push(format!("Warning: {}", message));
            Ok(())
        }
    }

    #[test]
    fn test_stats_command_empty_repo() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = StatsCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 2);
        assert_eq!(ui.output[0], "Repository Statistics:");
        assert_eq!(ui.output[1], "  Total test runs: 0");
    }

    #[test]
    fn test_stats_command_with_runs() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Add two test runs
        for i in 0..2 {
            let mut test_run = TestRun::new(i.to_string());
            test_run.timestamp =
                chrono::DateTime::from_timestamp(1000000000 + i as i64, 0).unwrap();

            test_run.add_result(TestResult {
                test_id: TestId::new(format!("test{}", i)),
                status: TestStatus::Success,
                duration: None,
                message: None,
                details: None,
                tags: vec![],
            });

            repo.insert_test_run(test_run).unwrap();
        }

        let mut ui = TestUI::new();
        let cmd = StatsCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 6);
        assert_eq!(ui.output[0], "Repository Statistics:");
        assert_eq!(ui.output[1], "  Total test runs: 2");
        assert_eq!(ui.output[2], "  Latest run: 1");
        assert_eq!(ui.output[3], "  Tests in latest run: 1");
        assert_eq!(ui.output[4], "  Failures in latest run: 0");
        assert_eq!(ui.output[5], "  Total tests executed: 2");
    }
}
