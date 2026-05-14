//! Run tests repeatedly and report which ones flap.
//!
//! `inq stress` drives [`crate::commands::RunCommand`] for a configurable
//! number of iterations, then reads the run IDs it produced back from the
//! repository and surfaces every test that flipped between pass and fail
//! across those iterations. Each iteration is a normal recorded run, so it
//! also shows up in `inq log`, `inq flaky`, etc.

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::config::TimeoutSetting;
use crate::error::Result;
use crate::ordering::TestOrder;
use crate::repository::{summarise_flakiness, Repository, RunId, TestFlakiness, TestId};
use crate::ui::UI;
use std::collections::HashMap;
use std::time::Duration;

/// Default number of iterations when `--iterations` is not given.
pub const DEFAULT_ITERATIONS: usize = 10;

/// Command to run the test suite repeatedly and report flaky tests.
#[derive(Default)]
pub struct StressCommand {
    /// Repository path (defaults to current directory).
    pub base_path: Option<String>,
    /// Number of iterations to run.
    pub iterations: usize,
    /// Stop early as soon as at least one flaky test is observed.
    pub stop_on_flaky: bool,
    /// Path to a file containing test IDs to run.
    pub load_list: Option<String>,
    /// Number of parallel test workers per iteration.
    pub concurrency: Option<usize>,
    /// Run each test in a separate process.
    pub isolated: bool,
    /// Test patterns to filter (regex).
    pub test_filters: Option<Vec<String>>,
    /// `--starting-with` prefixes (may be abbreviated).
    pub starting_with: Option<Vec<String>>,
    /// Additional arguments to pass to the test command.
    pub test_args: Option<Vec<String>>,
    /// Per-test timeout setting.
    pub test_timeout: TimeoutSetting,
    /// Overall run timeout setting.
    pub max_duration: TimeoutSetting,
    /// Kill test if no output for this duration.
    pub no_output_timeout: Option<Duration>,
    /// Maximum restarts on timeout/crash.
    pub max_restarts: Option<usize>,
    /// Optional explicit ordering strategy for each iteration.
    pub test_order: Option<TestOrder>,
}

impl StressCommand {
    /// Construct a stress command with default iteration count.
    pub fn new(base_path: Option<String>) -> Self {
        StressCommand {
            base_path,
            iterations: DEFAULT_ITERATIONS,
            ..Default::default()
        }
    }

    fn build_iteration(&self) -> crate::commands::RunCommand {
        crate::commands::RunCommand {
            base_path: self.base_path.clone(),
            load_list: self.load_list.clone(),
            concurrency: self.concurrency,
            isolated: self.isolated,
            test_filters: self.test_filters.clone(),
            starting_with: self.starting_with.clone(),
            test_args: self.test_args.clone(),
            test_timeout: self.test_timeout.clone(),
            max_duration: self.max_duration.clone(),
            no_output_timeout: self.no_output_timeout,
            max_restarts: self.max_restarts,
            // Each iteration runs the whole (filtered) suite, not just failing
            // tests; persist results as a full run, not a partial update.
            test_order: self.test_order.clone(),
            ..Default::default()
        }
    }
}

impl Command for StressCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        if self.iterations == 0 {
            return Err(crate::error::Error::Other(
                "stress: --iterations must be at least 1".to_string(),
            ));
        }

        let mut run_ids: Vec<RunId> = Vec::with_capacity(self.iterations);
        let mut any_failure = false;

        for iteration in 1..=self.iterations {
            ui.output(&format!(
                "\n=== Stress iteration {}/{} ===",
                iteration, self.iterations
            ))?;

            let run_cmd = self.build_iteration();
            let output = run_cmd.execute_returning_run_id(ui)?;

            if let Some(run_id) = output.run_id {
                run_ids.push(run_id);
            } else {
                // No run was recorded (e.g. no tests matched the filters).
                // Without runs to compare we can't measure flakiness, so bail
                // out early rather than silently doing nothing.
                ui.output(
                    "\nStress aborted: no test run was recorded for this iteration. \
                    Check that your filters match at least one test.",
                )?;
                return Ok(output.exit_code);
            }

            if output.exit_code != 0 {
                any_failure = true;
            }

            if self.stop_on_flaky && run_ids.len() >= 2 {
                let summary =
                    summarise_run_ids(&*open_repository(self.base_path.as_deref())?, &run_ids)?;
                if !summary.is_empty() {
                    ui.output(&format!(
                        "\nObserved {} flaky test(s) after {} iteration(s); stopping early.",
                        summary.len(),
                        iteration
                    ))?;
                    return self.report(ui, &run_ids, summary, any_failure);
                }
            }
        }

        let repo = open_repository(self.base_path.as_deref())?;
        let summary = summarise_run_ids(&*repo, &run_ids)?;
        drop(repo);
        self.report(ui, &run_ids, summary, any_failure)
    }

    fn name(&self) -> &str {
        "stress"
    }

    fn help(&self) -> &str {
        "Run the test suite repeatedly to flush out flaky tests"
    }
}

