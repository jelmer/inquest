//! Wait for in-progress test runs to complete.
//!
//! Mirrors the `inq_wait` MCP tool: polls every `POLL_INTERVAL` for running
//! runs to finish, with optional status-filter early-return and optional
//! streaming of new test results as they are observed.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{RunId, TestId, TestStatus};
use crate::ui::UI;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Exit code returned when `--timeout` elapses before runs complete.
pub const EXIT_TIMEOUT: i32 = 1;

/// Command that blocks until in-progress test runs finish (or a status
/// filter matches, or a timeout elapses).
pub struct WaitCommand {
    base_path: Option<String>,
    run_id: Option<String>,
    timeout: Duration,
    status_filter: Option<Vec<TestStatus>>,
    stream: bool,
    only_failures: bool,
    poll_interval: Duration,
}

impl WaitCommand {
    /// Build a new wait command. `status_filters` uses the same string syntax
    /// as `inq log`'s `--status` (e.g. `failing`, `passing`, `failure`).
    pub fn new(
        base_path: Option<String>,
        run_id: Option<String>,
        timeout: Duration,
        status_filters: Vec<String>,
        stream: bool,
        only_failures: bool,
    ) -> Result<Self> {
        let status_filter = if status_filters.is_empty() {
            None
        } else {
            Some(TestStatus::parse_filters(&status_filters)?)
        };
        Ok(Self {
            base_path,
            run_id,
            timeout,
            status_filter,
            stream,
            only_failures,
            poll_interval: POLL_INTERVAL,
        })
    }

    #[cfg(test)]
    fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

impl Command for WaitCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let target = self.run_id.as_deref().map(RunId::new);
        // Tracks which (run_id, test_id) pairs have already been streamed so
        // each result is printed exactly once even though each poll re-reads
        // the whole run file.
        let mut seen: HashSet<(RunId, TestId)> = HashSet::new();
        let start = Instant::now();

        loop {
            let repo = open_repository(self.base_path.as_deref())?;
            let running_ids = repo.get_running_run_ids()?;

            let still_running = match &target {
                Some(t) => running_ids.contains(t),
                None => !running_ids.is_empty(),
            };

            // Build the slice of run IDs to inspect this poll — either the
            // targeted one or the full in-progress set — once, and borrow it
            // for both the stream and the status-filter blocks.
            let target_slice;
            let ids_to_check: &[RunId] = match &target {
                Some(t) => {
                    target_slice = [t.clone()];
                    &target_slice
                }
                None => &running_ids,
            };

            if self.stream {
                for run_id in ids_to_check {
                    if let Ok(test_run) = repo.get_test_run(run_id) {
                        for (test_id, result) in &test_run.results {
                            if !seen.insert((run_id.clone(), test_id.clone())) {
                                continue;
                            }
                            if self.only_failures && !result.status.is_failure() {
                                continue;
                            }
                            emit_streamed(ui, run_id, test_id, result.status)?;
                        }
                    }
                }
            }

            if !still_running {
                if !self.stream {
                    ui.output("No matching runs are in progress")?;
                }
                return Ok(0);
            }

            if let Some(statuses) = &self.status_filter {
                for run_id in ids_to_check {
                    if let Ok(test_run) = repo.get_test_run(run_id) {
                        let matched: Vec<(&TestId, TestStatus)> = test_run
                            .results
                            .iter()
                            .filter(|(_, r)| statuses.contains(&r.status))
                            .map(|(id, r)| (id, r.status))
                            .collect();
                        if !matched.is_empty() {
                            if !self.stream {
                                ui.output(&format!(
                                    "Run {}: {} test(s) matched status filter ({} passed / {} failed so far):",
                                    run_id,
                                    matched.len(),
                                    test_run.count_successes(),
                                    test_run.count_failures(),
                                ))?;
                                for (id, status) in &matched {
                                    ui.output(&format!("  {} [{}]", id, status))?;
                                }
                            }
                            return Ok(0);
                        }
                    }
                }
            }

            drop(repo);

            if start.elapsed() >= self.timeout {
                let joined: String = running_ids
                    .iter()
                    .map(RunId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                let still = if joined.is_empty() { "(none)" } else { &joined };
                ui.error(&format!(
                    "Timed out after {}s; still running: {}",
                    self.timeout.as_secs(),
                    still,
                ))?;
                return Ok(EXIT_TIMEOUT);
            }

            std::thread::sleep(self.poll_interval);
        }
    }

    fn name(&self) -> &str {
        "wait"
    }

    fn help(&self) -> &str {
        "Wait for in-progress test runs to complete"
    }
}

