//! `inq ci` - run tests with output formatted for a CI provider.
//!
//! Runs the test executor directly (no `RunCommand` wrap) with CI-friendly
//! defaults: smart ordering, opt-in flake retries, and provider-specific
//! output. Autodetects GitHub Actions, GitLab CI, Forgejo Actions (Codeberg),
//! and Woodpecker CI from environment variables; each gets only the features
//! its runner actually renders:
//!
//! - **GitHub**: streaming `::error::` / `::group::` annotations as failures
//!   land in the workflow log, a markdown job summary at
//!   `$GITHUB_STEP_SUMMARY`, and step outputs written to `$GITHUB_OUTPUT`.
//! - **GitLab**: collapsible `section_start` / `section_end` ANSI markers,
//!   `::error::`/`::warning::` annotations.
//! - **Forgejo**: no workflow-command markup (Forgejo's runner doesn't render
//!   it), but `$GITHUB_OUTPUT` writes go through the runner's compatibility
//!   mirror so downstream steps still see the counters.
//! - **Woodpecker**: plain output. Provider integration happens exclusively
//!   via the explicit `--junit-path` / `--dotenv-path` artifacts.
//!
//! To persist results across runs on GitHub Actions, restore the `.inquest`
//! directory from cache before this step and save it after:
//!
//! ```yaml
//! - uses: actions/cache@v4
//!   with:
//!     path: .inquest
//!     key: inquest-${{ github.run_id }}
//!     restore-keys: inquest-
//! - run: inq ci
//! ```
//!
//! With cached history present, the default ordering surfaces historically
//! failing tests first so a fresh regression fails the run quickly.

use crate::commands::export::{escape_data, escape_param, export_github, extract_source_location};
use crate::commands::run::compute_failure_counts;
use crate::commands::utils::{open_or_init_repository, open_repository, persist_and_display_run};
use crate::commands::Command;
use crate::config::TimeoutSetting;
use crate::error::Result;
use crate::ordering::{apply_order, OrderingContext, TestOrder};
use crate::repository::{RunId, TestId, TestResult, TestRun, TestStatus};
use crate::test_executor::{self, CancellationToken, TestExecutor, TestExecutorConfig};
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Which CI provider's annotations to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiFormat {
    /// Detect from environment variables; falls back to `Plain`.
    Auto,
    /// GitHub Actions workflow commands plus a markdown job summary when
    /// `$GITHUB_STEP_SUMMARY` is set.
    Github,
    /// GitLab CI workflow commands and collapsible-section ANSI markers.
    Gitlab,
    /// Forgejo Actions (Codeberg). Forgejo runs workflows but does not
    /// render GitHub's `::error::` / `::group::` workflow commands or
    /// `$GITHUB_STEP_SUMMARY`, so we emit plain text with no markers. It
    /// does honour `$GITHUB_OUTPUT` (mirrored from `$FORGEJO_OUTPUT`), so
    /// step outputs still get passed to downstream jobs.
    Forgejo,
    /// Woodpecker CI. No workflow-command or step-summary surfaces; output
    /// stays plain. Step outputs go through explicit `--dotenv-path` /
    /// `--junit-path` artifacts only.
    Woodpecker,
    /// Human-readable output with no provider-specific markers.
    Plain,
}

impl std::str::FromStr for CiFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(CiFormat::Auto),
            "github" => Ok(CiFormat::Github),
            "gitlab" => Ok(CiFormat::Gitlab),
            "forgejo" | "codeberg" | "gitea" => Ok(CiFormat::Forgejo),
            "woodpecker" => Ok(CiFormat::Woodpecker),
            "plain" | "none" => Ok(CiFormat::Plain),
            other => Err(format!(
                "unknown ci format '{}': expected auto, github, gitlab, forgejo, woodpecker, or plain",
                other
            )),
        }
    }
}

impl CiFormat {
    /// Resolve `Auto` against the current environment. Concrete formats are
    /// returned unchanged. Forgejo is checked before GitHub because Forgejo's
    /// runner sets both `FORGEJO_ACTIONS` and `GITHUB_ACTIONS` for
    /// GitHub-Actions compatibility — if we matched on `GITHUB_ACTIONS`
    /// first we'd misclassify every Forgejo job.
    fn resolve(self, env: &dyn EnvLookup) -> CiFormat {
        match self {
            CiFormat::Auto => {
                if env.get("FORGEJO_ACTIONS").as_deref() == Some("true") {
                    CiFormat::Forgejo
                } else if env.get("GITHUB_ACTIONS").as_deref() == Some("true") {
                    CiFormat::Github
                } else if env.get("GITLAB_CI").as_deref() == Some("true") {
                    CiFormat::Gitlab
                } else if env.get("CI").as_deref() == Some("woodpecker") {
                    CiFormat::Woodpecker
                } else {
                    CiFormat::Plain
                }
            }
            other => other,
        }
    }
}

/// Indirection over `std::env::var` so tests can inject a fake environment
/// without mutating process-global state.
pub(crate) trait EnvLookup {
    fn get(&self, key: &str) -> Option<String>;
}

struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Command for `inq ci`.
pub struct CiCommand {
    /// Repository base path.
    pub base_path: Option<String>,
    /// CI provider to format for.
    pub format: CiFormat,
    /// Number of retry passes for failing tests. `0` disables retries.
    pub retries: usize,
    /// Explicit ordering, overrides the CI default.
    pub order: Option<TestOrder>,
    /// Test ID filters forwarded to the runner.
    pub test_filters: Vec<String>,
    /// Tests to start with, forwarded to the runner.
    pub starting_with: Vec<String>,
    /// Worker concurrency override.
    pub concurrency: Option<usize>,
    /// Per-test timeout override.
    pub test_timeout: TimeoutSetting,
    /// Overall run timeout override.
    pub max_duration: TimeoutSetting,
    /// Extra arguments forwarded to the test command after `--`.
    pub test_args: Vec<String>,
    /// Active profile name from `--profile` / `INQ_PROFILE`.
    pub profile: Option<String>,
    /// If set, write a JUnit XML report to this path after the initial run.
    /// GitLab picks it up via `artifacts:reports:junit:` and shows test
    /// failures inline on the MR; most other CI systems consume JUnit too.
    /// Requires the `junit` cargo feature (enabled by default).
    pub junit_path: Option<std::path::PathBuf>,
    /// If set, write `key=value` lines (the same set `$GITHUB_OUTPUT` gets) to
    /// this path. GitLab picks it up via `artifacts:reports:dotenv:` and
    /// exposes the values as env vars in downstream jobs. `$GITHUB_OUTPUT`,
    /// when set, is always written in addition to this — the two are
    /// independent integration points.
    pub dotenv_path: Option<std::path::PathBuf>,
    /// Stop the initial run after this many failures. `None` disables the
    /// limit. With parallel workers, a few tests already in flight may still
    /// complete after the threshold is hit, so the recorded failure count can
    /// slightly exceed the limit — it's a stop-soon hint, not a hard cap.
    /// Retries (`--retry`) are not affected by this limit; they only re-run
    /// tests that failed initially.
    pub max_failures: Option<usize>,
    /// Niceness increment applied to spawned test processes (unix only).
    pub nice: Option<i32>,
}

impl CiCommand {
    /// Create a `CiCommand` with sensible CI defaults.
    pub fn new(base_path: Option<String>) -> Self {
        CiCommand {
            base_path,
            format: CiFormat::Auto,
            retries: 0,
            order: None,
            test_filters: Vec::new(),
            starting_with: Vec::new(),
            concurrency: None,
            test_timeout: TimeoutSetting::default(),
            max_duration: TimeoutSetting::default(),
            test_args: Vec::new(),
            profile: None,
            junit_path: None,
            dotenv_path: None,
            max_failures: None,
            nice: None,
        }
    }
}

impl Command for CiCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        self.run_with_env(ui, &ProcessEnv)
    }

    fn name(&self) -> &str {
        "ci"
    }

    fn help(&self) -> &str {
        "Run tests with output formatted for a CI provider (GitHub Actions, GitLab CI)"
    }
}

