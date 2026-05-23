//! `inq ci` - run tests with output formatted for a CI provider.
//!
//! Wraps `RunCommand` with a few CI-friendly defaults (smart ordering, auto
//! detection of the CI provider, opt-in flake retries) and emits structured
//! output that GitHub Actions or GitLab CI can render natively: log groups
//! per failing test, inline annotations, and a markdown job summary written
//! to `$GITHUB_STEP_SUMMARY` when present.
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
use crate::commands::run::RunCommand;
use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::config::TimeoutSetting;
use crate::error::Result;
use crate::ordering::TestOrder;
use crate::repository::{TestId, TestRun, TestStatus};
use crate::ui::UI;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;

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

        let order = match self.order.clone() {
            Some(o) => o,
            None => default_ci_order(self.base_path.as_deref()),
        };

        let initial = self.make_run_command(order.clone(), false);
        let initial_output = initial.execute_returning_run_id(ui)?;

        let initial_run_id = match initial_output.run_id {
            Some(id) => id,
            None => return Ok(initial_output.exit_code),
        };

        let initial_failures = collect_failures(self.base_path.as_deref(), &initial_run_id)?;

        let mut still_failing: HashSet<TestId> = initial_failures.iter().cloned().collect();
        if self.retries > 0 && !still_failing.is_empty() {
            for _ in 0..self.retries {
                if still_failing.is_empty() {
                    break;
                }
                let retry = self.make_retry_command(order.clone());
                let retry_out = retry.execute_returning_run_id(ui)?;
                if let Some(rid) = retry_out.run_id {
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

        // Re-read the initial run so annotations reflect its details.
        let initial_run = {
            let repo = open_repository(self.base_path.as_deref())?;
            repo.get_test_run(&initial_run_id)?
        };

        emit_ci_output(ui, &initial_run, &flaky, format, env)?;

        // Successful overall if nothing is still failing after retries.
        if still_failing.is_empty() {
            Ok(0)
        } else {
            Ok(1)
        }
    }

    fn make_run_command(&self, order: TestOrder, failing_only: bool) -> RunCommand {
        RunCommand {
            base_path: self.base_path.clone(),
            failing_only,
            partial: failing_only,
            // Auto-detect config so a fresh CI checkout works without setup.
            auto: !failing_only,
            force_init: !failing_only,
            concurrency: self.concurrency,
            test_filters: if self.test_filters.is_empty() {
                None
            } else {
                Some(self.test_filters.clone())
            },
            starting_with: if self.starting_with.is_empty() {
                None
            } else {
                Some(self.starting_with.clone())
            },
            test_args: if self.test_args.is_empty() {
                None
            } else {
                Some(self.test_args.clone())
            },
            test_timeout: self.test_timeout.clone(),
            max_duration: self.max_duration.clone(),
            test_order: Some(order),
            profile: self.profile.clone(),
            ..Default::default()
        }
    }

    fn make_retry_command(&self, order: TestOrder) -> RunCommand {
        self.make_run_command(order, true)
    }
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

fn emit_ci_output(
    ui: &mut dyn UI,
    run: &TestRun,
    flaky: &[TestId],
    format: CiFormat,
    env: &dyn EnvLookup,
) -> Result<()> {
    match format {
        CiFormat::Plain | CiFormat::Auto => Ok(()),
        CiFormat::Github | CiFormat::Gitlab => {
            let mut out = String::new();

            // Per-failing-test log groups (foldable in the workflow log).
            out.push_str(&format_failure_groups(run));

            // Warning annotations for recovered (flaky) tests.
            out.push_str(&format_flaky_warnings(run, flaky));

            // Error annotations for tests still failing.
            let still_failing: HashSet<&TestId> = run
                .results
                .iter()
                .filter(|(id, r)| is_failure(r.status) && !flaky.iter().any(|f| f == *id))
                .map(|(id, _)| id)
                .collect();
            out.push_str(&filter_annotations(&export_github(run), &still_failing));

            if !out.is_empty() {
                ui.output(out.trim_end_matches('\n'))?;
            }

            // GitHub-only: write a markdown summary to $GITHUB_STEP_SUMMARY.
            if format == CiFormat::Github {
                if let Some(path) = env.get("GITHUB_STEP_SUMMARY") {
                    let summary = format_step_summary(run, flaky);
                    std::fs::write(&path, summary)?;
                }
            }
            Ok(())
        }
    }
}

fn is_failure(status: TestStatus) -> bool {
    matches!(
        status,
        TestStatus::Failure | TestStatus::Error | TestStatus::UnexpectedSuccess
    )
}

/// Emit one `::group::TEST_ID` / `::endgroup::` block per failing test, with
/// the test's `details` (traceback, captured output) as the group body.
fn format_failure_groups(run: &TestRun) -> String {
    let mut out = String::new();
    let mut failures: Vec<_> = run
        .results
        .values()
        .filter(|r| is_failure(r.status))
        .collect();
    failures.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));

    for result in failures {
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
    }
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
        let out = format_failure_groups(&run);
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
        let out = format_failure_groups(&run);
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
}
