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

/// Maximum number of distinct failure messages to display per flaky test.
const MAX_MESSAGES_PER_TEST: usize = 3;
/// Maximum displayed length of any single failure message (longer messages
/// are truncated with an ellipsis).
const MAX_MESSAGE_LEN: usize = 200;

/// Per-flaky-test list of `(failure message, occurrence count)`, sorted
/// most-frequent-first. Tests that weren't flaky are omitted from the map.
pub type FailureMessageCounts = HashMap<TestId, Vec<(String, u32)>>;

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
    /// Niceness increment for spawned test processes (unix only). See
    /// [`crate::test_executor::TestExecutorConfig::nice`].
    pub nice: Option<i32>,
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

    fn build_iteration(&self, concurrency: Option<usize>) -> crate::commands::RunCommand {
        crate::commands::RunCommand {
            base_path: self.base_path.clone(),
            load_list: self.load_list.clone(),
            concurrency,
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

    /// Decide the per-iteration worker count to hand each child `RunCommand`.
    ///
    /// When the user passes `-j` we honour it verbatim (including `-j 1` to
    /// opt back into serial). Otherwise we auto-detect CPU count up front so
    /// that (a) every iteration runs in parallel, surfacing concurrency-only
    /// flakes that a serial run would hide, and (b) the auto-detection
    /// message prints once instead of once per iteration.
    fn resolve_iteration_concurrency(&self, ui: &mut dyn UI) -> Result<Option<usize>> {
        if let Some(c) = self.concurrency {
            return Ok(Some(c));
        }
        let cpus = num_cpus::get();
        if cpus > 1 {
            ui.output(&format!(
                "Stress: running each iteration in parallel across {} CPUs (auto-detected). \
                 Pass `-j N` to set a specific worker count, or `-j 1` to run serially.",
                cpus
            ))?;
        }
        Ok(Some(cpus))
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

        let iteration_concurrency = self.resolve_iteration_concurrency(ui)?;

        for iteration in 1..=self.iterations {
            ui.output(&format!(
                "\n=== Stress iteration {}/{} ===",
                iteration, self.iterations
            ))?;

            let run_cmd = self.build_iteration(iteration_concurrency);
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
                let (summary, messages) =
                    summarise_run_ids(&*open_repository(self.base_path.as_deref())?, &run_ids)?;
                if !summary.is_empty() {
                    ui.output(&format!(
                        "\nObserved {} flaky test(s) after {} iteration(s); stopping early.",
                        summary.len(),
                        iteration
                    ))?;
                    return self.report(ui, &run_ids, summary, messages, any_failure);
                }
            }
        }

        let repo = open_repository(self.base_path.as_deref())?;
        let (summary, messages) = summarise_run_ids(&*repo, &run_ids)?;
        drop(repo);
        self.report(ui, &run_ids, summary, messages, any_failure)
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
        messages: FailureMessageCounts,
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
            if let Some(msgs) = messages.get(&entry.test_id) {
                let shown = msgs.iter().take(MAX_MESSAGES_PER_TEST);
                for (msg, count) in shown {
                    ui.output(&format!(
                        "      [{}x] {}",
                        count,
                        truncate_message(msg, MAX_MESSAGE_LEN)
                    ))?;
                }
                if msgs.len() > MAX_MESSAGES_PER_TEST {
                    ui.output(&format!(
                        "      ... and {} other distinct message(s)",
                        msgs.len() - MAX_MESSAGES_PER_TEST
                    ))?;
                }
            }
        }
        Ok(1)
    }
}

/// Pick a short, comparable summary for a single failure. Prefers an explicit
/// `message`, falling back to the last non-empty line of `details` (which for
/// most test runners is the actual assertion or exception line at the tail of
/// the traceback), and finally a placeholder.
fn failure_summary(message: Option<&str>, details: Option<&str>) -> String {
    if let Some(m) = message.map(str::trim).filter(|s| !s.is_empty()) {
        return m.to_string();
    }
    if let Some(d) = details {
        if let Some(line) = d.lines().rev().map(str::trim).find(|s| !s.is_empty()) {
            return line.to_string();
        }
    }
    "(no message)".to_string()
}

