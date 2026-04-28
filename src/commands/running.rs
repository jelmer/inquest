//! Show currently in-progress test runs

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::repository::estimate_progress;
use crate::ui::UI;

fn format_duration_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
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

        // Historical per-test durations let us estimate the total expected
        // count and the time remaining for each in-progress run.
        let historical = repo.get_test_times().unwrap_or_default();

        let now = chrono::Utc::now();
        for run_id in &run_ids {
            let test_run = repo.get_test_run(run_id)?;
            let observed = test_run.total_tests();
            let failures = test_run.count_failures();
            let passed = test_run.count_successes();

            // Wall-clock elapsed comes from the actual run timestamp recorded
            // when the run was registered. `test_run.timestamp` is set to
            // `Utc::now()` in parse_stream and would always read as ~0s, so
            // we fall back to it only when the backend can't tell us.
            let started_at = repo
                .get_run_started_at(run_id)?
                .unwrap_or(test_run.timestamp);
            let elapsed_secs = (now - started_at).num_seconds().max(0);
            let elapsed = std::time::Duration::from_secs(elapsed_secs as u64);

            let (total_expected, percent, eta_secs) =
                estimate_progress(&historical, &test_run, Some(elapsed));

            let count_part = match total_expected {
                Some(total) => format!("{}/{} tests", observed, total),
                None => format!("{} tests", observed),
            };

            let mut line = format!(
                "Run {}: {} ({} passed, {} failed)",
                run_id, count_part, passed, failures,
            );

            if let Some(p) = percent {
                line.push_str(&format!(", {:.0}% complete", p * 100.0));
            }

            line.push_str(&format!(", elapsed {}", format_duration_secs(elapsed_secs)));

            if let Some(eta) = eta_secs {
                line.push_str(&format!(", ETA {}", format_duration_secs(eta as i64)));
            }

            ui.output(&line)?;
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
    use crate::repository::{RepositoryFactory, RunId, TestId, TestResult, TestRun, TestStatus};
    use crate::subunit_stream;
    use crate::ui::test_ui::TestUI;
    use std::io::Write;
    use std::time::Duration;
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

    #[test]
    fn test_running_command_shows_progress_with_history() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Seed historical durations for 4 tests so we can compute a fraction
        // and an ETA.
        let mut times = std::collections::HashMap::new();
        for i in 0..4 {
            times.insert(TestId::new(format!("test_{i}")), Duration::from_secs(2));
        }
        repo.update_test_times(&times).unwrap();

        // Begin an in-progress run with one test completed, leaving the lock
        // file in place so `inq running` sees it.
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        let mut run = TestRun::new(run_id.clone());
        run.timestamp = chrono::DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        run.add_result(TestResult {
            test_id: TestId::new("test_0"),
            status: TestStatus::Success,
            duration: Some(Duration::from_secs(2)),
            message: None,
            details: None,
            tags: vec![],
        });
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        writer.flush().unwrap();
        drop(repo);

        let mut ui = TestUI::new();
        let cmd = RunningCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        assert_eq!(ui.output.len(), 1);
        let line = &ui.output[0];
        assert!(line.contains(&format!("Run {}", run_id)), "line: {}", line);
        assert!(line.contains("1/4 tests"), "line: {}", line);
        assert!(line.contains("1 passed"), "line: {}", line);
        assert!(line.contains("0 failed"), "line: {}", line);
        assert!(line.contains("25% complete"), "line: {}", line);
        // The run was just begun, so elapsed wall-clock is essentially 0
        // and `estimate_progress` takes the no-elapsed fallback: sum of
        // unobserved historical durations = 3 × 2s = 6s. The wall-clock
        // projection branch is exercised in test_run.rs unit tests.
        assert!(line.contains("ETA 6s"), "line: {}", line);
        assert!(line.contains("elapsed"), "line: {}", line);

        drop(writer);
    }

    #[test]
    fn test_running_command_no_history_omits_eta() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        let run = TestRun::new(run_id.clone());
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        writer.flush().unwrap();
        drop(repo);

        let mut ui = TestUI::new();
        let cmd = RunningCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui).unwrap();

        assert_eq!(result, 0);
        let line = &ui.output[0];
        assert!(line.contains(&format!("Run {}", run_id)), "line: {}", line);
        assert!(!line.contains("ETA"), "line: {}", line);
        assert!(!line.contains("% complete"), "line: {}", line);
        assert!(line.contains("0 tests"), "line: {}", line);
        assert!(line.contains("elapsed"), "line: {}", line);

        drop(writer);
    }
}