impl StressCommand {
    /// Print the flakiness table and choose the exit code.
    ///
    /// Exit code 0 if no flaky tests were observed across the iterations.
    /// Exit code 1 if at least one test flapped (the whole point of the
    /// command is to surface those). A hard run-level failure that did *not*
    /// produce flakiness (i.e. a test failed in every iteration) is reported
    /// in the summary but does not by itself fail the stress run — that is
    /// `inq run`'s job, not `inq stress`'s.
    fn report(
        &self,
        ui: &mut dyn UI,
        run_ids: &[RunId],
        flaky: Vec<TestFlakiness>,
        any_failure: bool,
    ) -> Result<i32> {
        ui.output(&format!(
            "\nStress summary ({} iteration(s), runs {}):",
            run_ids.len(),
            format_run_id_list(run_ids)
        ))?;

        if flaky.is_empty() {
            if any_failure {
                ui.output(
                    "  No flaky tests observed, but at least one iteration had failing tests \
                    (likely a consistently broken test rather than flakiness).",
                )?;
            } else {
                ui.output("  No flaky tests observed.")?;
            }
            return Ok(0);
        }

        ui.output(&format!("  Flaky tests: {}", flaky.len()))?;
        ui.output("  fail/runs  flake%  test")?;
        for entry in &flaky {
            ui.output(&format!(
                "  {:>4}/{:<4} {:5.1}%  {}",
                entry.failures,
                entry.runs,
                entry.flakiness_score * 100.0,
                entry.test_id,
            ))?;
        }
        Ok(1)
    }
}

/// Collect per-test pass/fail histories from a specific set of run IDs and
/// summarise them with the same scoring used by `inq flaky`.
///
/// Only tests that *both* passed and failed across the given runs end up in
/// the output (that's what [`summarise_flakiness`] enforces via its
/// `failures == 0` filter — we additionally want to drop tests that *always*
/// failed, so we require at least one success).
pub(crate) fn summarise_run_ids(
    repo: &dyn Repository,
    run_ids: &[RunId],
) -> Result<Vec<TestFlakiness>> {
    let mut history: HashMap<TestId, Vec<bool>> = HashMap::new();
    for run_id in run_ids {
        let run = repo.get_test_run(run_id)?;
        for (test_id, result) in &run.results {
            history
                .entry(test_id.clone())
                .or_default()
                .push(result.status.is_failure());
        }
    }

    // Drop tests that failed in every iteration in which they ran — those
    // are broken, not flaky. `summarise_flakiness` already drops the inverse
    // (never-failed) case.
    history.retain(|_, statuses| statuses.iter().any(|f| !f));

    // min_runs = 1 here because the caller already controls how many
    // iterations to run; we want every test that appeared in our stress
    // window to be eligible.
    Ok(summarise_flakiness(history, 1))
}