impl CiCommand {
    /// Test-friendly entry point that takes an explicit environment lookup.
    pub(crate) fn run_with_env(&self, ui: &mut dyn UI, env: &dyn EnvLookup) -> Result<i32> {
        let format = self.format.resolve(env);
        let streaming = matches!(format, CiFormat::Github | CiFormat::Gitlab);

        let order = match self.order.clone() {
            Some(o) => o,
            None => default_ci_order(self.base_path.as_deref()),
        };

        // Stream `::group::`/`::error::` annotations as failures land. We only
        // emit `::error::` when retries are disabled — with retries, a test
        // that fails initially might pass on retry and become flaky, in which
        // case it should show as a `::warning::` instead. The retries-enabled
        // path emits annotations post-hoc to avoid that double-flag.
        let streamed_ids: Option<Arc<Mutex<HashSet<TestId>>>> = if streaming {
            Some(Arc::new(Mutex::new(HashSet::new())))
        } else {
            None
        };
        let emit_errors_now = streaming && self.retries == 0;

        // When `--max-failures` is set, the executor needs a cancellation token
        // it can poll between test batches; the result callback flips it on
        // once enough failures have been observed. We always allocate the token
        // (it's cheap) so the callback can be wired uniformly.
        let cancel_token = CancellationToken::new();
        let failure_counter = Arc::new(AtomicUsize::new(0));
        let result_callback = self.build_result_callback(
            streamed_ids.clone(),
            emit_errors_now,
            format,
            failure_counter.clone(),
            cancel_token.clone(),
        );

        let total_start = std::time::Instant::now();
        let initial_run_id =
            match self.run_initial(ui, order.clone(), result_callback, cancel_token)? {
                RunOnceOutcome::Run(id) => id,
                RunOnceOutcome::EarlyExit(code) => return Ok(code),
            };

        let initial_failures = collect_failures(self.base_path.as_deref(), &initial_run_id)?;

        let mut still_failing: HashSet<TestId> = initial_failures.iter().cloned().collect();
        if self.retries > 0 && !still_failing.is_empty() {
            for _ in 0..self.retries {
                if still_failing.is_empty() {
                    break;
                }
                if let RunOnceOutcome::Run(rid) = self.run_retry(ui, order.clone())? {
                    let recovered =
                        recovered_tests(self.base_path.as_deref(), &rid, &still_failing)?;
                    for t in recovered {
                        still_failing.remove(&t);
                    }
                }
            }
        }

        // Flakes = initially failing but eventually passed on retry.
        let flaky: Vec<TestId> = initial_failures
            .iter()
            .filter(|t| !still_failing.contains(*t))
            .cloned()
            .collect();

        // Re-read the initial run so the summary/annotations reflect its details.
        let initial_run = {
            let repo = open_repository(self.base_path.as_deref())?;
            repo.get_test_run(&initial_run_id)?
        };

        // Annotations already streamed during the run get suppressed here so
        // we don't double-emit. `streamed_ids` is `None` when streaming was
        // off, which means emit everything.
        let already_streamed = streamed_ids
            .as_ref()
            .map(|s| s.lock().map(|g| g.clone()).unwrap_or_default());
        // Recent-runs window for the per-test failure-history hints in the
        // step summary. 10 is a sweet spot: long enough to distinguish a
        // chronic flake from a one-off, short enough that the read is cheap
        // and reviewers don't have to discount stale incidents from months ago.
        let history = collect_test_history(self.base_path.as_deref(), &initial_run_id, 10);
        let baseline_duration =
            collect_recent_duration_avg(self.base_path.as_deref(), &initial_run_id, 10);
        emit_ci_output(
            ui,
            &initial_run,
            &flaky,
            format,
            env,
            already_streamed.as_ref(),
            emit_errors_now,
            &initial_run_id,
            total_start.elapsed(),
            &history,
            baseline_duration,
        )?;

        if let Some(path) = &self.junit_path {
            write_junit_report(&initial_run, path)?;
        }
        if let Some(path) = &self.dotenv_path {
            write_dotenv(
                &initial_run,
                &flaky,
                &initial_run_id,
                total_start.elapsed(),
                path,
            )?;
        }

        Ok(if still_failing.is_empty() { 0 } else { 1 })
    }

    /// Build the per-result callback handed to the executor. Records the id
    /// of each failure it streams so the post-run code can avoid re-emitting,
    /// optionally writes `::group::`/`::error::` annotations immediately, and
    /// — when `max_failures` is set — trips `cancel_token` once enough
    /// failures have been observed so the executor stops dispatching new
    /// tests.
    fn build_result_callback(
        &self,
        streamed_ids: Option<Arc<Mutex<HashSet<TestId>>>>,
        emit_errors_now: bool,
        format: CiFormat,
        failure_counter: Arc<AtomicUsize>,
        cancel_token: CancellationToken,
    ) -> Option<test_executor::ResultCallback> {
        // Without either a streaming surface or a failure cap, the callback
        // has nothing to do.
        if streamed_ids.is_none() && self.max_failures.is_none() {
            return None;
        }
        let max_failures = self.max_failures;
        Some(Arc::new(move |result: &TestResult| {
            if !is_failure(result.status) {
                return;
            }

            if let Some(ref ids) = streamed_ids {
                let mut out = String::new();
                out.push_str(&format_group_block(result, format));
                if emit_errors_now {
                    out.push_str(&format_error_annotation(result));
                }
                // CI runners are non-TTY so progress bars are hidden; writing
                // directly to stdout works without progress-bar interference.
                let _ = std::io::stdout().write_all(out.as_bytes());
                let _ = std::io::stdout().flush();

                if let Ok(mut g) = ids.lock() {
                    g.insert(result.test_id.clone());
                }
            }

            if let Some(limit) = max_failures {
                // `fetch_add` returns the previous value, so `+ 1` is the new
                // count. Trip once when we cross the threshold; subsequent
                // failures from tests already in flight just re-cancel a
                // cancelled token, which is a no-op.
                let observed = failure_counter.fetch_add(1, Ordering::Relaxed) + 1;
                if observed >= limit {
                    cancel_token.cancel();
                }
            }
        }))
    }

    /// Initial run: auto-config if needed, run all tests in `order`. Mirrors
    /// the relevant slice of `RunCommand::execute_returning_run_id`.
    fn run_initial(
        &self,
        ui: &mut dyn UI,
        order: TestOrder,
        result_callback: Option<test_executor::ResultCallback>,
        cancel_token: CancellationToken,
    ) -> Result<RunOnceOutcome> {
        self.run_once(
            ui,
            order,
            result_callback,
            Some(cancel_token),
            /* failing_only */ false,
        )
    }

    /// Retry run: only previously failing tests, no streaming callback (the
    /// annotation set is decided after retries settle). The cancellation
    /// token is intentionally not threaded through — `--max-failures` caps
    /// the initial run, not retries, since retries only re-run the already
    /// failing set.
    fn run_retry(&self, ui: &mut dyn UI, order: TestOrder) -> Result<RunOnceOutcome> {
        self.run_once(ui, order, None, None, /* failing_only */ true)
    }

    fn run_once(
        &self,
        ui: &mut dyn UI,
        order: TestOrder,
        result_callback: Option<test_executor::ResultCallback>,
        cancel_token: Option<CancellationToken>,
        failing_only: bool,
    ) -> Result<RunOnceOutcome> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        // Auto-detect config so a fresh CI checkout works without setup. Only
        // run auto on the initial pass; retries reuse the now-present config.
        if !failing_only && crate::config::ConfigFile::find_in_directory(base).is_err() {
            let auto_cmd = crate::commands::auto::AutoCommand::new(self.base_path.clone());
            let exit_code = auto_cmd.execute(ui)?;
            if exit_code != 0 {
                return Ok(RunOnceOutcome::EarlyExit(exit_code));
            }
        }

        let mut repo = open_or_init_repository(self.base_path.as_deref(), true, ui)?;

        let (config_file, _config_path) = crate::config::ConfigFile::find_in_directory(base)?;
        let (resolved, active_profile) = config_file.resolve(self.profile.as_deref())?;
        let test_cmd = TestCommand::new(resolved, base.to_path_buf());
        if let Some(ref name) = active_profile {
            ui.output(&format!("Using profile: {}", name))?;
        }

        let (test_timeout, max_duration, no_output_timeout) = test_executor::resolve_timeouts(
            &self.test_timeout,
            &self.max_duration,
            None,
            &test_cmd,
        )?;

        let mut test_ids: Option<Vec<TestId>> = if failing_only {
            let failing = repo.get_failing_tests()?;
            if failing.is_empty() {
                ui.output("No failing tests to run")?;
                return Ok(RunOnceOutcome::EarlyExit(0));
            }
            Some(failing)
        } else {
            None
        };

        if !self.test_filters.is_empty() {
            use regex::Regex;
            let compiled: Result<Vec<Regex>> = self
                .test_filters
                .iter()
                .map(|pattern| {
                    Regex::new(pattern).map_err(|e| {
                        crate::error::Error::Config(format!(
                            "Invalid test filter regex '{}': {}",
                            pattern, e
                        ))
                    })
                })
                .collect();
            let compiled = compiled?;
            let all = match test_ids {
                Some(ids) => ids,
                None => test_cmd.list_tests()?,
            };
            test_ids = Some(
                all.into_iter()
                    .filter(|id| compiled.iter().any(|re| re.is_match(id.as_str())))
                    .collect(),
            );
        }

        if !self.starting_with.is_empty() {
            let all = match test_ids {
                Some(ids) => ids,
                None => test_cmd.list_tests()?,
            };
            let known: Vec<&str> = all.iter().map(|id| id.as_str()).collect();
            let mut prefixes = Vec::with_capacity(self.starting_with.len());
            for s in &self.starting_with {
                let expanded = crate::abbreviation::expand_abbreviation(s, &known)?;
                if expanded != *s {
                    ui.output(&format!("Expanded '{}' to '{}'", s, expanded))?;
                }
                prefixes.push(expanded);
            }
            test_ids = Some(
                all.into_iter()
                    .filter(|id| {
                        let s = id.as_str();
                        prefixes.iter().any(|p| {
                            s == p
                                || s.starts_with(p)
                                    && s.as_bytes().get(p.len()).copied() == Some(b'.')
                        })
                    })
                    .collect(),
            );
        }