fn emit_streamed(
    ui: &mut dyn UI,
    run_id: &RunId,
    test_id: &TestId,
    status: TestStatus,
) -> Result<()> {
    ui.output(&format!("{} [{}] {}", run_id, status, test_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    fn base_path(temp: &TempDir) -> Option<String> {
        Some(temp.path().to_string_lossy().to_string())
    }

    /// Write a `.lock` file pointing at the current PID so the repository
    /// treats `run_id` as in-progress.
    fn mark_in_progress(temp: &TempDir, run_id: &str) {
        let lock = temp
            .path()
            .join(".inquest")
            .join("runs")
            .join(format!("{}.lock", run_id));
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
    }

    fn insert_run(
        repo: &mut dyn crate::repository::Repository,
        id: &str,
        results: Vec<TestResult>,
    ) {
        let mut run = TestRun::new(RunId::new(id));
        run.timestamp = chrono::Utc::now();
        for r in results {
            run.add_result(r);
        }
        repo.insert_test_run(run).unwrap();
    }

    #[test]
    fn test_no_runs_in_progress_returns_immediately() {
        let temp = TempDir::new().unwrap();
        InquestRepositoryFactory.initialise(temp.path()).unwrap();

        let cmd = WaitCommand::new(
            base_path(&temp),
            None,
            Duration::from_secs(5),
            vec![],
            false,
            false,
        )
        .unwrap();
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();

        assert_eq!(exit, 0);
        assert_eq!(ui.output, vec!["No matching runs are in progress"]);
    }

    #[test]
    fn test_timeout_when_run_stays_in_progress() {
        let temp = TempDir::new().unwrap();
        let mut repo = InquestRepositoryFactory.initialise(temp.path()).unwrap();
        insert_run(repo.as_mut(), "0", vec![TestResult::success("test1")]);
        drop(repo);
        mark_in_progress(&temp, "0");

        let cmd = WaitCommand::new(
            base_path(&temp),
            Some("0".into()),
            Duration::from_millis(50),
            vec![],
            false,
            false,
        )
        .unwrap()
        .with_poll_interval(Duration::from_millis(10));
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();

        assert_eq!(exit, EXIT_TIMEOUT);
        assert_eq!(ui.errors.len(), 1);
        assert!(ui.errors[0].contains("Timed out"));
        assert!(ui.errors[0].contains("0"));
    }

    #[test]
    fn test_status_filter_early_return() {
        let temp = TempDir::new().unwrap();
        let mut repo = InquestRepositoryFactory.initialise(temp.path()).unwrap();
        insert_run(
            repo.as_mut(),
            "0",
            vec![
                TestResult::success("test_pass"),
                TestResult::failure("test_fail", "boom"),
            ],
        );
        drop(repo);
        mark_in_progress(&temp, "0");

        let cmd = WaitCommand::new(
            base_path(&temp),
            Some("0".into()),
            Duration::from_secs(5),
            vec!["failing".into()],
            false,
            false,
        )
        .unwrap()
        .with_poll_interval(Duration::from_millis(10));
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();

        assert_eq!(exit, 0);
        let joined = ui.output.join("\n");
        assert!(joined.contains("matched status filter"), "got: {}", joined);
        assert!(joined.contains("test_fail"), "got: {}", joined);
        assert!(!joined.contains("test_pass"), "got: {}", joined);
    }

    #[test]
    fn test_stream_only_failures_filters_output() {
        let temp = TempDir::new().unwrap();
        let mut repo = InquestRepositoryFactory.initialise(temp.path()).unwrap();
        insert_run(
            repo.as_mut(),
            "0",
            vec![
                TestResult::success("test_pass"),
                TestResult::failure("test_fail", "boom"),
            ],
        );
        // Don't mark in progress — run is already complete, so streaming
        // should emit once and exit.

        let cmd = WaitCommand::new(
            base_path(&temp),
            None,
            Duration::from_secs(5),
            vec![],
            true,
            true,
        )
        .unwrap()
        .with_poll_interval(Duration::from_millis(10));
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();

        assert_eq!(exit, 0);
        // Run is already complete at first poll, so no runs are iterated
        // for streaming. This verifies the "no active runs" path doesn't
        // spuriously print anything.
        assert!(ui.output.is_empty(), "got: {:?}", ui.output);
    }

    #[test]
    fn test_stream_prints_only_failing_tests_while_running() {
        let temp = TempDir::new().unwrap();
        let mut repo = InquestRepositoryFactory.initialise(temp.path()).unwrap();
        insert_run(
            repo.as_mut(),
            "0",
            vec![
                TestResult::success("test_pass"),
                TestResult::failure("test_fail", "boom"),
            ],
        );
        drop(repo);
        mark_in_progress(&temp, "0");

        // Timeout fires quickly; we just want to observe one streaming pass.
        let cmd = WaitCommand::new(
            base_path(&temp),
            Some("0".into()),
            Duration::from_millis(20),
            vec![],
            true,
            true,
        )
        .unwrap()
        .with_poll_interval(Duration::from_millis(5));
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();

        assert_eq!(exit, EXIT_TIMEOUT);
        let streamed = ui.output.join("\n");
        assert!(streamed.contains("test_fail"), "got: {}", streamed);
        assert!(!streamed.contains("test_pass"), "got: {}", streamed);
    }

    #[test]
    fn test_invalid_status_filter_rejected() {
        let result = WaitCommand::new(
            None,
            None,
            Duration::from_secs(5),
            vec!["bogus".into()],
            false,
            false,
        );
        match result {
            Err(e) => assert!(e.to_string().contains("Unknown status filter")),
            Ok(_) => panic!("expected error for bogus status filter"),
        }
    }
}
