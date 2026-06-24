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
use crate::repository::{
    summarise_flakiness, ConcurrencyBreakdown, Repository, RunId, TestFlakiness, TestId,
};
use crate::ui::UI;
use std::collections::HashMap;
use std::time::Duration;

/// Maximum number of distinct failure messages to display per flaky test.
const MAX_MESSAGES_PER_TEST: usize = 3;
/// Maximum displayed length of any single failure message (longer messages
/// are truncated with an ellipsis).
const MAX_MESSAGE_LEN: usize = 200;
/// Maximum number of lines of full traceback/details printed per distinct
/// failure mode in the analysis section.
const MAX_DETAILS_LINES: usize = 30;

/// One distinct failure mode for a flaky test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureMode {
    /// Single-line summary used to group failures (from `failure_summary`).
    pub message: String,
    /// Number of iterations in which this exact message appeared.
    pub count: u32,
    /// Full `details` (typically a traceback) from a representative failure,
    /// if any of the failures grouped under this message had details. We
    /// keep one representative rather than all, on the assumption that
    /// identically-summarised failures share a stack.
    pub details: Option<String>,
}

/// Per-flaky-test analysis collected from the stress window.
#[derive(Debug, Clone, Default)]
pub struct FailureAnalysis {
    /// 1-based stress iteration indices in which this test failed.
    pub failing_iterations: Vec<u32>,
    /// Run IDs corresponding to `failing_iterations`, same ordering.
    pub failing_run_ids: Vec<RunId>,
    /// Distinct failure modes, sorted most-frequent-first.
    pub modes: Vec<FailureMode>,
    /// Pass/fail counts stratified by the concurrency level of each
    /// iteration the test appeared in.
    pub concurrency: ConcurrencyBreakdown,
}

/// Per-flaky-test failure analysis keyed by test ID. Tests that weren't
/// flaky are omitted from the map.
pub type FailureAnalyses = HashMap<TestId, FailureAnalysis>;

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
        analyses: FailureAnalyses,
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
            if let Some(analysis) = analyses.get(&entry.test_id) {
                let shown = analysis.modes.iter().take(MAX_MESSAGES_PER_TEST);
                for mode in shown {
                    ui.output(&format!(
                        "      [{}x] {}",
                        mode.count,
                        truncate_message(&mode.message, MAX_MESSAGE_LEN)
                    ))?;
                }
                if analysis.modes.len() > MAX_MESSAGES_PER_TEST {
                    ui.output(&format!(
                        "      ... and {} other distinct message(s)",
                        analysis.modes.len() - MAX_MESSAGES_PER_TEST
                    ))?;
                }
            }
        }

        // Per-test analysis: failure rate, which iterations failed, and the
        // full traceback for each distinct failure mode. The table above is
        // a scannable index; this block is the "open the failure" view.
        ui.output("\nFailure analysis:")?;
        for entry in &flaky {
            let Some(analysis) = analyses.get(&entry.test_id) else {
                continue;
            };
            ui.output(&format!("\n* {}", entry.test_id))?;
            ui.output(&format!(
                "    failure rate: {}/{} iteration(s) ({:.1}%)",
                entry.failures,
                entry.runs,
                entry.failure_rate * 100.0
            ))?;
            if (entry.runs as usize) < run_ids.len() {
                ui.output(&format!(
                    "    note: only appeared in {} of {} stress iteration(s) — \
                     missing iterations may indicate sharding, ordering, or a \
                     prior crash that prevented this test from running.",
                    entry.runs,
                    run_ids.len()
                ))?;
            }
            ui.output(&format!(
                "    failed in iteration(s): {}",
                format_iteration_list(&analysis.failing_iterations)
            ))?;
            ui.output(&format!(
                "    failing run id(s): {}",
                format_run_id_list(&analysis.failing_run_ids)
            ))?;
            if let Some(verdict) = analysis.concurrency.verdict() {
                ui.output(&format!("    concurrency: {}", verdict))?;
            }
            for (i, mode) in analysis.modes.iter().enumerate() {
                ui.output(&format!(
                    "    failure mode {} of {} ({}x): {}",
                    i + 1,
                    analysis.modes.len(),
                    mode.count,
                    truncate_message(&mode.message, MAX_MESSAGE_LEN),
                ))?;
                if let Some(details) = &mode.details {
                    render_details_block(ui, details, MAX_DETAILS_LINES)?;
                }
            }
        }
        Ok(1)
    }
}