        let historical_times = repo.get_test_times().unwrap_or_default();
        let max_duration_value =
            test_executor::compute_max_duration(&max_duration, &historical_times);
        let test_timeout_fn =
            test_executor::build_test_timeout_fn(&test_timeout, &historical_times);

        // Materialise the test list and apply the chosen order. CI always
        // wants explicit ordering applied so the FrequentFailingFirst default
        // surfaces known-bad tests early.
        let mut resolved_order = order;
        let mut failure_counts = std::collections::HashMap::new();
        if resolved_order == TestOrder::Auto {
            failure_counts = compute_failure_counts(repo.as_ref());
            resolved_order = crate::ordering::resolve_auto(&failure_counts);
            ui.output(&format!(
                "Auto-selected test order: {}",
                resolved_order.as_str()
            ))?;
        } else if resolved_order == TestOrder::FrequentFailingFirst {
            failure_counts = compute_failure_counts(repo.as_ref());
        }

        if resolved_order != TestOrder::Discovery {
            let materialised = match test_ids.take() {
                Some(ids) => ids,
                None => test_cmd.list_tests()?,
            };
            let failing_for_order =
                if matches!(resolved_order, TestOrder::FailingFirst) && !failing_only {
                    repo.get_failing_tests().unwrap_or_default()
                } else {
                    Vec::new()
                };
            let ctx = OrderingContext {
                failing_tests: &failing_for_order,
                historical_times: &historical_times,
                failure_counts: &failure_counts,
                group_regex: test_cmd.config().group_regex.as_deref(),
            };
            test_ids = Some(apply_order(materialised, &resolved_order, &ctx)?);
        }

        let stderr_capture = Arc::new(Mutex::new(Vec::new()));
        let display_prefix = test_executor::display_prefix_for(test_ids.as_deref(), &test_cmd);
        let config = TestExecutorConfig {
            base_path: self.base_path.clone(),
            all_output: false,
            test_args: if self.test_args.is_empty() {
                None
            } else {
                Some(self.test_args.clone())
            },
            cancellation_token: cancel_token,
            max_restarts: None,
            stderr_capture: Some(stderr_capture.clone()),
            result_callback,
            display_prefix,
            nice: self.nice,
        };
        let executor = TestExecutor::new(&config);

        let (concurrency, _src) = test_cmd.resolve_concurrency(self.concurrency)?;
        let output = if concurrency > 1 {
            let run_id = repo.get_next_run_id()?;
            executor.run_parallel(
                ui,
                &test_cmd,
                test_ids.as_deref(),
                concurrency,
                max_duration_value,
                no_output_timeout,
                test_timeout_fn.as_ref(),
                run_id,
                &historical_times,
                1.0,
                || repo.begin_test_run_raw().map(|(_, w)| w),
            )?
        } else {
            let (run_id, writer) = repo.begin_test_run_raw()?;
            executor.run_serial(
                ui,
                &test_cmd,
                test_ids.as_deref(),
                max_duration_value,
                no_output_timeout,
                test_timeout_fn.as_ref(),
                run_id,
                writer,
                &historical_times,
                1.0,
            )?
        };

        let (_exit, run_id) = persist_and_display_run(
            ui,
            repo.as_mut(),
            output,
            failing_only,
            &historical_times,
            &[],
            active_profile,
            false,
            false,
        )?;

        let bytes = match stderr_capture.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(p) => std::mem::take(&mut *p.into_inner()),
        };
        repo.set_run_stderr(&run_id, &bytes)?;

        Ok(RunOnceOutcome::Run(run_id))
    }
}

/// Outcome of `run_once`: a completed run with an id, or a short-circuit exit
/// before any tests were dispatched (e.g. auto-detect failed, or there were
/// no failing tests to retry).
enum RunOnceOutcome {
    Run(RunId),
    EarlyExit(i32),
}

/// Default ordering for `inq ci`: surface known-bad tests first if the
/// repository has any failure history, otherwise fall back to discovery
/// order (the cheap, deterministic default).
fn default_ci_order(base_path: Option<&str>) -> TestOrder {
    let has_history = open_repository(base_path)
        .and_then(|repo| repo.count())
        .map(|n| n > 0)
        .unwrap_or(false);
    if has_history {
        TestOrder::FrequentFailingFirst
    } else {
        TestOrder::Discovery
    }
}

/// Read the failing-tests file for a run. We use the failing-tests file
/// rather than scanning `run.results` so we get the exact same set the rest
/// of inquest treats as "currently failing".
fn collect_failures(
    base_path: Option<&str>,
    _run_id: &crate::repository::RunId,
) -> Result<Vec<TestId>> {
    let repo = open_repository(base_path)?;
    repo.get_failing_tests()
}

/// Per-test failure history pulled from the repository's recent runs. Used
/// to annotate the GitHub step summary with "failed N of last M runs" hints
/// so reviewers can tell a fresh regression apart from a long-standing flake.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TestHistory {
    /// Number of times this test reached a failing status in the window.
    pub failures: usize,
    /// Total number of runs in the window that recorded a result for this
    /// test (whether passing or failing). `0` means we have no history for
    /// the test; the summary suppresses the hint in that case.
    pub runs_seen: usize,
}

/// Build per-test history from the last `window` runs *before* `current`,
/// keyed by test id. We exclude `current` so a test that fails in the
/// run we're summarizing doesn't double-count itself. Returns an empty
/// map on any repository error — history is a UX nicety, not load-bearing,
/// so failures here should never break the CI run.
pub(crate) fn collect_test_history(
    base_path: Option<&str>,
    current: &RunId,
    window: usize,
) -> std::collections::HashMap<TestId, TestHistory> {
    let mut out: std::collections::HashMap<TestId, TestHistory> = std::collections::HashMap::new();
    if window == 0 {
        return out;
    }
    let Ok(repo) = open_repository(base_path) else {
        return out;
    };
    let Ok(run_ids) = repo.list_run_ids() else {
        return out;
    };
    // `list_run_ids` is ascending; take the most recent `window` ids that
    // aren't the current run. `iter().rev()` walks newest-first; we stop
    // once we've collected `window` historical runs.
    let recent: Vec<&RunId> = run_ids
        .iter()
        .rev()
        .filter(|id| id.as_str() != current.as_str())
        .take(window)
        .collect();
    for run_id in recent {
        let Ok(run) = repo.get_test_run(run_id) else {
            continue;
        };
        for (test_id, result) in &run.results {
            let entry = out.entry(test_id.clone()).or_default();
            entry.runs_seen += 1;
            if is_failure(result.status) {
                entry.failures += 1;
            }
        }
    }
    out
}

