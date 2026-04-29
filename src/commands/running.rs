//! Show currently in-progress test runs

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{estimate_progress, RunId, TestId, TestRun};
use crate::ui::UI;
use std::collections::HashMap;
use std::time::Duration;

fn format_duration_secs(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render the one-line progress summary for a single in-progress run.
/// Pure: takes the run, historical timings, and elapsed wall-clock as inputs
/// so the formatting can be tested deterministically.
fn format_running_line(
    run_id: &RunId,
    test_run: &TestRun,
    historical: &HashMap<TestId, Duration>,
    elapsed: Duration,
) -> String {
    let observed = test_run.total_tests();
    let failures = test_run.count_failures();
    let passed = test_run.count_successes();
    let elapsed_secs = elapsed.as_secs() as i64;

    let (total_expected, percent, eta_secs) =
        estimate_progress(historical, test_run, Some(elapsed));

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

    line
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

            // Wall-clock elapsed comes from the actual run timestamp recorded
            // when the run was registered. `test_run.timestamp` is set to
            // `Utc::now()` in parse_stream and would always read as ~0s, so
            // we fall back to it only when the backend can't tell us.
            let started_at = repo
                .get_run_started_at(run_id)?
                .unwrap_or(test_run.timestamp);
            let elapsed_secs = (now - started_at).num_seconds().max(0);
            let elapsed = Duration::from_secs(elapsed_secs as u64);

            let line = format_running_line(run_id, &test_run, &historical, elapsed);
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
    use crate::repository::{RepositoryFactory, TestResult, TestStatus};
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

    #[test]
    fn test_format_running_line_with_history_zero_elapsed() {
        // 4 tests historically, each 2s. One completed in this run.
        // With elapsed = 0, estimate_progress takes the no-elapsed
        // fallback: ETA = sum of unobserved historical durations = 6s.
        let mut historical = HashMap::new();
        for i in 0..4 {
            historical.insert(TestId::new(format!("test_{i}")), Duration::from_secs(2));
        }
        let run_id = RunId::new("7");
        let mut run = TestRun::new(run_id.clone());
        run.add_result(TestResult {
            test_id: TestId::new("test_0"),
            status: TestStatus::Success,
            duration: Some(Duration::from_secs(2)),
            message: None,
            details: None,
            tags: vec![],
        });

        let line = format_running_line(&run_id, &run, &historical, Duration::from_secs(0));
        assert_eq!(
            line,
            "Run 7: 1/4 tests (1 passed, 0 failed), 25% complete, elapsed 0s, ETA 6s",
        );
    }

    #[test]
    fn test_format_running_line_with_history_nonzero_elapsed() {
        // Same setup, but with 4s elapsed: estimate_progress takes the
        // wall-clock projection branch. fraction_done = 0.25, projected
        // total = 4 / 0.25 = 16s, ETA = 12s.
        let mut historical = HashMap::new();
        for i in 0..4 {
            historical.insert(TestId::new(format!("test_{i}")), Duration::from_secs(2));
        }
        let run_id = RunId::new("7");
        let mut run = TestRun::new(run_id.clone());
        run.add_result(TestResult {
            test_id: TestId::new("test_0"),
            status: TestStatus::Success,
            duration: Some(Duration::from_secs(2)),
            message: None,
            details: None,
            tags: vec![],
        });

        let line = format_running_line(&run_id, &run, &historical, Duration::from_secs(4));
        assert_eq!(
            line,
            "Run 7: 1/4 tests (1 passed, 0 failed), 25% complete, elapsed 4s, ETA 12s",
        );
    }

    #[test]
    fn test_format_running_line_no_history_omits_eta_and_percent() {
        let historical = HashMap::new();
        let run_id = RunId::new("3");
        let run = TestRun::new(run_id.clone());

        let line = format_running_line(&run_id, &run, &historical, Duration::from_secs(2));
        assert_eq!(line, "Run 3: 0 tests (0 passed, 0 failed), elapsed 2s");
    }
}