/// Render up to `max_lines` of an indented details/traceback block,
/// summarising the tail when the source is longer.
fn render_details_block(ui: &mut dyn UI, details: &str, max_lines: usize) -> Result<()> {
    let lines: Vec<&str> = details.lines().collect();
    let (shown, truncated) = if lines.len() > max_lines {
        (&lines[..max_lines], lines.len() - max_lines)
    } else {
        (&lines[..], 0)
    };
    for line in shown {
        ui.output(&format!("        | {}", line))?;
    }
    if truncated > 0 {
        ui.output(&format!(
            "        | ... ({} more line(s) truncated; see `inq log <run-id>` for the full output)",
            truncated
        ))?;
    }
    Ok(())
}

/// Format a list of 1-based iteration indices, collapsing contiguous runs
/// into `start..end` ranges (e.g. `1,3..5,8`).
fn format_iteration_list(iterations: &[u32]) -> String {
    if iterations.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = iterations[0];
    let mut prev = iterations[0];
    for &n in &iterations[1..] {
        if n == prev + 1 {
            prev = n;
            continue;
        }
        parts.push(if start == prev {
            start.to_string()
        } else {
            format!("{}..{}", start, prev)
        });
        start = n;
        prev = n;
    }
    parts.push(if start == prev {
        start.to_string()
    } else {
        format!("{}..{}", start, prev)
    });
    parts.join(",")
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
) -> Result<(Vec<TestFlakiness>, FailureAnalyses)> {
    let mut history: HashMap<TestId, Vec<bool>> = HashMap::new();
    let mut analyses: HashMap<TestId, FailureAnalysis> = HashMap::new();
    for (iter_index, run_id) in run_ids.iter().enumerate() {
        let iteration = (iter_index + 1) as u32;
        let run = repo.get_test_run(run_id)?;
        // The streaming subunit parser doesn't populate `concurrency` on the
        // in-memory run, so go through the repository's metadata table.
        let concurrency = repo
            .get_run_metadata(run_id)
            .ok()
            .and_then(|m| m.concurrency);
        for (test_id, result) in &run.results {
            let is_failure = result.status.is_failure();
            history.entry(test_id.clone()).or_default().push(is_failure);
            let entry = analyses.entry(test_id.clone()).or_default();
            entry.concurrency.record(concurrency, is_failure);
            if is_failure {
                let msg = failure_summary(result.message.as_deref(), result.details.as_deref());
                let details = result
                    .details
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                entry.failing_iterations.push(iteration);
                entry.failing_run_ids.push(run_id.clone());
                if let Some(slot) = entry.modes.iter_mut().find(|m| m.message == msg) {
                    slot.count += 1;
                    if slot.details.is_none() {
                        slot.details = details;
                    }
                } else {
                    entry.modes.push(FailureMode {
                        message: msg,
                        count: 1,
                        details,
                    });
                }
            }
        }
    }

    // Drop tests that consistently failed across the *entire* stress window —
    // those are broken, not flaky. A test that failed in every iteration it
    // was recorded in but was absent from others (e.g. crashed the runner,
    // sharding/ordering, filtered out) is itself suspicious and should still
    // be surfaced as flaky.
    let total_iterations = run_ids.len();
    history.retain(|_, statuses| statuses.iter().any(|f| !f) || statuses.len() < total_iterations);

    // min_runs = 1 here because the caller already controls how many
    // iterations to run; we want every test that appeared in our stress
    // window to be eligible.
    let summary = summarise_flakiness(history, 1);

    // Restrict the analyses to tests that ended up in the summary, and
    // sort each entry's modes most-frequent-first for predictable output.
    let flaky_ids: std::collections::HashSet<&TestId> =
        summary.iter().map(|f| &f.test_id).collect();
    analyses.retain(|id, _| flaky_ids.contains(id));
    for analysis in analyses.values_mut() {
        analysis.modes.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.message.cmp(&b.message))
        });
    }

    Ok((summary, analyses))
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
    fn summarise_run_ids_keeps_test_that_appeared_in_a_subset() {
        // Regression: a test that failed in just one of ten stress iterations
        // and didn't appear in the other nine (e.g. it crashed the runner,
        // got sharded out, or was conditionally skipped) used to be silently
        // dropped as "always fails", leaving the user with a misleading
        // "No flaky tests observed, but at least one iteration had failing
        // tests" summary. Such a test is itself suspicious and should still
        // be surfaced.
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        let mut run_ids: Vec<RunId> = Vec::new();
        for i in 0..10 {
            let results: Vec<(&str, TestStatus)> = if i == 4 {
                vec![("anchor", Success), ("ghost", Failure)]
            } else {
                vec![("anchor", Success)]
            };
            run_ids.push(insert_run(repo.as_mut(), &i.to_string(), &results));
        }

        let (summary, _analyses) = summarise_run_ids(repo.as_ref(), &run_ids).unwrap();
        let ghost = summary
            .iter()
            .find(|s| s.test_id.as_str() == "ghost")
            .expect("ghost should be reported as flaky despite appearing only once");
        assert_eq!(ghost.failures, 1);
        assert_eq!(ghost.runs, 1);
    }

    #[test]
    fn summarise_run_ids_still_drops_consistently_broken_test() {
        // Counter-check for the regression test above: a test that failed in
        // *every* iteration of the stress window is broken, not flaky, and
        // must still be dropped.
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::Failure;
        let run_ids: Vec<RunId> = (0..3)
            .map(|i| insert_run(repo.as_mut(), &i.to_string(), &[("broken", Failure)]))
            .collect();

        let (summary, _analyses) = summarise_run_ids(repo.as_ref(), &run_ids).unwrap();
        assert!(summary.is_empty(), "broken-in-every-iter must not be flaky");
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
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (3 iteration(s), runs 0..2):".to_string(),
                "  Flaky tests: 1".to_string(),
                "  fail/runs  flake%  test".to_string(),
                "     1/3    100.0%  flap".to_string(),
                "\nFailure analysis:".to_string(),
            ]
        );
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
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (2 iteration(s), runs 0..1):".to_string(),
                "  No flaky tests observed, but at least one iteration had failing tests \
                 (likely a consistently broken test rather than flakiness)."
                    .to_string(),
            ]
        );
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
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (2 iteration(s), runs 0..1):".to_string(),
                "  No flaky tests observed.".to_string(),
            ]
        );
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

        let (summary, analyses) = summarise_run_ids(repo.as_ref(), &[r0, r1, r2, r3]).unwrap();
        assert_eq!(summary.len(), 1);
        let analysis = analyses.get(&TestId::new("flap")).expect("flap analysis");
        let mode_pairs: Vec<(&str, u32)> = analysis
            .modes
            .iter()
            .map(|m| (m.message.as_str(), m.count))
            .collect();
        assert_eq!(
            mode_pairs,
            vec![("connection refused", 2), ("timeout after 5s", 1)]
        );
        assert_eq!(analysis.failing_iterations, vec![1, 3, 4]);
    }

    #[test]
    fn summarise_run_ids_keeps_representative_details() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        let r0 = insert_run_with_messages(
            repo.as_mut(),
            "0",
            &[("flap", Failure, Some("AssertionError: 1 != 2"))],
        );
        let r1 = insert_run_with_messages(repo.as_mut(), "1", &[("flap", Success, None)]);

        let (_summary, analyses) = summarise_run_ids(repo.as_ref(), &[r0, r1]).unwrap();
        let analysis = analyses.get(&TestId::new("flap")).expect("flap analysis");
        assert_eq!(analysis.modes.len(), 1);
        assert_eq!(
            analysis.modes[0].details.as_deref(),
            Some("AssertionError: 1 != 2")
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

        let (_summary, analyses) = summarise_run_ids(repo.as_ref(), &[r0, r1]).unwrap();
        let analysis = analyses.get(&TestId::new("flap")).expect("flap analysis");
        assert_eq!(analysis.modes.len(), 1);
        assert_eq!(analysis.modes[0].message, "(no message)");
    }

    #[test]
    fn summarise_run_ids_records_concurrency_per_iteration() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        let r0 = insert_run_with_messages(repo.as_mut(), "0", &[("flap", Success, None)]);
        let r1 = insert_run_with_messages(repo.as_mut(), "1", &[("flap", Failure, Some("race"))]);
        let r2 = insert_run_with_messages(repo.as_mut(), "2", &[("flap", Success, None)]);
        let r3 = insert_run_with_messages(repo.as_mut(), "3", &[("flap", Failure, Some("race"))]);

        // r0 + r2 ran serially, r1 + r3 ran with -j 8.
        for (id, concurrency) in [(&r0, 1), (&r2, 1), (&r1, 8), (&r3, 8)] {
            let metadata = crate::repository::RunMetadata {
                concurrency: Some(concurrency),
                ..Default::default()
            };
            repo.set_run_metadata(id, metadata).unwrap();
        }

        let (summary, analyses) = summarise_run_ids(repo.as_ref(), &[r0, r1, r2, r3]).unwrap();
        assert_eq!(summary.len(), 1);
        let analysis = analyses.get(&TestId::new("flap")).expect("flap analysis");
        assert_eq!(
            analysis.concurrency,
            ConcurrencyBreakdown {
                serial_failures: 0,
                serial_runs: 2,
                parallel_failures: 2,
                parallel_runs: 2,
                max_parallel_concurrency: Some(8),
                unknown_failures: 0,
                unknown_runs: 0,
            }
        );
    }

    fn analysis_with(modes: Vec<(&str, u32)>, failing_iters: Vec<u32>) -> FailureAnalysis {
        FailureAnalysis {
            failing_iterations: failing_iters.clone(),
            failing_run_ids: failing_iters
                .iter()
                .map(|i| RunId::new((i - 1).to_string()))
                .collect(),
            modes: modes
                .into_iter()
                .map(|(m, c)| FailureMode {
                    message: m.to_string(),
                    count: c,
                    details: None,
                })
                .collect(),
            concurrency: ConcurrencyBreakdown::default(),
        }
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
        let mut analyses = HashMap::new();
        analyses.insert(
            TestId::new("flap"),
            analysis_with(
                vec![("connection refused", 2), ("timeout after 5s", 1)],
                vec![1, 3, 4],
            ),
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
                analyses,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (4 iteration(s), runs 0..3):".to_string(),
                "  Flaky tests: 1".to_string(),
                "  fail/runs  flake%  test".to_string(),
                "     3/4     66.7%  flap".to_string(),
                "      [2x] connection refused".to_string(),
                "      [1x] timeout after 5s".to_string(),
                "\nFailure analysis:".to_string(),
                "\n* flap".to_string(),
                "    failure rate: 3/4 iteration(s) (75.0%)".to_string(),
                "    failed in iteration(s): 1,3..4".to_string(),
                "    failing run id(s): 0,2,3".to_string(),
                "    failure mode 1 of 2 (2x): connection refused".to_string(),
                "    failure mode 2 of 2 (1x): timeout after 5s".to_string(),
            ]
        );
    }

    #[test]
    fn report_includes_concurrency_verdict_when_available() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 4,
            failures: 2,
            transitions: 2,
            flakiness_score: 2.0 / 3.0,
            failure_rate: 0.5,
        }];
        let mut analysis = analysis_with(vec![("race", 2)], vec![3, 4]);
        analysis.concurrency.record(Some(1), false);
        analysis.concurrency.record(Some(1), false);
        analysis.concurrency.record(Some(8), true);
        analysis.concurrency.record(Some(8), true);
        let mut analyses = HashMap::new();
        analyses.insert(TestId::new("flap"), analysis);

        cmd.report(
            &mut ui,
            &[
                RunId::new("0"),
                RunId::new("1"),
                RunId::new("2"),
                RunId::new("3"),
            ],
            flaky,
            analyses,
            true,
        )
        .unwrap();
        let concurrency_lines: Vec<&String> = ui
            .output
            .iter()
            .filter(|l| l.starts_with("    concurrency:"))
            .collect();
        assert_eq!(
            concurrency_lines,
            vec![
                &"    concurrency: fails 2/2 in parallel (max -j 8), 0/2 serially — likely concurrency-related".to_string(),
            ]
        );
    }

    #[test]
    fn report_omits_concurrency_line_with_no_observations() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 1,
            failures: 1,
            transitions: 0,
            flakiness_score: 0.0,
            failure_rate: 1.0,
        }];
        let mut analyses = HashMap::new();
        analyses.insert(
            TestId::new("flap"),
            analysis_with(vec![("boom", 1)], vec![1]),
        );

        cmd.report(&mut ui, &[RunId::new("0")], flaky, analyses, true)
            .unwrap();
        assert!(
            ui.output.iter().all(|l| !l.starts_with("    concurrency:")),
            "got: {:?}",
            ui.output
        );
    }

    #[test]
    fn report_flags_test_that_didnt_run_in_every_iteration() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("ghost"),
            runs: 1,
            failures: 1,
            transitions: 0,
            flakiness_score: 0.0,
            failure_rate: 1.0,
        }];
        let mut analyses = HashMap::new();
        analyses.insert(
            TestId::new("ghost"),
            analysis_with(vec![("boom", 1)], vec![5]),
        );

        let run_ids: Vec<RunId> = (0..10).map(|i| RunId::new(i.to_string())).collect();
        cmd.report(&mut ui, &run_ids, flaky, analyses, true)
            .unwrap();
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (10 iteration(s), runs 0..9):".to_string(),
                "  Flaky tests: 1".to_string(),
                "  fail/runs  flake%  test".to_string(),
                "     1/1      0.0%  ghost".to_string(),
                "      [1x] boom".to_string(),
                "\nFailure analysis:".to_string(),
                "\n* ghost".to_string(),
                "    failure rate: 1/1 iteration(s) (100.0%)".to_string(),
                "    note: only appeared in 1 of 10 stress iteration(s) — \
                 missing iterations may indicate sharding, ordering, or a \
                 prior crash that prevented this test from running."
                    .to_string(),
                "    failed in iteration(s): 5".to_string(),
                "    failing run id(s): 4".to_string(),
                "    failure mode 1 of 1 (1x): boom".to_string(),
            ]
        );
    }

    #[test]
    fn report_renders_details_block_for_each_failure_mode() {
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
        let mut analyses = HashMap::new();
        analyses.insert(
            TestId::new("flap"),
            FailureAnalysis {
                failing_iterations: vec![2],
                failing_run_ids: vec![RunId::new("1")],
                modes: vec![FailureMode {
                    message: "AssertionError: 1 != 2".to_string(),
                    count: 1,
                    details: Some(
                        "Traceback (most recent call last):\n  File \"x.py\", line 1\n    assert 1 == 2\nAssertionError: 1 != 2"
                            .to_string(),
                    ),
                }],
                concurrency: ConcurrencyBreakdown::default(),
            },
        );

        let code = cmd
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1"), RunId::new("2")],
                flaky,
                analyses,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        assert_eq!(
            ui.output,
            vec![
                "\nStress summary (3 iteration(s), runs 0..2):".to_string(),
                "  Flaky tests: 1".to_string(),
                "  fail/runs  flake%  test".to_string(),
                "     1/3    100.0%  flap".to_string(),
                "      [1x] AssertionError: 1 != 2".to_string(),
                "\nFailure analysis:".to_string(),
                "\n* flap".to_string(),
                "    failure rate: 1/3 iteration(s) (33.3%)".to_string(),
                "    failed in iteration(s): 2".to_string(),
                "    failing run id(s): 1".to_string(),
                "    failure mode 1 of 1 (1x): AssertionError: 1 != 2".to_string(),
                "        | Traceback (most recent call last):".to_string(),
                "        |   File \"x.py\", line 1".to_string(),
                "        |     assert 1 == 2".to_string(),
                "        | AssertionError: 1 != 2".to_string(),
            ]
        );
    }

    #[test]
    fn report_truncates_long_details_block() {
        let cmd = StressCommand::new(None);
        let mut ui = TestUI::new();
        let flaky = vec![TestFlakiness {
            test_id: TestId::new("flap"),
            runs: 2,
            failures: 1,
            transitions: 1,
            flakiness_score: 1.0,
            failure_rate: 0.5,
        }];
        let mut analyses = HashMap::new();
        let traceback = (0..MAX_DETAILS_LINES + 5)
            .map(|i| format!("line-{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        analyses.insert(
            TestId::new("flap"),
            FailureAnalysis {
                failing_iterations: vec![1],
                failing_run_ids: vec![RunId::new("0")],
                modes: vec![FailureMode {
                    message: "boom".to_string(),
                    count: 1,
                    details: Some(traceback),
                }],
                concurrency: ConcurrencyBreakdown::default(),
            },
        );

        cmd.report(
            &mut ui,
            &[RunId::new("0"), RunId::new("1")],
            flaky,
            analyses,
            true,
        )
        .unwrap();
        assert_eq!(
            *ui.output.last().unwrap(),
            "        | ... (5 more line(s) truncated; see `inq log <run-id>` for the full output)"
                .to_string()
        );
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
        let mut analyses = HashMap::new();
        let modes: Vec<FailureMode> = (0..MAX_MESSAGES_PER_TEST + 2)
            .map(|i| FailureMode {
                message: format!("msg-{}", i),
                count: 1,
                details: None,
            })
            .collect();
        analyses.insert(
            TestId::new("flap"),
            FailureAnalysis {
                failing_iterations: (1..=modes.len() as u32).collect(),
                failing_run_ids: (0..modes.len() as u32)
                    .map(|i| RunId::new(i.to_string()))
                    .collect(),
                modes,
                concurrency: ConcurrencyBreakdown::default(),
            },
        );

        let code = cmd
            .report(
                &mut ui,
                &[RunId::new("0"), RunId::new("1")],
                flaky,
                analyses,
                true,
            )
            .unwrap();
        assert_eq!(code, 1);
        // The table-only "... and N other distinct message(s)" line is the
        // index entry just before the per-test analysis section begins.
        let table_truncation_idx = ui
            .output
            .iter()
            .position(|l| l == "      ... and 2 other distinct message(s)")
            .expect("expected truncation line in table");
        assert_eq!(
            ui.output[table_truncation_idx + 1],
            "\nFailure analysis:".to_string()
        );
    }

    #[test]
    fn format_iteration_list_collapses_ranges() {
        assert_eq!(format_iteration_list(&[1]), "1");
        assert_eq!(format_iteration_list(&[1, 2, 3]), "1..3");
        assert_eq!(format_iteration_list(&[1, 3, 4, 5, 8]), "1,3..5,8");
        assert_eq!(format_iteration_list(&[]), "");
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