/// Average wall-clock duration of the last `window` runs *before* `current`,
/// used for the "vs history" comparison in the step summary. Reads the
/// persisted per-run `duration_secs` metadata; runs without a recorded
/// duration (e.g. predating duration tracking) are ignored. Returns `None`
/// when there's no usable history, so the summary omits the comparison
/// rather than comparing against a meaningless zero.
pub(crate) fn collect_recent_duration_avg(
    base_path: Option<&str>,
    current: &RunId,
    window: usize,
) -> Option<Duration> {
    if window == 0 {
        return None;
    }
    let repo = open_repository(base_path).ok()?;
    let run_ids = repo.list_run_ids().ok()?;
    let recent = run_ids
        .iter()
        .rev()
        .filter(|id| id.as_str() != current.as_str())
        .take(window);
    let mut total = 0.0;
    let mut count = 0usize;
    for run_id in recent {
        let Ok(meta) = repo.get_run_metadata(run_id) else {
            continue;
        };
        if let Some(secs) = meta.duration_secs {
            total += secs;
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    Some(Duration::from_secs_f64(total / count as f64))
}

/// Subset of `candidates` that the run identified by `run_id` reports as
/// passing (so they were "recovered" by the retry).
fn recovered_tests(
    base_path: Option<&str>,
    run_id: &crate::repository::RunId,
    candidates: &HashSet<TestId>,
) -> Result<Vec<TestId>> {
    let repo = open_repository(base_path)?;
    let run = repo.get_test_run(run_id)?;
    let mut out = Vec::new();
    for (test_id, result) in &run.results {
        if candidates.contains(test_id) && result.status == TestStatus::Success {
            out.push(test_id.clone());
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn emit_ci_output(
    ui: &mut dyn UI,
    run: &TestRun,
    flaky: &[TestId],
    format: CiFormat,
    env: &dyn EnvLookup,
    already_streamed: Option<&HashSet<TestId>>,
    streamed_errors_already: bool,
    run_id: &RunId,
    duration: Duration,
    history: &std::collections::HashMap<TestId, TestHistory>,
    baseline_duration: Option<Duration>,
) -> Result<()> {
    match format {
        // Plain and Auto stay silent: the test runner already printed each
        // failure, and there are no provider-specific markers to add.
        CiFormat::Plain | CiFormat::Auto => Ok(()),

        // Woodpecker has no log-markup or env-driven output surfaces;
        // anything provider-specific would just be noise in the log. Users
        // rely on `--junit-path` / `--dotenv-path` for artifacts.
        CiFormat::Woodpecker => Ok(()),

        // Forgejo Actions doesn't render workflow commands or step
        // summaries, but it does honour `$GITHUB_OUTPUT` (mirrored from
        // `$FORGEJO_OUTPUT`), so downstream steps can still read the
        // run's pass/fail/flaky counters.
        CiFormat::Forgejo => {
            write_github_output(env, run, flaky, run_id, duration)?;
            Ok(())
        }

        CiFormat::Github | CiFormat::Gitlab => {
            let mut out = String::new();

            // Per-failing-test log groups (foldable in the workflow log).
            // Skip any id streamed live during the run.
            out.push_str(&format_failure_groups(run, already_streamed, format));

            // Warning annotations for recovered (flaky) tests. Always
            // emitted post-hoc: we only know which tests recovered after
            // retries settle.
            out.push_str(&format_flaky_warnings(run, flaky));

            // Error annotations for tests still failing. Skipped entirely
            // when the streaming callback already wrote them inline.
            if !streamed_errors_already {
                let still_failing: HashSet<&TestId> = run
                    .results
                    .iter()
                    .filter(|(id, r)| is_failure(r.status) && !flaky.iter().any(|f| f == *id))
                    .map(|(id, _)| id)
                    .collect();
                out.push_str(&filter_annotations(&export_github(run), &still_failing));
            }

            if !out.is_empty() {
                ui.output(out.trim_end_matches('\n'))?;
            }

            // GitHub-only: markdown summary + step outputs.
            if format == CiFormat::Github {
                if let Some(path) = env.get("GITHUB_STEP_SUMMARY") {
                    let summary =
                        format_step_summary(run, flaky, history, duration, baseline_duration);
                    std::fs::write(&path, summary)?;
                }
                write_github_output(env, run, flaky, run_id, duration)?;
            }
            Ok(())
        }
    }
}

/// Render the standard `key=value` step-output block. Used by both
/// `$GITHUB_OUTPUT` (where GitHub Actions concatenates from multiple writers,
/// so append makes sense) and `--dotenv-path` (where GitLab's dotenv report
/// expects the same wire format).
fn format_step_outputs(
    run: &TestRun,
    flaky: &[TestId],
    run_id: &RunId,
    duration: Duration,
) -> String {
    let mut passed = 0usize;
    let mut failed = 0usize;
    for r in run.results.values() {
        if matches!(r.status, TestStatus::Success) {
            passed += 1;
        } else if is_failure(r.status) {
            failed += 1;
        }
    }
    // Treat flakes as "not failed" — the run as a whole stayed green.
    let hard_failures = failed.saturating_sub(flaky.len());

    let mut body = String::new();
    let _ = writeln!(body, "passed={}", passed);
    let _ = writeln!(body, "failed={}", hard_failures);
    let _ = writeln!(body, "flaky={}", flaky.len());
    let _ = writeln!(body, "duration={:.3}", duration.as_secs_f64());
    let _ = writeln!(body, "run_id={}", run_id.as_str());
    body
}

/// Write `key=value` lines to `$GITHUB_OUTPUT` so downstream workflow steps
/// can branch on the result. No-op when the env var is unset (i.e. not running
/// inside a GitHub Actions step).
fn write_github_output(
    env: &dyn EnvLookup,
    run: &TestRun,
    flaky: &[TestId],
    run_id: &RunId,
    duration: Duration,
) -> Result<()> {
    let Some(path) = env.get("GITHUB_OUTPUT") else {
        return Ok(());
    };

    let body = format_step_outputs(run, flaky, run_id, duration);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

/// Write the same key=value block to an explicit path, for GitLab's
/// `artifacts:reports:dotenv:` and any other CI that consumes dotenv files.
/// Truncates the target — unlike `$GITHUB_OUTPUT`, this path is owned by inq
/// for the duration of the job, so there's no concatenation from other steps
/// to preserve.
fn write_dotenv(
    run: &TestRun,
    flaky: &[TestId],
    run_id: &RunId,
    duration: Duration,
    path: &std::path::Path,
) -> Result<()> {
    let body = format_step_outputs(run, flaky, run_id, duration);
    std::fs::write(path, body)?;
    Ok(())
}

/// Write a JUnit XML report so GitLab (`artifacts:reports:junit:`) and other
/// JUnit-consuming CI integrations can surface test results inline on the
/// MR/PR view.
#[cfg(feature = "junit")]
fn write_junit_report(run: &TestRun, path: &std::path::Path) -> Result<()> {
    let xml = crate::commands::export::export_junit(run)?;
    std::fs::write(path, xml)?;
    Ok(())
}

/// Without the `junit` feature, `--junit-path` becomes a hard error: silently
/// ignoring it would let a CI workflow that depends on the artifact think it
/// was produced.
#[cfg(not(feature = "junit"))]
fn write_junit_report(_run: &TestRun, _path: &std::path::Path) -> Result<()> {
    Err(crate::error::Error::Config(
        "--junit-path requires the 'junit' cargo feature".to_string(),
    ))
}

fn is_failure(status: TestStatus) -> bool {
    matches!(
        status,
        TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess
    )
}

/// Emit one collapsible-section block per failing test, with the test's
/// `details` (traceback, captured output) as the body. Format is GitHub's
/// `::group::` / `::endgroup::` for `CiFormat::Github` and GitLab's
/// `section_start` / `section_end` ANSI sequences for `CiFormat::Gitlab`;
/// other formats fall back to the GitHub form. Skips any test whose id is in
/// `skip` (typically: already streamed live).
fn format_failure_groups(
    run: &TestRun,
    skip: Option<&HashSet<TestId>>,
    format: CiFormat,
) -> String {
    let mut out = String::new();
    let mut failures: Vec<_> = run
        .results
        .values()
        .filter(|r| is_failure(r.status))
        .filter(|r| skip.is_none_or(|s| !s.contains(&r.test_id)))
        .collect();
    failures.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));

    for result in failures {
        out.push_str(&format_group_block(result, format));
    }
    out
}

/// Collapsible block for a single result. GitLab's collapsible-section
/// protocol uses ANSI control codes around `section_start`/`section_end`
/// pseudo-events that the runner intercepts; everything else uses the
/// GitHub `::group::` workflow command.
fn format_group_block(result: &TestResult, format: CiFormat) -> String {
    format_group_block_with(result, format, gitlab_section_timestamp)
}

/// Live timestamp source used by `format_group_block` in production. Pulled
/// out as a free function so tests can substitute a deterministic clock via
/// `format_group_block_with`.
fn gitlab_section_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Like `format_group_block` but accepts an explicit timestamp source, so
/// tests can pin the GitLab section markers to a known value.
fn format_group_block_with(
    result: &TestResult,
    format: CiFormat,
    timestamp: fn() -> i64,
) -> String {
    let mut out = String::new();
    let test_id = result.test_id.as_str();
    let (open, close) = match format {
        CiFormat::Gitlab => {
            let name = sanitize_gitlab_section_name(test_id);
            // GitLab uses the timestamp only to derive the section's
            // displayed duration; it doesn't need to be wall-clock. We reuse
            // the same value for start/end here, which makes the section
            // show no duration — fine for failure-detail blocks.
            let ts = timestamp();
            (
                format!("\x1b[0Ksection_start:{ts}:{name}[collapsed=true]\r\x1b[0K{test_id}\n"),
                format!("\x1b[0Ksection_end:{ts}:{name}\r\x1b[0K\n"),
            )
        }
        _ => (
            format!("::group::{test_id}\n"),
            "::endgroup::\n".to_string(),
        ),
    };
    out.push_str(&open);
    if let Some(msg) = &result.message {
        let trimmed = msg.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    if let Some(details) = &result.details {
        let trimmed = details.trim_end();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.push_str(&close);
    out
}

/// GitLab section names must contain only ASCII letters, numbers, periods,
/// underscores, and hyphens (per the GitLab CI docs). Test ids commonly
/// contain `::`, `,`, `/`, and `[]`; replace any unsupported character with
/// `_` so the runner accepts the section. Names are not human-shown — they're
/// only used to match start/end pairs — so a lossy mapping is fine.
fn sanitize_gitlab_section_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Single `::error file=...,line=...,title=TEST_ID::message` annotation for a
/// failing test. Mirrors what `export_github` emits in bulk, so streamed and
/// batched annotations look identical to the workflow runner.
fn format_error_annotation(result: &TestResult) -> String {
    if !is_failure(result.status) {
        return String::new();
    }
    let location = result.details.as_deref().and_then(extract_source_location);
    let message = result
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.lines().next().unwrap_or(s).to_string())
        .unwrap_or_else(|| result.status.to_string());

    let mut params: Vec<String> = Vec::new();
    if let Some(loc) = &location {
        params.push(format!("file={}", escape_param(&loc.file)));
        params.push(format!("line={}", loc.line));
        if let Some(c) = loc.col {
            params.push(format!("col={}", c));
        }
    }
    params.push(format!("title={}", escape_param(result.test_id.as_str())));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "::error {}::{}",
        params.join(","),
        escape_data(&message)
    );
    out
}

/// Emit `::warning::` annotations for tests that failed initially but passed
/// on retry. CI stays green but the flake is visible in the PR diff.
fn format_flaky_warnings(run: &TestRun, flaky: &[TestId]) -> String {
    let mut out = String::new();
    let mut sorted: Vec<&TestId> = flaky.iter().collect();
    sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    for test_id in sorted {
        let original = run.results.get(test_id);
        let location = original
            .and_then(|r| r.details.as_deref())
            .and_then(extract_source_location);
        let message = original
            .and_then(|r| r.message.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.lines().next().unwrap_or(s).to_string())
            .unwrap_or_else(|| "passed on retry".to_string());

        let mut params: Vec<String> = Vec::new();
        if let Some(loc) = &location {
            params.push(format!("file={}", escape_param(&loc.file)));
            params.push(format!("line={}", loc.line));
            if let Some(c) = loc.col {
                params.push(format!("col={}", c));
            }
        }
        params.push(format!("title=Flaky: {}", escape_param(test_id.as_str())));
        let _ = writeln!(
            out,
            "::warning {}::{}",
            params.join(","),
            escape_data(&format!("{} (passed on retry)", message))
        );
    }
    out
}

/// Keep only `::error ... title=<id>::...` lines from `annotations` where
/// `<id>` is in `keep`. Filtering by title is unambiguous because
/// `export_github` URL-escapes commas in test IDs.
fn filter_annotations(annotations: &str, keep: &HashSet<&TestId>) -> String {
    let titles: HashSet<String> = keep.iter().map(|t| escape_param(t.as_str())).collect();
    annotations
        .lines()
        .filter(|line| {
            line.split(",")
                .find_map(|p| p.strip_prefix("title="))
                .map(|t| t.split("::").next().unwrap_or(t))
                .map(|t| titles.contains(t))
                .unwrap_or(false)
        })
        .fold(String::new(), |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        })
}

/// Append a "(failed N of last M)" hint when there's recorded history for the
/// test. New tests (no history) produce an empty string so we don't print a
/// misleading "0 of 0". The hint helps reviewers tell a fresh regression
/// (`0 of 10`) apart from a chronic flake (`8 of 10`) at a glance.
fn format_history_hint(
    test_id: &TestId,
    history: &std::collections::HashMap<TestId, TestHistory>,
) -> String {
    match history.get(test_id) {
        Some(h) if h.runs_seen > 0 => {
            format!(" (failed {} of last {})", h.failures, h.runs_seen)
        }
        _ => String::new(),
    }
}

/// Max lines of traceback embedded per test in the step summary.
const SUMMARY_DETAILS_MAX_LINES: usize = 20;
/// Max bytes of traceback embedded per test in the step summary. GitHub caps
/// the whole file at 1MiB, so we truncate aggressively per-entry.
const SUMMARY_DETAILS_MAX_BYTES: usize = 2048;

/// Truncate `details` to at most [`SUMMARY_DETAILS_MAX_LINES`] lines and
/// [`SUMMARY_DETAILS_MAX_BYTES`] bytes, appending a truncation marker when
/// either limit trims content.
fn truncate_details(details: &str) -> String {
    let mut truncated = false;
    let mut out = String::new();
    for (i, line) in details.lines().enumerate() {
        if i >= SUMMARY_DETAILS_MAX_LINES {
            truncated = true;
            break;
        }
        if out.len() + line.len() + 1 > SUMMARY_DETAILS_MAX_BYTES {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if truncated {
        out.push_str("... (truncated)\n");
    }
    out
}

/// Emit a nested `<details>` block containing `details` inside a fenced code
/// block. Callers embed this under a top-level test entry.
fn write_details_block(out: &mut String, details: &str) {
    let body = truncate_details(details);
    if body.is_empty() {
        return;
    }
    let _ = writeln!(out, "  <details><summary>traceback</summary>");
    let _ = writeln!(out);
    let _ = writeln!(out, "  ```");
    for line in body.lines() {
        let _ = writeln!(out, "  {}", line);
    }
    let _ = writeln!(out, "  ```");
    let _ = writeln!(out);
    let _ = writeln!(out, "  </details>");
}

/// Human-readable `1m2.3s` / `4.56s` / `789ms` rendering of a duration for
/// the step summary. Sub-second durations keep millisecond precision; longer
/// ones round to a tenth of a second, and anything past a minute splits into
/// `<m>m<s>s`.
fn format_duration_human(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{:.2}s", secs)
    } else {
        let whole = secs.round() as u64;
        format!("{}m{}s", whole / 60, whole % 60)
    }
}

/// The runtime line shown under the counts table: wall-clock of the step, the
/// summed per-test execution time (which exceeds wall-clock when tests run in
/// parallel), and a comparison against the recent-runs average when history
/// is available.
fn format_runtime_line(
    run: &TestRun,
    duration: Duration,
    baseline_duration: Option<Duration>,
) -> String {
    let mut line = format!("**Runtime:** {}", format_duration_human(duration));

    if let Some(baseline) = baseline_duration {
        let base = baseline.as_secs_f64();
        if base > 0.0 {
            let delta = (duration.as_secs_f64() - base) / base * 100.0;
            // Round to a whole percent; treat anything under 1% as flat so we
            // don't flag noise as a regression or improvement.
            let rounded = delta.round() as i64;
            let marker = if rounded > 0 {
                format!("up {}%", rounded)
            } else if rounded < 0 {
                format!("down {}%", -rounded)
            } else {
                "no change".to_string()
            };
            let _ = write!(
                line,
                " ({} vs avg of last runs, {})",
                marker,
                format_duration_human(baseline)
            );
        }
    }

    if let Some(test_time) = run.total_duration() {
        let _ = write!(
            line,
            "  \u{2014}  test time: {}",
            format_duration_human(test_time)
        );
    }

    line
}

/// Number of slowest tests listed in the step summary. Small enough to stay a
/// glanceable pointer at what to optimise, not a full profile.
const SUMMARY_SLOWEST_COUNT: usize = 5;

/// Append a collapsible block listing the slowest tests by execution time.
/// No-op when no result carries timing data (e.g. an adapter that doesn't
/// report per-test durations).
fn write_slowest_tests_block(out: &mut String, run: &TestRun) {
    let mut timed: Vec<(&TestId, Duration)> = run
        .results
        .values()
        .filter_map(|r| r.duration.map(|d| (&r.test_id, d)))
        .collect();
    if timed.is_empty() {
        return;
    }
    // Slowest first; break ties on test id so the output is deterministic.
    timed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));
    timed.truncate(SUMMARY_SLOWEST_COUNT);

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "<details><summary>Slowest tests ({})</summary>",
        timed.len()
    );
    let _ = writeln!(out);
    for (test_id, d) in timed {
        let _ = writeln!(
            out,
            "- `{}` — {}",
            test_id.as_str(),
            format_duration_human(d)
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "</details>");
}

/// Markdown summary written to `$GITHUB_STEP_SUMMARY`, which GitHub renders
/// on the workflow run page.
fn format_step_summary(
    run: &TestRun,
    flaky: &[TestId],
    history: &std::collections::HashMap<TestId, TestHistory>,
    duration: Duration,
    baseline_duration: Option<Duration>,
) -> String {
    let total = run.results.len();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errored = 0usize;
    let mut skipped = 0usize;
    for r in run.results.values() {
        match r.status {
            TestStatus::Success => passed += 1,
            TestStatus::Failure | TestStatus::UnexpectedSuccess => failed += 1,
            TestStatus::Error => errored += 1,
            TestStatus::Skip | TestStatus::ExpectedFailure => skipped += 1,
        }
    }
    let flaky_set: HashSet<&TestId> = flaky.iter().collect();

    let mut out = String::new();
    let _ = writeln!(out, "## Test results");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Total | Passed | Failed | Errored | Skipped | Flaky |"
    );
    let _ = writeln!(
        out,
        "|------:|-------:|-------:|--------:|--------:|------:|"
    );
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | {} |",
        total,
        passed,
        failed,
        errored,
        skipped,
        flaky.len()
    );

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        format_runtime_line(run, duration, baseline_duration)
    );

    write_slowest_tests_block(&mut out, run);

    let mut failures: Vec<_> = run
        .results
        .values()
        .filter(|r| is_failure(r.status) && !flaky_set.contains(&r.test_id))
        .collect();
    if !failures.is_empty() {
        failures.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "<details><summary>Failing tests ({})</summary>",
            failures.len()
        );
        let _ = writeln!(out);
        for r in failures {
            let msg = r
                .message
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.lines().next().unwrap_or(s).to_string())
                .unwrap_or_else(|| r.status.to_string());
            let _ = writeln!(
                out,
                "- `{}` — {}{}",
                r.test_id.as_str(),
                msg,
                format_history_hint(&r.test_id, history),
            );
            if let Some(d) = r
                .details
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                write_details_block(&mut out, d);
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "</details>");
    }

    if !flaky.is_empty() {
        let mut flaky_sorted: Vec<&TestId> = flaky.iter().collect();
        flaky_sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "<details><summary>Flaky tests ({})</summary>",
            flaky.len()
        );
        let _ = writeln!(out);
        for t in flaky_sorted {
            let _ = writeln!(
                out,
                "- `{}` (passed on retry){}",
                t.as_str(),
                format_history_hint(t, history),
            );
            if let Some(d) = run
                .results
                .get(t)
                .and_then(|r| r.details.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                write_details_block(&mut out, d);
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "</details>");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{RunId, TestResult};
    use std::collections::HashMap;

    struct FakeEnv(HashMap<String, String>);

    impl EnvLookup for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn env(pairs: &[(&str, &str)]) -> FakeEnv {
        FakeEnv(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    #[test]
    fn ci_format_resolve_detects_github() {
        let e = env(&[("GITHUB_ACTIONS", "true")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Github);
    }

    #[test]
    fn ci_format_resolve_detects_gitlab() {
        let e = env(&[("GITLAB_CI", "true")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Gitlab);
    }

    #[test]
    fn ci_format_resolve_falls_back_to_plain() {
        let e = env(&[]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Plain);
    }

    #[test]
    fn ci_format_resolve_passes_through_explicit() {
        let e = env(&[("GITHUB_ACTIONS", "true")]);
        assert_eq!(CiFormat::Plain.resolve(&e), CiFormat::Plain);
    }

    #[test]
    fn ci_format_resolve_detects_forgejo() {
        let e = env(&[("FORGEJO_ACTIONS", "true")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Forgejo);
    }

    #[test]
    fn ci_format_resolve_prefers_forgejo_over_github_when_both_set() {
        // Forgejo Actions sets BOTH `FORGEJO_ACTIONS` and `GITHUB_ACTIONS`
        // for GitHub-compatibility. We must classify as Forgejo so we don't
        // emit `::group::` / `::error::` markers that the Forgejo runner
        // would render as literal log text.
        let e = env(&[("FORGEJO_ACTIONS", "true"), ("GITHUB_ACTIONS", "true")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Forgejo);
    }

    #[test]
    fn ci_format_resolve_detects_woodpecker() {
        // Woodpecker uses `CI=woodpecker` rather than a dedicated flag.
        let e = env(&[("CI", "woodpecker")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Woodpecker);
    }

    #[test]
    fn ci_format_resolve_generic_ci_true_falls_back_to_plain() {
        // Plenty of providers set `CI=true` (the generic convention); we
        // only treat the `woodpecker` value as a positive identification.
        let e = env(&[("CI", "true")]);
        assert_eq!(CiFormat::Auto.resolve(&e), CiFormat::Plain);
    }

    #[test]
    fn ci_format_from_str() {
        assert_eq!("github".parse::<CiFormat>().unwrap(), CiFormat::Github);
        assert_eq!("AUTO".parse::<CiFormat>().unwrap(), CiFormat::Auto);
        assert_eq!("plain".parse::<CiFormat>().unwrap(), CiFormat::Plain);
        assert_eq!("forgejo".parse::<CiFormat>().unwrap(), CiFormat::Forgejo);
        // "codeberg" and "gitea" are accepted as aliases for the same
        // underlying runner (Codeberg uses Forgejo; Gitea Actions is the
        // upstream Forgejo Actions started from).
        assert_eq!("codeberg".parse::<CiFormat>().unwrap(), CiFormat::Forgejo);
        assert_eq!("gitea".parse::<CiFormat>().unwrap(), CiFormat::Forgejo);
        assert_eq!(
            "woodpecker".parse::<CiFormat>().unwrap(),
            CiFormat::Woodpecker
        );
        assert!("xml".parse::<CiFormat>().is_err());
    }

    fn make_run() -> TestRun {
        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        run.add_result(TestResult::success("tests.a"));
        run.add_result(
            TestResult::failure("tests.b", "boom")
                .with_details("File \"tests/b.py\", line 7, in test\n    raise AssertionError"),
        );
        run.add_result(TestResult::error("tests.c", "timeout"));
        run.add_result(TestResult::skip("tests.d"));
        run
    }

    #[test]
    fn failure_groups_emits_one_block_per_failure_for_github() {
        let run = make_run();
        let out = format_failure_groups(&run, None, CiFormat::Github);
        let expected = "::group::tests.b\n\
                        boom\n\
                        File \"tests/b.py\", line 7, in test\n    \
                        raise AssertionError\n\
                        ::endgroup::\n\
                        ::group::tests.c\n\
                        timeout\n\
                        ::endgroup::\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn flaky_warnings_emit_warning_annotations() {
        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        let out = format_flaky_warnings(&run, &flaky);
        assert_eq!(
            out,
            "::warning file=tests/b.py,line=7,title=Flaky: tests.b::boom (passed on retry)\n"
        );
    }

    #[test]
    fn flaky_warnings_empty_when_no_flakes() {
        let run = make_run();
        let out = format_flaky_warnings(&run, &[]);
        assert_eq!(out, "");
    }

    /// Traceback block that appears under `tests.b` in the make_run fixture:
    /// two indented lines wrapped in a `<details>`+code fence, matching what
    /// `write_details_block` emits.
    const TESTS_B_DETAILS: &str = concat!(
        "  <details><summary>traceback</summary>\n",
        "\n",
        "  ```\n",
        "  File \"tests/b.py\", line 7, in test\n",
        "      raise AssertionError\n",
        "  ```\n",
        "\n",
        "  </details>\n",
    );

    #[test]
    fn step_summary_counts_and_lists_failures() {
        let run = make_run();
        // No per-test durations in the fixture, so no "Slowest tests" block;
        // `None` baseline suppresses the vs-history comparison.
        let summary = format_step_summary(
            &run,
            &[],
            &HashMap::new(),
            Duration::from_secs_f64(2.5),
            None,
        );
        // total=4, passed=1, failed=1, errored=1, skipped=1, flaky=0
        let expected = format!(
            "## Test results\n\
             \n\
             | Total | Passed | Failed | Errored | Skipped | Flaky |\n\
             |------:|-------:|-------:|--------:|--------:|------:|\n\
             | 4 | 1 | 1 | 1 | 1 | 0 |\n\
             \n\
             **Runtime:** 2.50s\n\
             \n\
             <details><summary>Failing tests (2)</summary>\n\
             \n\
             - `tests.b` — boom\n\
             {TESTS_B_DETAILS}\
             - `tests.c` — timeout\n\
             \n\
             </details>\n"
        );
        assert_eq!(summary, expected);
    }

    #[test]
    fn step_summary_separates_flakes_from_hard_failures() {
        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        let summary = format_step_summary(
            &run,
            &flaky,
            &HashMap::new(),
            Duration::from_secs_f64(2.5),
            None,
        );
        // tests.b is now flaky, so failing list shows only tests.c, and
        // there's a separate flaky-tests block at the end.
        let expected = format!(
            "## Test results\n\
             \n\
             | Total | Passed | Failed | Errored | Skipped | Flaky |\n\
             |------:|-------:|-------:|--------:|--------:|------:|\n\
             | 4 | 1 | 1 | 1 | 1 | 1 |\n\
             \n\
             **Runtime:** 2.50s\n\
             \n\
             <details><summary>Failing tests (1)</summary>\n\
             \n\
             - `tests.c` — timeout\n\
             \n\
             </details>\n\
             \n\
             <details><summary>Flaky tests (1)</summary>\n\
             \n\
             - `tests.b` (passed on retry)\n\
             {TESTS_B_DETAILS}\
             \n\
             </details>\n"
        );
        assert_eq!(summary, expected);
    }

    #[test]
    fn format_duration_human_scales_by_magnitude() {
        assert_eq!(format_duration_human(Duration::from_millis(789)), "789ms");
        assert_eq!(
            format_duration_human(Duration::from_secs_f64(4.567)),
            "4.57s"
        );
        assert_eq!(format_duration_human(Duration::from_secs(62)), "1m2s");
    }

    #[test]
    fn runtime_line_reports_delta_vs_baseline() {
        let run = make_run();
        // 44s now against a 40s average is +10%. The fixture has no per-test
        // timing, so the "test time" tail is omitted.
        let line =
            format_runtime_line(&run, Duration::from_secs(44), Some(Duration::from_secs(40)));
        assert_eq!(
            line,
            "**Runtime:** 44.00s (up 10% vs avg of last runs, 40.00s)"
        );
    }

    #[test]
    fn runtime_line_reports_improvement_and_test_time() {
        let mut run = TestRun::new(RunId::new("t"));
        run.add_result(TestResult::success("tests.a").with_duration(Duration::from_secs(3)));
        run.add_result(TestResult::success("tests.b").with_duration(Duration::from_secs(2)));
        // Wall-clock 4s vs 5s average is -20%; summed test time is 5s.
        let line = format_runtime_line(&run, Duration::from_secs(4), Some(Duration::from_secs(5)));
        assert_eq!(
            line,
            "**Runtime:** 4.00s (down 20% vs avg of last runs, 5.00s)  \u{2014}  test time: 5.00s"
        );
    }

    #[test]
    fn runtime_line_omits_comparison_without_baseline() {
        let run = make_run();
        let line = format_runtime_line(&run, Duration::from_secs_f64(1.5), None);
        assert_eq!(line, "**Runtime:** 1.50s");
    }

    #[test]
    fn slowest_tests_block_lists_top_by_duration() {
        let mut run = TestRun::new(RunId::new("s"));
        run.add_result(TestResult::success("tests.fast").with_duration(Duration::from_millis(10)));
        run.add_result(TestResult::success("tests.slow").with_duration(Duration::from_secs(3)));
        run.add_result(TestResult::success("tests.mid").with_duration(Duration::from_secs(1)));
        let mut out = String::new();
        write_slowest_tests_block(&mut out, &run);
        let expected = "\n\
             <details><summary>Slowest tests (3)</summary>\n\
             \n\
             - `tests.slow` — 3.00s\n\
             - `tests.mid` — 1.00s\n\
             - `tests.fast` — 10ms\n\
             \n\
             </details>\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn slowest_tests_block_empty_without_timing() {
        // `make_run` results carry no durations, so the block is suppressed.
        let run = make_run();
        let mut out = String::new();
        write_slowest_tests_block(&mut out, &run);
        assert_eq!(out, "");
    }

    #[test]
    fn filter_annotations_keeps_only_matching_titles() {
        let annotations = "\
            ::error file=src/a.rs,line=1,title=tests.a::msg\n\
            ::error file=src/b.rs,line=2,title=tests.b::msg\n";
        let mut keep: HashSet<&TestId> = HashSet::new();
        let id = TestId::new("tests.b");
        keep.insert(&id);
        let out = filter_annotations(annotations, &keep);
        assert_eq!(out, "::error file=src/b.rs,line=2,title=tests.b::msg\n");
    }

    #[test]
    fn format_group_block_matches_batched_output() {
        // A single block built by `format_group_block` should be exactly what
        // `format_failure_groups` would emit for that one test, so the
        // streaming and batched paths produce identical workflow output.
        let run = make_run();
        let beta = run.results.get(&TestId::new("tests.b")).unwrap();
        let single = format_group_block(beta, CiFormat::Github);
        let mut only_beta = TestRun::new(RunId::new("only"));
        only_beta.add_result(beta.clone());
        let batched = format_failure_groups(&only_beta, None, CiFormat::Github);
        assert_eq!(single, batched);
    }

    #[test]
    fn format_group_block_skips_empty_message_and_details() {
        // Empty/whitespace-only message and missing details should still
        // produce a well-formed group with just the header and endgroup.
        let mut result = TestResult::failure("tests.empty", "   ");
        result.details = None;
        let out = format_group_block(&result, CiFormat::Github);
        assert_eq!(out, "::group::tests.empty\n::endgroup::\n");
    }

    /// Deterministic clock for GitLab section tests. The actual timestamp
    /// value is irrelevant to GitLab's section matching (it only uses it for
    /// the cosmetic duration display); we just want it stable for `assert_eq!`.
    fn fake_clock() -> i64 {
        1_700_000_000
    }

    #[test]
    fn format_group_block_uses_gitlab_section_markers() {
        let run = make_run();
        let beta = run.results.get(&TestId::new("tests.b")).unwrap();
        let out = format_group_block_with(beta, CiFormat::Gitlab, fake_clock);
        let expected = "\x1b[0Ksection_start:1700000000:tests.b[collapsed=true]\r\x1b[0Ktests.b\n\
                        boom\n\
                        File \"tests/b.py\", line 7, in test\n    \
                        raise AssertionError\n\
                        \x1b[0Ksection_end:1700000000:tests.b\r\x1b[0K\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn gitlab_section_name_sanitizes_disallowed_chars() {
        // GitLab section names allow only [A-Za-z0-9._-]; everything else
        // gets replaced with `_` so the runner accepts the marker.
        assert_eq!(
            sanitize_gitlab_section_name("foo::bar::baz"),
            "foo__bar__baz"
        );
        assert_eq!(sanitize_gitlab_section_name("mod/file.rs"), "mod_file.rs");
        assert_eq!(
            sanitize_gitlab_section_name("test[case,with,commas]"),
            "test_case_with_commas_"
        );
        assert_eq!(sanitize_gitlab_section_name("plain.id-9_x"), "plain.id-9_x");
        // Empty input collapses to a single underscore so the section name
        // is never empty (GitLab rejects empty names).
        assert_eq!(sanitize_gitlab_section_name(""), "_");
    }

    #[test]
    fn format_error_annotation_extracts_source_location() {
        let run = make_run();
        let beta = run.results.get(&TestId::new("tests.b")).unwrap();
        let out = format_error_annotation(beta);
        assert_eq!(out, "::error file=tests/b.py,line=7,title=tests.b::boom\n");
    }

    #[test]
    fn format_error_annotation_empty_for_non_failures() {
        let run = make_run();
        let alpha = run.results.get(&TestId::new("tests.a")).unwrap();
        let skipped = run.results.get(&TestId::new("tests.d")).unwrap();
        assert_eq!(format_error_annotation(alpha), "");
        assert_eq!(format_error_annotation(skipped), "");
    }

    #[test]
    fn failure_groups_skip_already_streamed() {
        let run = make_run();
        let mut skip: HashSet<TestId> = HashSet::new();
        skip.insert(TestId::new("tests.b"));
        let out = format_failure_groups(&run, Some(&skip), CiFormat::Github);
        // tests.b was already streamed live; only tests.c remains for the
        // batched emission.
        assert_eq!(
            out,
            "::group::tests.c\n\
             timeout\n\
             ::endgroup::\n"
        );
    }

    /// Expected step-output body for `make_run()` (1 success, 1 failure,
    /// 1 error, 1 skip). Each test constructs the exact `flaky` /
    /// `run_id` / `duration` it wants and reuses this fragment.
    fn expected_step_outputs(failed: usize, flaky: usize, run_id: &str, duration: &str) -> String {
        format!(
            "passed=1\nfailed={}\nflaky={}\nduration={}\nrun_id={}\n",
            failed, flaky, duration, run_id,
        )
    }

    #[test]
    fn write_github_output_writes_expected_lines() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let run = make_run();
        write_github_output(
            &e,
            &run,
            &[],
            &RunId::new("42"),
            Duration::from_millis(2500),
        )
        .unwrap();

        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, expected_step_outputs(2, 0, "42", "2.500"));
    }

    #[test]
    fn write_github_output_subtracts_flakes_from_failed() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        write_github_output(&e, &run, &flaky, &RunId::new("1"), Duration::ZERO).unwrap();

        let body = std::fs::read_to_string(temp.path()).unwrap();
        // tests.b recovered on retry, so it counts as flaky, not failed.
        assert_eq!(body, expected_step_outputs(1, 1, "1", "0.000"));
    }

    #[test]
    fn write_github_output_appends_when_file_already_has_content() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), "prior=value\n").unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let run = make_run();
        write_github_output(&e, &run, &[], &RunId::new("1"), Duration::ZERO).unwrap();

        let body = std::fs::read_to_string(temp.path()).unwrap();
        // GitHub Actions concatenates outputs from multiple writers in a
        // single step; we must append, not truncate.
        let expected = format!("prior=value\n{}", expected_step_outputs(2, 0, "1", "0.000"));
        assert_eq!(body, expected);
    }

    #[test]
    fn write_github_output_no_op_when_env_var_unset() {
        let e = env(&[]);
        let run = make_run();
        // Should succeed without trying to open any file.
        write_github_output(&e, &run, &[], &RunId::new("1"), Duration::ZERO).unwrap();
    }

    #[test]
    fn emit_ci_output_skips_streamed_error_annotations() {
        // When the streaming callback already wrote error annotations,
        // emit_ci_output must not duplicate them — only the foldable
        // groups should reach the UI.
        let run = make_run();
        let mut ui = crate::ui::test_ui::TestUI::new();
        let e = env(&[]);
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Github,
            &e,
            None,
            /* streamed_errors_already */ true,
            &RunId::new("0"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        // ui.output() trims the trailing newline before recording, so the
        // captured form drops the final `\n` after the last `::endgroup::`.
        assert_eq!(
            ui.output,
            vec!["::group::tests.b\n\
                 boom\n\
                 File \"tests/b.py\", line 7, in test\n    \
                 raise AssertionError\n\
                 ::endgroup::\n\
                 ::group::tests.c\n\
                 timeout\n\
                 ::endgroup::"
                .to_string()]
        );
    }

    #[test]
    fn emit_ci_output_writes_step_outputs_only_for_github() {
        // `$GITHUB_OUTPUT` is a GitHub Actions concept; the GitLab branch
        // must not touch it even when the env var happens to be set.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        std::fs::write(temp.path(), "").unwrap();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = make_run();
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Gitlab,
            &e,
            None,
            false,
            &RunId::new("0"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, "", "GitLab branch must not write to $GITHUB_OUTPUT");
    }

    #[test]
    fn emit_ci_output_forgejo_writes_step_outputs_but_no_log_markup() {
        // Forgejo doesn't render workflow commands but does honour
        // `$GITHUB_OUTPUT` (mirrored from `$FORGEJO_OUTPUT`). Emitting
        // `::group::` / `::error::` would just clutter the log, so the
        // log output stays empty.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = make_run();
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Forgejo,
            &e,
            None,
            false,
            &RunId::new("forge"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(ui.output, Vec::<String>::new());
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, expected_step_outputs(2, 0, "forge", "0.000"));
    }

    #[test]
    fn emit_ci_output_forgejo_skips_step_summary() {
        // `$GITHUB_STEP_SUMMARY` is a GitHub-only feature; Forgejo doesn't
        // render it. We must not touch the file even if the env var leaks
        // in (e.g. from a copy-pasted workflow).
        let out_temp = tempfile::NamedTempFile::new().unwrap();
        let summary_temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(summary_temp.path(), "untouched\n").unwrap();
        let out_path = out_temp.path().to_string_lossy().to_string();
        let summary_path = summary_temp.path().to_string_lossy().to_string();
        let e = env(&[
            ("GITHUB_OUTPUT", out_path.as_str()),
            ("GITHUB_STEP_SUMMARY", summary_path.as_str()),
        ]);

        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = make_run();
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Forgejo,
            &e,
            None,
            false,
            &RunId::new("forge"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        let summary = std::fs::read_to_string(summary_temp.path()).unwrap();
        assert_eq!(summary, "untouched\n");
    }

    #[test]
    fn emit_ci_output_woodpecker_writes_nothing_provider_specific() {
        // Woodpecker has no workflow-command or env-driven output surfaces;
        // both stdout and any `$GITHUB_OUTPUT`-like file must stay empty.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = make_run();
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Woodpecker,
            &e,
            None,
            false,
            &RunId::new("wp"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(ui.output, Vec::<String>::new());
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn write_dotenv_writes_step_outputs_to_explicit_path() {
        // `--dotenv-path` is the GitLab path: same wire format as
        // `$GITHUB_OUTPUT`, but written to a user-named file the pipeline
        // declares as `artifacts:reports:dotenv:`.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let run = make_run();
        write_dotenv(
            &run,
            &[],
            &RunId::new("7"),
            Duration::from_millis(500),
            temp.path(),
        )
        .unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, expected_step_outputs(2, 0, "7", "0.500"));
    }

    #[test]
    fn write_dotenv_truncates_existing_content() {
        // Unlike $GITHUB_OUTPUT, the dotenv path is owned by inq for the
        // run; pre-existing content must be replaced, not appended.
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), "stale=true\n").unwrap();
        let run = make_run();
        write_dotenv(&run, &[], &RunId::new("1"), Duration::ZERO, temp.path()).unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, expected_step_outputs(2, 0, "1", "0.000"));
    }

    #[cfg(feature = "junit")]
    #[test]
    fn write_junit_report_matches_export_junit_output() {
        // `write_junit_report` is a thin wrapper around `export_junit`. We
        // care about the exact bytes hitting disk because GitLab parses the
        // XML structurally — any drift between the two paths would be a bug.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let run = make_run();
        write_junit_report(&run, temp.path()).unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        let expected = crate::commands::export::export_junit(&run).unwrap();
        assert_eq!(body, expected);
    }

    #[test]
    fn step_summary_annotates_failures_with_recent_history() {
        // When the repository has past-run history for a failing test, the
        // summary appends a "(failed N of last M)" hint so reviewers can
        // tell a fresh regression apart from a chronic flake at a glance.
        let run = make_run();
        let mut history = HashMap::new();
        history.insert(
            TestId::new("tests.b"),
            TestHistory {
                failures: 7,
                runs_seen: 10,
            },
        );
        // tests.c has no history — it's a brand-new test; the hint should
        // be omitted rather than printing a misleading "0 of 0".
        let summary = format_step_summary(&run, &[], &history, Duration::from_secs(1), None);
        assert!(
            summary.contains("- `tests.b` — boom (failed 7 of last 10)\n"),
            "missing history hint for tests.b: {summary}"
        );
        assert!(
            summary.contains("- `tests.c` — timeout\n"),
            "tests.c should not have a hint: {summary}"
        );
    }

    #[test]
    fn step_summary_annotates_flaky_tests_with_recent_history() {
        // The same hint applies to the flaky-tests list: a test that just
        // started failing should look different from one that's been
        // flaking for weeks.
        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        let mut history = HashMap::new();
        history.insert(
            TestId::new("tests.b"),
            TestHistory {
                failures: 4,
                runs_seen: 10,
            },
        );
        let summary = format_step_summary(&run, &flaky, &history, Duration::from_secs(1), None);
        assert!(
            summary.contains("- `tests.b` (passed on retry) (failed 4 of last 10)\n"),
            "missing flake history hint: {summary}"
        );
    }

    #[test]
    fn step_summary_truncates_long_tracebacks() {
        // A traceback exceeding SUMMARY_DETAILS_MAX_LINES is cut off with a
        // "... (truncated)" marker so a single blown-up failure can't push
        // the whole summary past GitHub's 1MiB cap.
        let mut run = TestRun::new(RunId::new("0"));
        let long: String = (0..50).map(|i| format!("line {i}\n")).collect();
        run.add_result(TestResult::failure("tests.big", "oops").with_details(&long));
        let summary = format_step_summary(&run, &[], &HashMap::new(), Duration::from_secs(1), None);
        assert!(
            summary.contains("  line 0\n"),
            "first line missing: {summary}"
        );
        assert!(
            summary.contains(&format!("  line {}\n", SUMMARY_DETAILS_MAX_LINES - 1)),
            "last kept line missing: {summary}"
        );
        assert!(
            !summary.contains(&format!("  line {}\n", SUMMARY_DETAILS_MAX_LINES)),
            "line past the cap should be dropped: {summary}"
        );
        assert!(
            summary.contains("  ... (truncated)\n"),
            "truncation marker missing: {summary}"
        );
    }

    #[test]
    fn step_summary_omits_traceback_when_details_empty() {
        // Tests without recorded details (e.g., a bare `TestResult::error`)
        // still appear in the failing list but with no nested block.
        let mut run = TestRun::new(RunId::new("0"));
        run.add_result(TestResult::error("tests.nodetail", "gone"));
        let summary = format_step_summary(&run, &[], &HashMap::new(), Duration::from_secs(1), None);
        assert!(summary.contains("- `tests.nodetail` — gone\n"));
        assert!(!summary.contains("<summary>traceback</summary>"));
    }

    #[test]
    fn format_history_hint_omits_runs_seen_zero() {
        // Defensive: even if a `TestHistory` entry exists with `runs_seen = 0`
        // (no recorded results in the window), we must not print "0 of 0".
        let id = TestId::new("tests.x");
        let mut history = HashMap::new();
        history.insert(id.clone(), TestHistory::default());
        assert_eq!(format_history_hint(&id, &history), "");
    }

    #[test]
    fn build_result_callback_cancels_token_on_max_failures() {
        // With `max_failures = 2`, the third failure to come through the
        // callback should find the token already cancelled because the
        // second one tripped it. Passing tests don't count toward the limit.
        let mut cmd = CiCommand::new(None);
        cmd.max_failures = Some(2);
        let counter = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        // No streaming surface — we're testing the cap path on its own.
        let cb = cmd
            .build_result_callback(None, false, CiFormat::Plain, counter.clone(), token.clone())
            .expect("callback should exist when max_failures is set");

        cb(&TestResult::success("tests.ok"));
        assert!(!token.is_cancelled());
        cb(&TestResult::failure("tests.fail1", "x"));
        assert!(!token.is_cancelled());
        cb(&TestResult::failure("tests.fail2", "x"));
        assert!(token.is_cancelled(), "second failure should cancel");
        // Subsequent failures must be a no-op (re-cancelling is fine).
        cb(&TestResult::failure("tests.fail3", "x"));
        assert!(token.is_cancelled());
        // Counter reflects observed failures, including post-cancel ones —
        // we don't pretend they didn't happen.
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn build_result_callback_none_when_no_streaming_and_no_cap() {
        // With neither a streaming surface nor a failure cap, the callback
        // has nothing to do and we save the executor an allocation per test.
        let cmd = CiCommand::new(None);
        let counter = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        assert!(cmd
            .build_result_callback(None, false, CiFormat::Plain, counter, token)
            .is_none());
    }

    #[test]
    fn build_result_callback_cap_alone_returns_callback_without_streaming() {
        // `--max-failures` without a streaming format (e.g. plain/forgejo)
        // still needs the callback to count failures and trip the token.
        let mut cmd = CiCommand::new(None);
        cmd.max_failures = Some(1);
        let counter = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        let cb = cmd
            .build_result_callback(None, false, CiFormat::Plain, counter, token.clone())
            .expect("callback should exist");
        cb(&TestResult::failure("tests.fail", "x"));
        assert!(token.is_cancelled());
    }

    #[test]
    fn emit_ci_output_writes_step_outputs_for_github() {
        // The GitHub branch wires `$GITHUB_OUTPUT` through, so a downstream
        // step can read passed/failed/run_id without inq needing a separate
        // call from the orchestration layer.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        std::fs::write(temp.path(), "").unwrap();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = make_run();
        emit_ci_output(
            &mut ui,
            &run,
            &[],
            CiFormat::Github,
            &e,
            None,
            false,
            &RunId::new("99"),
            Duration::ZERO,
            &HashMap::new(),
            None,
        )
        .unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, expected_step_outputs(2, 0, "99", "0.000"));
    }
}
