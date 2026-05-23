//! `inq ci` - run tests with output formatted for a CI provider.
//!
//! Runs the test executor directly (no `RunCommand` wrap) with CI-friendly
//! defaults: smart ordering, CI provider auto-detection, opt-in flake retries,
//! streaming `::error::` / `::group::` annotations as failures land in the
//! workflow log, a markdown job summary at `$GITHUB_STEP_SUMMARY`, and
//! step outputs written to `$GITHUB_OUTPUT` so downstream steps can read
//! `passed`/`failed`/`flaky`/`duration`/`run_id`.
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
use crate::test_executor::{self, TestExecutor, TestExecutorConfig};
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::io::Write;
use std::path::Path;
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
    /// GitLab CI workflow commands (same wire format as GitHub).
    Gitlab,
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
            "plain" | "none" => Ok(CiFormat::Plain),
            other => Err(format!(
                "unknown ci format '{}': expected auto, github, gitlab, or plain",
                other
            )),
        }
    }
}

impl CiFormat {
    /// Resolve `Auto` against the current environment. Concrete formats are
    /// returned unchanged.
    fn resolve(self, env: &dyn EnvLookup) -> CiFormat {
        match self {
            CiFormat::Auto => {
                if env.get("GITHUB_ACTIONS").as_deref() == Some("true") {
                    CiFormat::Github
                } else if env.get("GITLAB_CI").as_deref() == Some("true") {
                    CiFormat::Gitlab
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
        let result_callback = self.build_result_callback(streamed_ids.clone(), emit_errors_now);

        let total_start = std::time::Instant::now();
        let initial_run_id = match self.run_initial(ui, order.clone(), result_callback)? {
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
        )?;

        Ok(if still_failing.is_empty() { 0 } else { 1 })
    }

    /// Build the per-result callback handed to the executor. Records the id
    /// of each failure it streams so the post-run code can avoid re-emitting,
    /// and optionally writes `::group::`/`::error::` annotations immediately.
    fn build_result_callback(
        &self,
        streamed_ids: Option<Arc<Mutex<HashSet<TestId>>>>,
        emit_errors_now: bool,
    ) -> Option<test_executor::ResultCallback> {
        let streamed_ids = streamed_ids?;
        Some(Arc::new(move |result: &TestResult| {
            if !is_failure(result.status) {
                return;
            }

            let mut out = String::new();
            out.push_str(&format_group_block(result));
            if emit_errors_now {
                out.push_str(&format_error_annotation(result));
            }
            // CI runners are non-TTY so progress bars are hidden; writing
            // directly to stdout works without progress-bar interference.
            let _ = std::io::stdout().write_all(out.as_bytes());
            let _ = std::io::stdout().flush();

            if let Ok(mut g) = streamed_ids.lock() {
                g.insert(result.test_id.clone());
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
    ) -> Result<RunOnceOutcome> {
        self.run_once(ui, order, result_callback, /* failing_only */ false)
    }

    /// Retry run: only previously failing tests, no streaming callback (the
    /// annotation set is decided after retries settle).
    fn run_retry(&self, ui: &mut dyn UI, order: TestOrder) -> Result<RunOnceOutcome> {
        self.run_once(ui, order, None, /* failing_only */ true)
    }

    fn run_once(
        &self,
        ui: &mut dyn UI,
        order: TestOrder,
        result_callback: Option<test_executor::ResultCallback>,
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
        let config = TestExecutorConfig {
            base_path: self.base_path.clone(),
            all_output: false,
            test_args: if self.test_args.is_empty() {
                None
            } else {
                Some(self.test_args.clone())
            },
            cancellation_token: None,
            max_restarts: None,
            stderr_capture: Some(stderr_capture.clone()),
            result_callback,
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
) -> Result<()> {
    match format {
        CiFormat::Plain | CiFormat::Auto => Ok(()),
        CiFormat::Github | CiFormat::Gitlab => {
            let mut out = String::new();

            // Per-failing-test log groups (foldable in the workflow log). Skip
            // any test whose group was already streamed live during the run.
            out.push_str(&format_failure_groups(run, already_streamed));

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

            // GitHub-only: markdown summary + step outputs. Both no-op when
            // their env vars are unset (i.e. running locally with
            // `--format=github`).
            if format == CiFormat::Github {
                if let Some(path) = env.get("GITHUB_STEP_SUMMARY") {
                    let summary = format_step_summary(run, flaky);
                    std::fs::write(&path, summary)?;
                }
                write_github_output(env, run, flaky, run_id, duration)?;
            }
            Ok(())
        }
    }
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

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

fn is_failure(status: TestStatus) -> bool {
    matches!(
        status,
        TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess
    )
}

/// Emit one `::group::TEST_ID` / `::endgroup::` block per failing test, with
/// the test's `details` (traceback, captured output) as the group body.
/// Skips any test whose id is in `skip` (typically: already streamed live).
fn format_failure_groups(run: &TestRun, skip: Option<&HashSet<TestId>>) -> String {
    let mut out = String::new();
    let mut failures: Vec<_> = run
        .results
        .values()
        .filter(|r| is_failure(r.status))
        .filter(|r| skip.is_none_or(|s| !s.contains(&r.test_id)))
        .collect();
    failures.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));

    for result in failures {
        out.push_str(&format_group_block(result));
    }
    out
}

/// `::group::TEST_ID` / `::endgroup::` block for a single result.
fn format_group_block(result: &TestResult) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "::group::{}", result.test_id.as_str());
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
    out.push_str("::endgroup::\n");
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

/// Markdown summary written to `$GITHUB_STEP_SUMMARY`, which GitHub renders
/// on the workflow run page.
fn format_step_summary(run: &TestRun, flaky: &[TestId]) -> String {
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
            let _ = writeln!(out, "- `{}` — {}", r.test_id.as_str(), msg);
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
            let _ = writeln!(out, "- `{}` (passed on retry)", t.as_str());
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
    fn ci_format_from_str() {
        assert_eq!("github".parse::<CiFormat>().unwrap(), CiFormat::Github);
        assert_eq!("AUTO".parse::<CiFormat>().unwrap(), CiFormat::Auto);
        assert_eq!("plain".parse::<CiFormat>().unwrap(), CiFormat::Plain);
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
    fn failure_groups_only_for_failing_tests() {
        let run = make_run();
        let out = format_failure_groups(&run, None);
        assert!(out.contains("::group::tests.b"));
        assert!(out.contains("::group::tests.c"));
        assert!(!out.contains("::group::tests.a"));
        assert!(!out.contains("::group::tests.d"));
        // Each group is properly closed.
        assert_eq!(out.matches("::group::").count(), 2);
        assert_eq!(out.matches("::endgroup::").count(), 2);
    }

    #[test]
    fn failure_group_body_carries_message_and_details() {
        let run = make_run();
        let out = format_failure_groups(&run, None);
        assert!(out.contains("boom"));
        assert!(out.contains("AssertionError"));
    }

    #[test]
    fn flaky_warnings_emit_warning_annotations() {
        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        let out = format_flaky_warnings(&run, &flaky);
        assert!(out.starts_with("::warning "));
        assert!(out.contains("file=tests/b.py"));
        assert!(out.contains("line=7"));
        assert!(out.contains("title=Flaky: tests.b"));
        assert!(out.contains("passed on retry"));
    }

    #[test]
    fn flaky_warnings_empty_when_no_flakes() {
        let run = make_run();
        let out = format_flaky_warnings(&run, &[]);
        assert_eq!(out, "");
    }

    #[test]
    fn step_summary_counts_and_lists_failures() {
        let run = make_run();
        let summary = format_step_summary(&run, &[]);
        assert!(summary.contains("## Test results"));
        // total=4, passed=1, failed=1, errored=1, skipped=1, flaky=0
        assert!(summary.contains("| 4 | 1 | 1 | 1 | 1 | 0 |"));
        assert!(summary.contains("Failing tests (2)"));
        assert!(summary.contains("`tests.b`"));
        assert!(summary.contains("`tests.c`"));
    }

    #[test]
    fn step_summary_separates_flakes_from_hard_failures() {
        let run = make_run();
        let flaky = vec![TestId::new("tests.b")];
        let summary = format_step_summary(&run, &flaky);
        // tests.b is now flaky, so failing list shows only tests.c.
        assert!(summary.contains("Failing tests (1)"));
        assert!(summary.contains("Flaky tests (1)"));
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
        let single = format_group_block(beta);
        let mut only_beta = TestRun::new(RunId::new("only"));
        only_beta.add_result(beta.clone());
        let batched = format_failure_groups(&only_beta, None);
        assert_eq!(single, batched);
    }

    #[test]
    fn format_group_block_skips_empty_message_and_details() {
        // Empty/whitespace-only message and missing details should still
        // produce a well-formed group with just the header and endgroup.
        let mut result = TestResult::failure("tests.empty", "   ");
        result.details = None;
        let out = format_group_block(&result);
        assert_eq!(out, "::group::tests.empty\n::endgroup::\n");
    }

    #[test]
    fn format_error_annotation_extracts_source_location() {
        let run = make_run();
        let beta = run.results.get(&TestId::new("tests.b")).unwrap();
        let out = format_error_annotation(beta);
        assert!(out.starts_with("::error "));
        assert!(out.contains("file=tests/b.py"));
        assert!(out.contains("line=7"));
        assert!(out.contains("title=tests.b"));
        assert!(out.contains("boom"));
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
        let out = format_failure_groups(&run, Some(&skip));
        // tests.b was already streamed; only tests.c should appear.
        assert!(!out.contains("::group::tests.b"));
        assert!(out.contains("::group::tests.c"));
        assert_eq!(out.matches("::group::").count(), 1);
    }

    #[test]
    fn write_github_output_writes_expected_lines() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let e = env(&[("GITHUB_OUTPUT", path.as_str())]);

        let run = make_run();
        let flaky: Vec<TestId> = Vec::new();
        let run_id = RunId::new("42");
        let duration = Duration::from_millis(2500);

        write_github_output(&e, &run, &flaky, &run_id, duration).unwrap();

        let body = std::fs::read_to_string(temp.path()).unwrap();
        // 1 passed (tests.a), 2 failed (tests.b + tests.c), 0 flaky.
        assert!(body.contains("passed=1\n"));
        assert!(body.contains("failed=2\n"));
        assert!(body.contains("flaky=0\n"));
        assert!(body.contains("duration=2.500\n"));
        assert!(body.contains("run_id=42\n"));
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
        assert!(body.contains("failed=1\n"));
        assert!(body.contains("flaky=1\n"));
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
        assert!(body.starts_with("prior=value\n"));
        assert!(body.contains("run_id=1\n"));
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
        // emit_ci_output must not duplicate them.
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
        )
        .unwrap();
        let captured = ui.output.join("\n");
        // Groups still come from the post-hoc path (we passed None for
        // already_streamed) but error lines should not appear.
        assert!(captured.contains("::group::tests.b"));
        assert!(!captured.contains("::error "));
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
        )
        .unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert_eq!(body, "", "GitLab branch must not write to $GITHUB_OUTPUT");
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
        )
        .unwrap();
        let body = std::fs::read_to_string(temp.path()).unwrap();
        assert!(body.contains("run_id=99\n"));
        assert!(body.contains("passed=1\n"));
    }
}
