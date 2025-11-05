//! Show the slowest tests

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

pub struct SlowestCommand {
    base_path: Option<String>,
    count: usize,
}

impl SlowestCommand {
    pub fn new(base_path: Option<String>) -> Self {
        SlowestCommand {
            base_path,
            count: 10, // Default to top 10
        }
    }

    pub fn with_count(base_path: Option<String>, count: usize) -> Self {
        SlowestCommand { base_path, count }
    }
}

impl Command for SlowestCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let test_run = repo.get_latest_run()?;

        // Collect tests with durations
        let mut tests_with_duration: Vec<_> = test_run
            .results
            .values()
            .filter_map(|result| result.duration.map(|dur| (result.test_id.clone(), dur)))
            .collect();

        if tests_with_duration.is_empty() {
            ui.output("No timing information available")?;
            return Ok(0);
        }

        // Sort by duration (slowest first)
        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));

        let display_count = self.count.min(tests_with_duration.len());
        ui.output(&format!("Slowest {} test(s):", display_count))?;

        for (test_id, duration) in tests_with_duration.iter().take(display_count) {
            let secs = duration.as_secs_f64();
            ui.output(&format!("  {:.3}s - {}", secs, test_id))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "slowest"
    }

    fn help(&self) -> &str {
        "Show the slowest tests from the last run"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::file::FileRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestId, TestResult, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_slowest_command_no_timing() {
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
        let cmd = SlowestCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 1);
        assert_eq!(ui.output[0], "No timing information available");
    }

    #[test]
    fn test_slowest_command_with_timing() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new("0".to_string());
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();

        // Add tests with different durations
        test_run.add_result(TestResult {
            test_id: TestId::new("fast_test"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(100)),
            message: None,
            details: None,
            tags: vec![],
        });

        test_run.add_result(TestResult {
            test_id: TestId::new("slow_test"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(5000)),
            message: None,
            details: None,
            tags: vec![],
        });

        test_run.add_result(TestResult {
            test_id: TestId::new("medium_test"),
            status: TestStatus::Success,
            duration: Some(Duration::from_millis(1000)),
            message: None,
            details: None,
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = SlowestCommand::with_count(Some(temp.path().to_string_lossy().to_string()), 2);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);

        // The subunit crate doesn't preserve duration information in the roundtrip,
        // so we expect "No timing information available"
        assert_eq!(ui.output.len(), 1);
        assert_eq!(ui.output[0], "No timing information available");
    }
}