/// Render a run-id list as a compact comma-separated string, with `..` when
/// the IDs form a contiguous numeric range.
fn format_run_id_list(run_ids: &[RunId]) -> String {
    if run_ids.is_empty() {
        return String::new();
    }
    if run_ids.len() == 1 {
        return run_ids[0].to_string();
    }
    let parsed: Option<Vec<u64>> = run_ids.iter().map(|r| r.as_str().parse().ok()).collect();
    if let Some(nums) = parsed {
        let contiguous = nums.windows(2).all(|w| w[1] == w[0] + 1);
        if contiguous {
            return format!("{}..{}", nums[0], nums[nums.len() - 1]);
        }
    }
    run_ids
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, TestResult, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    fn insert_run(
        repo: &mut dyn Repository,
        run_id: &str,
        results: &[(&str, TestStatus)],
    ) -> RunId {
        let mut run = TestRun::new(RunId::new(run_id));
        run.timestamp = chrono::Utc::now();
        for (test_id, status) in results {
            run.add_result(TestResult {
                test_id: TestId::new(*test_id),
                status: *status,
                duration: None,
                message: None,
                details: None,
                tags: vec![],
            });
        }
        repo.insert_test_run(run).unwrap()
    }

    #[test]
    fn name_and_help() {
        let cmd = StressCommand::new(None);
        assert_eq!(cmd.name(), "stress");
        assert!(!cmd.help().is_empty());
    }

    #[test]
    fn zero_iterations_is_rejected() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut cmd = StressCommand::new(Some(temp.path().to_string_lossy().to_string()));
        cmd.iterations = 0;

        let mut ui = TestUI::new();
        let err = cmd.execute(&mut ui).unwrap_err();
        assert_eq!(err.to_string(), "stress: --iterations must be at least 1");
    }

    #[test]
    fn summarise_run_ids_isolates_flapping_from_broken_and_stable() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        // flap: pass, fail, pass — flaky
        // broken: fail, fail, fail — should be excluded (always fails)
        // stable: pass, pass, pass — excluded by summarise_flakiness (never fails)
        let r0 = insert_run(
            repo.as_mut(),
            "0",
            &[("flap", Success), ("broken", Failure), ("stable", Success)],
        );
        let r1 = insert_run(
            repo.as_mut(),
            "1",
            &[("flap", Failure), ("broken", Failure), ("stable", Success)],
        );
        let r2 = insert_run(
            repo.as_mut(),
            "2",
            &[("flap", Success), ("broken", Failure), ("stable", Success)],
        );

        let summary = summarise_run_ids(repo.as_ref(), &[r0, r1, r2]).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].test_id.as_str(), "flap");
        assert_eq!(summary[0].runs, 3);
        assert_eq!(summary[0].failures, 1);
        assert_eq!(summary[0].transitions, 2);
    }

    #[test]
    fn summarise_run_ids_ignores_other_runs_in_repo() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        // An older run that's *not* in our stress window — must not pollute results.
        insert_run(repo.as_mut(), "0", &[("t", Failure)]);
        let r1 = insert_run(repo.as_mut(), "1", &[("t", Success)]);
        let r2 = insert_run(repo.as_mut(), "2", &[("t", Success)]);

        // Inside the stress window t always passed, so it is not flaky.
        let summary = summarise_run_ids(repo.as_ref(), &[r1, r2]).unwrap();
        assert!(summary.is_empty());
    }

    #[test]
    fn format_run_id_list_contiguous_range() {
        let ids = vec![RunId::new("3"), RunId::new("4"), RunId::new("5")];
        assert_eq!(format_run_id_list(&ids), "3..5");
    }

    #[test]
    fn format_run_id_list_noncontiguous() {
        let ids = vec![RunId::new("3"), RunId::new("5"), RunId::new("7")];
        assert_eq!(format_run_id_list(&ids), "3,5,7");
    }

    #[test]
    fn format_run_id_list_single() {
        let ids = vec![RunId::new("11")];
        assert_eq!(format_run_id_list(&ids), "11");
    }

    #[test]
    fn format_run_id_list_non_numeric_falls_back_to_csv() {
        let ids = vec![RunId::new("abc"), RunId::new("def")];
        assert_eq!(format_run_id_list(&ids), "abc,def");
    }

    #[test]
    fn report_with_flaky_returns_exit_1() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 3,
            failures: 1,
            transitions: 2,
            flakiness_score: 1.0,
            failure_rate: 1.0 / 3.0,
        }];
        let code = cmd
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1"), RunId::new("2")],
                flaky,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        let out = ui.output.join("\n");
        assert!(out.contains("Flaky tests: 1"), "got: {}", out);
        assert!(out.contains("flap"), "got: {}", out);
        assert!(out.contains("0..2"), "got: {}", out);
    }

    #[test]
    fn report_without_flaky_but_with_failures() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let code = cmd
            .report(&mut ui, &[RunId::new("0"), RunId::new("1")], vec![], true)
            .unwrap();
        assert_eq!(code, 0);
        let out = ui.output.join("\n");
        assert!(out.contains("No flaky tests observed"), "got: {}", out);
        assert!(out.contains("consistently broken test"), "got: {}", out);
    }

    #[test]
    fn report_clean_run_returns_exit_0() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let code = cmd
            .report(&mut ui, &[RunId::new("0"), RunId::new("1")], vec![], false)
            .unwrap();
        assert_eq!(code, 0);
        let out = ui.output.join("\n");
        assert!(out.contains("No flaky tests observed."), "got: {}", out);
        assert!(!out.contains("consistently broken test"), "got: {}", out);
    }
}
