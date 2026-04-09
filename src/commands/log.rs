//! Show logs for individual tests from a test run

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::subunit_stream;
use crate::ui::UI;
use glob::Pattern;

/// Command to display logs for specific tests from a test run.
pub struct LogCommand {
    base_path: Option<String>,
    run_id: Option<String>,
    test_patterns: Vec<Pattern>,
}

impl LogCommand {
    /// Creates a new log command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `run_id` - Run ID to show logs from (None = latest)
    /// * `test_patterns` - Glob patterns to match test IDs against
    pub fn new(
        base_path: Option<String>,
        run_id: Option<String>,
        test_patterns: Vec<Pattern>,
    ) -> Self {
        LogCommand {
            base_path,
            run_id,
            test_patterns,
        }
    }
}

fn matches_any_pattern(test_id: &str, patterns: &[Pattern]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|p| p.matches(test_id))
}

impl Command for LogCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let run_id = resolve_run_id(&*repo, self.run_id.as_deref())?;

        let raw_stream = repo.get_test_run_raw(&run_id)?;

        let test_run = subunit_stream::parse_stream_with_progress(
            raw_stream,
            run_id.clone(),
            |_test_id, _status| {},
            |_bytes| {},
            subunit_stream::OutputFilter::All,
        )?;

        let mut matching_results: Vec<_> = test_run
            .results
            .values()
            .filter(|r| matches_any_pattern(r.test_id.as_str(), &self.test_patterns))
            .collect();
        matching_results.sort_by_key(|r| r.test_id.as_str());

        if matching_results.is_empty() {
            if self.test_patterns.is_empty() {
                ui.error(&format!("No tests found in run {}", run_id))?;
            } else {
                let pattern_strs: Vec<_> = self.test_patterns.iter().map(|p| p.as_str()).collect();
                ui.error(&format!(
                    "No tests matching '{}' found in run {}",
                    pattern_strs.join("', '"),
                    run_id
                ))?;
            }
            return Ok(1);
        }

        if let Some(interruption) = &test_run.interruption {
            ui.output(&format!(
                "WARNING: Stream interrupted ({}), results may be incomplete",
                interruption
            ))?;
        }

        for (i, result) in matching_results.iter().enumerate() {
            if i > 0 {
                ui.output("")?;
            }

            ui.output(&format!("test: {}", result.test_id))?;
            ui.output(&format!("status: {}", result.status))?;
            if let Some(duration) = result.duration {
                ui.output(&format!("duration: {:.3}s", duration.as_secs_f64()))?;
            }

            if let Some(ref details) = result.details {
                ui.output("---")?;
                ui.output(details)?;
            }
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "log"
    }

    fn help(&self) -> &str {
        "Show logs for individual tests from a test run"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestId, TestResult, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn test_log_command_latest_run() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: Some(std::time::Duration::from_millis(100)),
            message: None,
            details: Some("test output here".to_string()),
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = LogCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            vec![Pattern::new("test1").unwrap()],
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output[0], "test: test1");
        assert_eq!(ui.output[1], "status: success");
        assert_eq!(ui.output[2], "duration: 0.100s");
        assert_eq!(ui.output[3], "---");
        assert_eq!(ui.output[4], "test output here");
    }

    #[test]
    fn test_log_command_specific_run() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run0 = TestRun::new(RunId::new("0"));
        run0.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run0.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("failed".to_string()),
            details: Some("old output".to_string()),
            tags: vec![],
        });
        repo.insert_test_run(run0).unwrap();

        let mut run1 = TestRun::new(RunId::new("1"));
        run1.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run1.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: Some("new output".to_string()),
            tags: vec![],
        });
        repo.insert_test_run(run1).unwrap();

        let mut ui = TestUI::new();
        let cmd = LogCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            Some("0".to_string()),
            vec![Pattern::new("test1").unwrap()],
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);
        assert!(ui.output.iter().any(|s| s == "status: failure"));
        assert!(ui.output.iter().any(|s| s == "old output"));
    }

    #[test]
    fn test_log_command_wildcard() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("module.TestClass.test_alpha"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: Some("alpha output".to_string()),
            tags: vec![],
        });
        test_run.add_result(TestResult {
            test_id: TestId::new("module.TestClass.test_beta"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("failed".to_string()),
            details: Some("beta output".to_string()),
            tags: vec![],
        });
        test_run.add_result(TestResult {
            test_id: TestId::new("other.Unrelated.test_gamma"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: Some("gamma output".to_string()),
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = LogCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            vec![Pattern::new("module.TestClass.*").unwrap()],
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let all_output = ui.output.join("\n");
        assert!(all_output.contains("test_alpha"));
        assert!(all_output.contains("test_beta"));
        assert!(!all_output.contains("test_gamma"));
    }

    #[test]
    fn test_log_command_no_patterns_shows_all() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(TestResult {
            test_id: TestId::new("test1"),
            status: TestStatus::Success,
            duration: None,
            message: None,
            details: Some("output1".to_string()),
            tags: vec![],
        });
        test_run.add_result(TestResult {
            test_id: TestId::new("test2"),
            status: TestStatus::Failure,
            duration: None,
            message: Some("failed".to_string()),
            details: Some("output2".to_string()),
            tags: vec![],
        });

        repo.insert_test_run(test_run).unwrap();

        let mut ui = TestUI::new();
        let cmd = LogCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            vec![],
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let all_output = ui.output.join("\n");
        assert!(all_output.contains("test1"));
        assert!(all_output.contains("test2"));
    }

    #[test]
    fn test_log_command_no_match() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = TestRun::new(RunId::new("0"));
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
        let cmd = LogCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            None,
            vec![Pattern::new("nonexistent*").unwrap()],
        );
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 1);
        assert!(ui.errors[0].contains("No tests matching"));
    }
}