/// Truncate a single-line preview of a failure message. Newlines are
/// collapsed to spaces so the table stays readable; the full message remains
/// available via `inq log <run-id>`.
fn truncate_message(msg: &str, max_len: usize) -> String {
    let collapsed: String = msg
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    if collapsed.chars().count() <= max_len {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(max_len).collect();
    format!("{}...", truncated)
}

/// Collect per-test pass/fail histories from a specific set of run IDs and
/// summarise them with the same scoring used by `inq flaky`.
///
/// Only tests that *both* passed and failed across the given runs end up in
/// the output (that's what [`summarise_flakiness`] enforces via its
/// `failures == 0` filter — we additionally want to drop tests that *always*
/// failed, so we require at least one success).
///
/// Returns the flakiness summary alongside a per-flaky-test list of distinct
/// failure messages with the number of iterations each occurred in, sorted
/// most-frequent-first. Tests that aren't flaky are omitted from the map.
pub(crate) fn summarise_run_ids(
    repo: &dyn Repository,
    run_ids: &[RunId],
) -> Result<(Vec<TestFlakiness>, FailureMessageCounts)> {
    let mut history: HashMap<TestId, Vec<bool>> = HashMap::new();
    let mut messages: HashMap<TestId, Vec<(String, u32)>> = HashMap::new();
    for run_id in run_ids {
        let run = repo.get_test_run(run_id)?;
        for (test_id, result) in &run.results {
            let is_failure = result.status.is_failure();
            history.entry(test_id.clone()).or_default().push(is_failure);
            if is_failure {
                let msg = failure_summary(result.message.as_deref(), result.details.as_deref());
                let entries = messages.entry(test_id.clone()).or_default();
                if let Some(slot) = entries.iter_mut().find(|(m, _)| m == &msg) {
                    slot.1 += 1;
                } else {
                    entries.push((msg, 1));
                }
            }
        }
    }

    // Drop tests that failed in every iteration in which they ran — those
    // are broken, not flaky. `summarise_flakiness` already drops the inverse
    // (never-failed) case.
    history.retain(|_, statuses| statuses.iter().any(|f| !f));

    // min_runs = 1 here because the caller already controls how many
    // iterations to run; we want every test that appeared in our stress
    // window to be eligible.
    let summary = summarise_flakiness(history, 1);

    // Restrict the message map to tests that ended up in the summary, and
    // sort each entry's messages most-frequent-first for predictable output.
    let flaky_ids: std::collections::HashSet<&TestId> =
        summary.iter().map(|f| &f.test_id).collect();
    messages.retain(|id, _| flaky_ids.contains(id));
    for entries in messages.values_mut() {
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }

    Ok((summary, messages))
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

        let (summary, _messages) = summarise_run_ids(repo.as_ref(), &[r0, r1, r2]).unwrap();
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
        let (summary, messages) = summarise_run_ids(repo.as_ref(), &[r1, r2]).unwrap();
        assert!(summary.is_empty());
        assert!(messages.is_empty());
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
                HashMap::new(),
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
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1")],
                vec![],
                HashMap::new(),
                true,
            )
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
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1")],
                vec![],
                HashMap::new(),
                false,
            )
            .unwrap();
        assert_eq!(code, 0);
        let out = ui.output.join("\n");
        assert!(out.contains("No flaky tests observed."), "got: {}", out);
        assert!(!out.contains("consistently broken test"), "got: {}", out);
    }

    fn insert_run_with_messages(
        repo: &mut dyn Repository,
        run_id: &str,
        results: &[(&str, TestStatus, Option<&str>)],
    ) -> RunId {
        let mut run = TestRun::new(RunId::new(run_id));
        run.timestamp = chrono::Utc::now();
        for (test_id, status, message) in results {
            let owned = message.map(|m| m.to_string());
            run.add_result(TestResult {
                test_id: TestId::new(*test_id),
                status: *status,
                duration: None,
                message: owned.clone(),
                details: owned,
                tags: vec![],
            });
        }
        repo.insert_test_run(run).unwrap()
    }

    #[test]
    fn summarise_run_ids_collects_failure_messages_for_flaky_tests() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        let r0 = insert_run_with_messages(
            repo.as_mut(),
            "0",
            &[("flap", Failure, Some("connection refused"))],
        );
        let r1 = insert_run_with_messages(repo.as_mut(), "1", &[("flap", Success, None)]);
        let r2 = insert_run_with_messages(
            repo.as_mut(),
            "2",
            &[("flap", Failure, Some("connection refused"))],
        );
        let r3 = insert_run_with_messages(
            repo.as_mut(),
            "3",
            &[("flap", Failure, Some("timeout after 5s"))],
        );

        let (summary, messages) = summarise_run_ids(repo.as_ref(), &[r0, r1, r2, r3]).unwrap();
        assert_eq!(summary.len(), 1);
        let entries = messages.get(&TestId::new("flap")).expect("flap messages");
        assert_eq!(
            entries,
            &vec![
                ("connection refused".to_string(), 2),
                ("timeout after 5s".to_string(), 1),
            ]
        );
    }

    #[test]
    fn summarise_run_ids_uses_placeholder_for_missing_message() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        let r0 = insert_run_with_messages(repo.as_mut(), "0", &[("flap", Failure, None)]);
        let r1 = insert_run_with_messages(repo.as_mut(), "1", &[("flap", Success, None)]);

        let (_summary, messages) = summarise_run_ids(repo.as_ref(), &[r0, r1]).unwrap();
        let entries = messages.get(&TestId::new("flap")).expect("flap messages");
        assert_eq!(entries, &vec![("(no message)".to_string(), 1)]);
    }

    #[test]
    fn report_renders_failure_messages_with_counts() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 4,
            failures: 3,
            transitions: 2,
            flakiness_score: 2.0 / 3.0,
            failure_rate: 0.75,
        }];
        let mut messages = HashMap::new();
        messages.insert(
            TestId::new("flap"),
            vec![
                ("connection refused".to_string(), 2),
                ("timeout after 5s".to_string(), 1),
            ],
        );

        let code = cmd
            .report(
                &mut ui,
                &[
                    RunId::new("0"),
                    RunId::new("1"),
                    RunId::new("2"),
                    RunId::new("3"),
                ],
                flaky,
                messages,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        let out = ui.output.join("\n");
        assert!(out.contains("[2x] connection refused"), "got: {}", out);
        assert!(out.contains("[1x] timeout after 5s"), "got: {}", out);
    }

    #[test]
    fn report_caps_messages_per_test() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 5,
            failures: 5,
            transitions: 0,
            flakiness_score: 0.0,
            failure_rate: 1.0,
        }];
        let mut messages = HashMap::new();
        let entries: Vec<(String, u32)> = (0..MAX_MESSAGES_PER_TEST + 2)
            .map(|i| (format!("msg-{}", i), 1))
            .collect();
        messages.insert(TestId::new("flap"), entries);

        let code = cmd
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1")],
                flaky,
                messages,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        let out = ui.output.join("\n");
        assert!(
            out.contains("and 2 other distinct message(s)"),
            "got: {}",
            out
        );
    }

    #[test]
    fn resolve_iteration_concurrency_honours_explicit() {
        let mut cmd = StressCommand::new(None);
        cmd.concurrency = Some(4);
        let mut ui = TestUI::new();
        assert_eq!(cmd.resolve_iteration_concurrency(&mut ui).unwrap(), Some(4));
        assert_eq!(ui.output, Vec::<String>::new());
    }

    #[test]
    fn resolve_iteration_concurrency_explicit_serial_is_silent() {
        let mut cmd = StressCommand::new(None);
        cmd.concurrency = Some(1);
        let mut ui = TestUI::new();
        assert_eq!(cmd.resolve_iteration_concurrency(&mut ui).unwrap(), Some(1));
        assert_eq!(ui.output, Vec::<String>::new());
    }

    #[test]
    fn resolve_iteration_concurrency_auto_detects_when_unset() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let resolved = cmd.resolve_iteration_concurrency(&mut ui).unwrap();
        let cpus = num_cpus::get();
        assert_eq!(resolved, Some(cpus));
        if cpus > 1 {
            assert_eq!(
                ui.output,
                vec![format!(
                    "Stress: running each iteration in parallel across {} CPUs (auto-detected). \
                     Pass `-j N` to set a specific worker count, or `-j 1` to run serially.",
                    cpus
                )]
            );
        } else {
            assert_eq!(ui.output, Vec::<String>::new());
        }
    }

    #[test]
    fn failure_summary_prefers_message_then_details_tail() {
        assert_eq!(failure_summary(Some("boom"), Some("ignored")), "boom");
        assert_eq!(
            failure_summary(Some("  "), Some("first\nlast line")),
            "last line"
        );
        assert_eq!(failure_summary(None, Some("only\n   \n  ")), "only");
        assert_eq!(failure_summary(None, None), "(no message)");
    }

    #[test]
    fn truncate_message_collapses_newlines_and_caps_length() {
        let short = truncate_message("hello\n\nworld", 80);
        assert_eq!(short, "hello / world");

        let long = "x".repeat(MAX_MESSAGE_LEN + 50);
        let cut = truncate_message(&long, MAX_MESSAGE_LEN);
        assert!(cut.ends_with("..."));
        assert_eq!(cut.chars().count(), MAX_MESSAGE_LEN + 3);
    }
}
