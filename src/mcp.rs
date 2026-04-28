//! MCP (Model Context Protocol) server for inquest
//!
//! Provides structured JSON access to test repository data via the MCP protocol.
//! Start with `inq mcp` to run the server over stdio.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorData, Implementation, Meta, ProgressNotificationParam,
    ProgressToken, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, Peer, RoleServer, ServerHandler};
use serde::Serialize;

use crate::commands::utils::{open_or_init_repository, open_repository, resolve_run_id};
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, TestResult, TestStatus};
use crate::subunit_stream;
use crate::testcommand::TestCommand;

use crate::test_executor::CancellationToken;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A UI implementation that collects output and errors into vectors.
struct CollectUI {
    output: Vec<String>,
    errors: Vec<String>,
}

impl CollectUI {
    fn new() -> Self {
        CollectUI {
            output: Vec::new(),
            errors: Vec::new(),
        }
    }
}

impl crate::ui::UI for CollectUI {
    fn output(&mut self, msg: &str) -> crate::error::Result<()> {
        self.output.push(msg.to_string());
        Ok(())
    }
    fn error(&mut self, msg: &str) -> crate::error::Result<()> {
        self.errors.push(msg.to_string());
        Ok(())
    }
    fn warning(&mut self, msg: &str) -> crate::error::Result<()> {
        self.errors.push(msg.to_string());
        Ok(())
    }
}

/// MCP server for inquest test repositories.
#[derive(Debug, Clone)]
pub struct InquestMcpService {
    directory: PathBuf,
    /// Cancellation tokens for background runs, keyed by run ID.
    cancel_tokens: Arc<Mutex<HashMap<crate::repository::RunId, CancellationToken>>>,
}

impl InquestMcpService {
    /// Create a new MCP service for the given directory.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            cancel_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn dir_str(&self) -> String {
        self.directory.to_string_lossy().to_string()
    }

    fn open_repo(&self) -> Result<Box<dyn Repository>, ErrorData> {
        open_repository(Some(&self.dir_str())).map_err(|e| {
            ErrorData::internal_error(format!("Failed to open repository: {}", e), None)
        })
    }
}

/// Parameters for tools that accept an optional run ID.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunIdParam {
    /// Run ID to query. Defaults to the latest run when omitted.
    ///
    /// Accepts either an absolute ID (e.g. "0", "42") or a negative index:
    /// "-1" = latest run, "-2" = second-to-latest, and so on. The absolute
    /// ID "0" always refers to the first run ever recorded, never to the
    /// latest — use "-1" for latest.
    pub run_id: Option<String>,
}

/// Parameters for the slowest tests tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SlowestParam {
    /// Number of slowest tests to return (default 10)
    pub count: Option<usize>,
}

/// Parameters for the flaky tests tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FlakyParam {
    /// Maximum number of tests to return, ranked by flakiness. Default 10.
    /// The response reports the total candidate pool so you know if more exist.
    pub count: Option<usize>,
    /// Minimum number of recorded runs a test must appear in to be ranked.
    /// Default 5. Tests with shorter history are dropped because their
    /// transition counts are statistically noisy. Lower it to inspect a
    /// young repository; raise it to be stricter.
    pub min_runs: Option<usize>,
}

/// Parameters for the diff tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiffParam {
    /// First run ID (defaults to second-to-latest; supports negative indices like -1, -2)
    pub run1: Option<String>,
    /// Second run ID (defaults to latest; supports negative indices like -1, -2)
    pub run2: Option<String>,
    /// Max number of test IDs to return per category (new_failures, new_passes, etc.).
    /// Default 50. The response reports totals so you know if more exist.
    pub limit: Option<usize>,
}

/// Parameters for tools that return a potentially large list of test IDs.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ListParam {
    /// Max number of test IDs to return (default 100). The response reports `total`
    /// and `truncated` so you know if more exist.
    pub limit: Option<usize>,
    /// Number of items to skip before returning results (default 0). Use with `limit`
    /// to page through large lists.
    pub offset: Option<usize>,
}

/// Parameters for the log tool.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct LogParam {
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2)
    pub run_id: Option<String>,
    /// Test ID patterns to match (glob-style wildcards). If empty, shows all tests.
    pub test_patterns: Option<Vec<String>>,
    /// Filter by test status. A test is included if its status matches **any**
    /// entry in the array (OR semantics). Valid individual values: "success",
    /// "failure", "error", "skip", "xfail", "uxsuccess". Two group aliases are
    /// also accepted: "failing" expands to failure+error+uxsuccess, and
    /// "passing" expands to success+skip+xfail.
    ///
    /// Examples: ["failing"] shows every test that failed in any way;
    /// ["failure", "error"] shows only failures and errors (no uxsuccess);
    /// ["failing", "skip"] shows every failure plus skipped tests. If empty
    /// or omitted, shows all statuses.
    pub status_filter: Option<Vec<String>>,
    /// Max number of results to return (default 20). Use a larger value when you need
    /// the full list; the response reports `total` and `truncated` so you know if more exist.
    pub limit: Option<usize>,
    /// Include failure messages and full tracebacks in each result. Off by default to
    /// keep responses small — enable when investigating a specific failure.
    pub include_details: Option<bool>,
    /// Max lines per message/details field when include_details=true. Long values
    /// are shortened by keeping the head and tail lines and eliding the middle.
    /// Individual lines are also capped at 500 characters. Default 60. Set to 0
    /// to disable truncation and receive the full content.
    pub max_detail_lines: Option<usize>,
}

/// Parameters for the failure-summary tool.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct FailureSummaryParam {
    /// Run ID to summarise (defaults to latest; supports negative indices like -1, -2).
    pub run_id: Option<String>,
    /// Max number of failures to return (default 25). The response reports
    /// `total_failing` and `truncated` so you know if more exist.
    pub limit: Option<usize>,
}

/// Parameters for the single-test lookup tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TestLookupParam {
    /// Exact test ID to fetch. Matched string-equal against stored IDs — no
    /// globs, no regex, no prefix matching. If you need pattern matching,
    /// use inq_log with `test_patterns` (glob) or inq_run with `test_filters`
    /// (regex).
    pub test_id: String,
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2).
    pub run_id: Option<String>,
    /// Max lines for message/details fields. See inq_log's max_detail_lines for semantics.
    /// Default 60. Set to 0 to receive the full content.
    pub max_detail_lines: Option<usize>,
}

/// Parameters for the batch test lookup tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TestBatchLookupParam {
    /// Exact test IDs to fetch. Each entry is matched string-equal against
    /// stored IDs — no globs, no regex, no prefix matching. Duplicates are
    /// de-duplicated in the response; unknown IDs are returned via `not_found`
    /// rather than failing the whole call. For pattern matching use inq_log.
    pub test_ids: Vec<String>,
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2).
    pub run_id: Option<String>,
    /// Max lines for message/details fields. See inq_log's max_detail_lines for semantics.
    /// Default 60. Set to 0 to receive the full content.
    pub max_detail_lines: Option<usize>,
}

/// Parameters for the run-listing tool.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ListRunsParam {
    /// Max number of runs to return (default 20). The response reports `total`
    /// and `truncated` so you know if more exist.
    pub limit: Option<usize>,
    /// Number of runs to skip before returning results (default 0). Use with
    /// `limit` to page through history. Offsets count from the newest run.
    pub offset: Option<usize>,
}

/// Parameters for the per-test history tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TestHistoryParam {
    /// Exact test ID to look up across runs. Matched string-equal — no globs.
    pub test_id: String,
    /// Max number of runs (newest first) to scan. Default 20. Runs where the
    /// test wasn't recorded are skipped and don't count against the limit.
    pub limit: Option<usize>,
}

/// Parameters for the analyze-isolation tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AnalyzeIsolationParam {
    /// The test ID to analyze for isolation issues
    pub test: String,
}

/// Parameters for the run tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunParam {
    /// Run only the tests that failed in the last run
    pub failing_only: Option<bool>,
    /// Number of parallel test workers
    pub concurrency: Option<usize>,
    /// Regex patterns to filter which tests to run
    pub test_filters: Option<Vec<String>>,
    /// Run tests in the background. Returns immediately with the run ID.
    /// Use inq_wait to block until the run completes, or inq_running to check
    /// progress. Use inq_last or inq_log to see results when done.
    ///
    /// Recommended for suites that may exceed the client's MCP tool-call
    /// timeout (Claude Code defaults to 2 minutes, configurable via
    /// MCP_TOOL_TIMEOUT). Foreground runs also emit periodic
    /// notifications/progress messages when the client supplies a
    /// progressToken in the request's _meta, which most compliant clients
    /// use to reset their timer — but background mode is the portable
    /// choice.
    pub background: Option<bool>,
    /// If the run is still in progress after this many seconds, stop waiting
    /// synchronously and return `{status: "running", run_id}`. The run keeps
    /// executing; poll `inq_running` or call `inq_wait` to follow it.
    ///
    /// Useful for suites whose duration is unpredictable: the tool returns
    /// the final result when the run is fast, and hands back a handle when
    /// it would otherwise risk tripping the client's tool-call timeout.
    /// Ignored when `background: true` (that path returns immediately).
    pub background_after: Option<u64>,
    /// Order in which tests are executed. One of: "auto" (smart pick from
    /// history — frequent-failing-first when there is failure history,
    /// otherwise spread), "discovery", "alphabetical", "failing-first",
    /// "spread", "shuffle" (optionally "shuffle:<seed>" for a reproducible
    /// shuffle), "slowest-first", "fastest-first", "frequent-failing-first".
    /// When omitted, falls back to the project's configured `test_order`,
    /// or discovery order if none is set.
    pub order: Option<String>,
}

/// Parameters for the cancel tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CancelParam {
    /// Run ID of the background test run to cancel.
    pub run_id: String,
}

/// Parameters for the wait tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WaitParam {
    /// Optional run ID to wait for. If not specified, waits for all running tests to complete.
    pub run_id: Option<String>,
    /// Optional status filter. If specified, returns early as soon as any test result
    /// in the running run matches **any** entry in the array (OR semantics). Uses
    /// the same syntax as inq_log's status_filter: individual values "success",
    /// "failure", "error", "skip", "xfail", "uxsuccess", plus group aliases "failing"
    /// (= failure+error+uxsuccess) and "passing" (= success+skip+xfail). Omit or
    /// leave empty to wait for the run to complete normally.
    pub status_filter: Option<Vec<String>>,
    /// Maximum time to wait in seconds. Defaults to 600 (10 minutes).
    pub timeout_secs: Option<u64>,
}

fn to_mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn duration_secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn ok_json<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string(value)
        .map_err(|e| ErrorData::internal_error(format!("serialize response: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn take_limited<T>(mut items: Vec<T>, limit: usize) -> (Vec<T>, usize) {
    if items.len() <= limit {
        (items, 0)
    } else {
        let extra = items.len() - limit;
        items.truncate(limit);
        (items, extra)
    }
}

/// Interval between MCP progress notifications during long-running tools.
///
/// Well under Claude Code's default MCP tool-call timeout (~2 minutes) so a
/// single notification always lands inside the window. MCP clients that reset
/// their tool-call timer on `notifications/progress` will keep the request
/// alive as long as these keep arriving.
const PROGRESS_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);

/// Send MCP progress notifications for the currently in-flight tool request.
///
/// Only constructed when the client opts in by attaching a `progressToken`
/// to the request's `_meta`. Handlers that don't receive one skip
/// reporting entirely. Cloneable so the ticker task and the handler itself
/// can each hold a copy.
#[derive(Clone)]
struct ProgressReporter {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

impl ProgressReporter {
    fn from_meta(peer: &Peer<RoleServer>, meta: &Meta) -> Option<Self> {
        meta.get_progress_token().map(|token| Self {
            peer: peer.clone(),
            token,
        })
    }

    async fn notify(&self, progress: u64, message: String) {
        let param = ProgressNotificationParam::new(self.token.clone(), progress as f64)
            .with_message(message);
        if let Err(e) = self.peer.notify_progress(param).await {
            tracing::debug!("progress notification failed: {e}");
        }
    }
}

/// Run `work` on a blocking thread while emitting MCP progress notifications
/// for the in-flight request at a regular cadence, so the client's tool-call
/// timer doesn't fire.
///
/// If `reporter` is `None` (the client didn't opt in to progress reporting),
/// no notifications are sent.
///
/// `message` is included in each notification for observability. Elapsed
/// seconds are appended automatically.
async fn run_blocking_with_progress<F, T>(
    reporter: Option<ProgressReporter>,
    message: &'static str,
    work: F,
) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let keepalive = reporter.map(|r| tokio::spawn(progress_keepalive_loop(r, stop_rx, message)));

    let result = tokio::task::spawn_blocking(work).await;

    // Signal the keepalive task to stop and wait for it to exit. Ignoring both
    // errors is intentional: the keepalive may have already exited (e.g. the
    // transport broke and it returned early), or the receiver was dropped.
    let _ = stop_tx.send(());
    if let Some(handle) = keepalive {
        let _ = handle.await;
    }

    result
}

async fn progress_keepalive_loop(
    reporter: ProgressReporter,
    stop: tokio::sync::oneshot::Receiver<()>,
    message: &'static str,
) {
    let start = std::time::Instant::now();
    let mut progress: u64 = 0;
    let mut ticker = tokio::time::interval(PROGRESS_KEEPALIVE_INTERVAL);
    // If the loop falls behind (e.g. the runtime stalls), catch up with a
    // single tick instead of firing a burst of backlogged notifications.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so we don't send a spurious 0-second
    // notification the moment the request starts.
    ticker.tick().await;

    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticker.tick() => {
                progress += 1;
                let elapsed = start.elapsed().as_secs();
                reporter
                    .notify(progress, format!("{message} (elapsed: {elapsed}s)"))
                    .await;
            }
        }
    }
}

/// Hard cap on characters per individual line when truncating tracebacks.
/// Protects against tests that dump a single giant blob on one line.
const MAX_LINE_CHARS: usize = 500;

/// Default cap for `failing_tests` arrays in responses (inq_last, inq_run).
/// A broken run with thousands of failing tests shouldn't produce a single
/// giant response.
const FAILING_LIST_LIMIT: usize = 100;

/// Default cap for `matching_tests` in inq_wait early-return responses.
const MATCHING_LIST_LIMIT: usize = 50;

/// Line budget for the `error_output` field on a failed inq_run response.
/// Matches the default for inq_log's traceback truncation.
const ERROR_OUTPUT_LINE_BUDGET: usize = 60;

/// Default number of failures returned by inq_failure_summary. Failures
/// cluster; 25 is usually enough to see the pattern.
const FAILURE_SUMMARY_LIMIT: usize = 25;

/// Max characters kept from the first line of a failure message in
/// inq_failure_summary. Long first lines (giant assertion diffs) are trimmed
/// with an ellipsis; the full message remains available via inq_test.
const SUMMARY_FIRST_LINE_CHARS: usize = 200;

/// Shorten a single line to at most `MAX_LINE_CHARS` characters (head + ellipsis).
/// Operates on char boundaries so UTF-8 sequences are never split.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    let omitted = line.chars().count() - MAX_LINE_CHARS;
    format!("{head}…({omitted} chars omitted)")
}

/// Truncate a multi-line string to roughly `max_lines` lines by keeping the head
/// and tail and eliding the middle. Each surviving line is also capped at
/// `MAX_LINE_CHARS` so a single pathological line can't dominate the output.
/// Returns the input unchanged when `max_lines` is 0. Preserves UTF-8 boundaries.
fn head_tail_truncate(s: &str, max_lines: usize) -> String {
    if max_lines == 0 {
        return s.to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return lines
            .iter()
            .map(|l| truncate_line(l))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let head_count = (max_lines * 2) / 3;
    let tail_count = max_lines - head_count;
    let omitted = lines.len() - head_count - tail_count;
    let head: Vec<String> = lines
        .iter()
        .take(head_count)
        .map(|l| truncate_line(l))
        .collect();
    let tail: Vec<String> = lines
        .iter()
        .skip(lines.len() - tail_count)
        .map(|l| truncate_line(l))
        .collect();
    format!(
        "{}\n…({omitted} lines omitted)…\n{}",
        head.join("\n"),
        tail.join("\n")
    )
}

/// Derive progress estimates for an in-progress run from historical test times.
///
/// Returns `(total_expected, percent_complete, estimated_remaining_secs)`. Each
/// element is `None` when it can't be estimated — specifically, all three are
/// `None` when no historical data is available (first run in a new repo).
///
/// `percent_complete` is in `[0, 1]`. `estimated_remaining_secs` sums the
/// historical durations of tests that haven't yet appeared in the run's
/// observed results. It ignores tests currently executing — at worst the ETA
/// is slightly pessimistic by one test's duration.
fn estimate_progress(
    historical: &std::collections::HashMap<crate::repository::TestId, Duration>,
    test_run: &crate::repository::TestRun,
) -> (Option<usize>, Option<f64>, Option<f64>) {
    if historical.is_empty() {
        return (None, None, None);
    }
    let total_expected = historical.len();
    let observed = test_run.total_tests();
    let percent = if total_expected > 0 {
        Some((observed as f64 / total_expected as f64).min(1.0))
    } else {
        None
    };
    let remaining: Duration = historical
        .iter()
        .filter(|(id, _)| !test_run.results.contains_key(*id))
        .map(|(_, d)| *d)
        .sum();
    (Some(total_expected), percent, Some(remaining.as_secs_f64()))
}

/// Extract the first non-empty line of a message, trimmed and capped at
/// `SUMMARY_FIRST_LINE_CHARS`. Returns `None` if no non-empty line exists.
/// Used by inq_failure_summary to offer a signal-rich one-liner per failure
/// without dragging the full traceback into the response.
fn first_nonempty_line(s: &str) -> Option<String> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= SUMMARY_FIRST_LINE_CHARS {
        Some(line.to_string())
    } else {
        let head: String = line.chars().take(SUMMARY_FIRST_LINE_CHARS).collect();
        Some(format!("{head}…"))
    }
}

#[derive(Serialize)]
struct RunSummary {
    id: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
}

#[derive(Serialize)]
struct StatsResponse {
    run_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_run: Option<RunSummary>,
}

#[derive(Serialize)]
struct FailingResponse {
    count: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    tests: Vec<String>,
}

#[derive(Serialize)]
struct LastResponse {
    id: String,
    timestamp: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failing_tests: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failing_truncated: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interruption: Option<String>,
}

#[derive(Serialize)]
struct StatusChange {
    test_id: String,
    old_status: String,
    new_status: String,
}

#[derive(Serialize)]
struct DiffResponse {
    run1: String,
    run2: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    new_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    new_passes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    status_changed: Vec<StatusChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added_tests: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    removed_tests: Vec<String>,
    #[serde(skip_serializing_if = "DiffTruncated::is_empty")]
    truncated: DiffTruncated,
}

#[derive(Serialize, Default)]
struct DiffTruncated {
    #[serde(skip_serializing_if = "Option::is_none")]
    new_failures: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_passes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_changed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    added_tests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    removed_tests: Option<usize>,
}

impl DiffTruncated {
    fn is_empty(&self) -> bool {
        self.new_failures.is_none()
            && self.new_passes.is_none()
            && self.status_changed.is_none()
            && self.added_tests.is_none()
            && self.removed_tests.is_none()
    }
}

#[derive(Serialize)]
struct SlowTest {
    test_id: String,
    duration_secs: f64,
    percentage: f64,
}

#[derive(Serialize)]
struct SlowestResponse {
    total_time_secs: f64,
    tests: Vec<SlowTest>,
}

#[derive(Serialize)]
struct FlakyTest {
    test_id: String,
    runs: u32,
    failures: u32,
    transitions: u32,
    flakiness_score: f64,
    failure_rate: f64,
}

#[derive(Serialize)]
struct FlakyResponse {
    /// Tests with at least `min_runs` recorded runs that qualified for ranking.
    total_candidates: usize,
    /// Number of `tests` returned (may be smaller than `total_candidates` when
    /// `count` truncated the list).
    count: usize,
    /// How many candidates were dropped because of the `count` limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    /// The `min_runs` threshold actually used. Surfaced so callers can tell
    /// when the default kicked in.
    min_runs: usize,
    tests: Vec<FlakyTest>,
}

#[derive(Serialize)]
struct LogEntry {
    test_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

#[derive(Serialize)]
struct LogResponse {
    run_id: String,
    count: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    results: Vec<LogEntry>,
}

#[derive(Serialize)]
struct FailureSummaryEntry {
    test_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_first_line: Option<String>,
}

#[derive(Serialize)]
struct FailureSummaryResponse {
    run_id: String,
    total_failing: usize,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    failures: Vec<FailureSummaryEntry>,
}

#[derive(Serialize)]
struct TestLookupResponse {
    run_id: String,
    test_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

/// One result in a batch lookup — same shape as the single-test response but
/// without the shared `run_id` (it's reported once at the top level).
#[derive(Serialize)]
struct TestBatchEntry {
    test_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

#[derive(Serialize)]
struct TestBatchResponse {
    run_id: String,
    count: usize,
    results: Vec<TestBatchEntry>,
    /// Test IDs from the request that weren't present in this run. Omitted
    /// when every requested ID was found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    not_found: Vec<String>,
}

#[derive(Serialize)]
struct RunResponse {
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    passed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failing_tests: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failing_truncated: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interruption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Captured child-process stderr, truncated head+tail. Populated only when
    /// `exit_code != 0` and stderr was non-empty — typically indicates a
    /// failure outside the subunit stream (compile error, collection error,
    /// pre-test panic).
    #[serde(skip_serializing_if = "Option::is_none")]
    error_output: Option<String>,
}

#[derive(Serialize)]
struct BackgroundStartedResponse {
    status: &'static str,
    run_id: String,
}

#[derive(Serialize)]
struct ListTestsResponse {
    count: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    tests: Vec<String>,
}

#[derive(Serialize)]
struct InfoResponse {
    id: String,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wall_duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    total_tests: usize,
    passed: usize,
    failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_test_time_secs: Option<f64>,
}

/// Project-level test configuration as the MCP server sees it. Lets callers
/// understand what inq_run will actually execute before triggering it.
#[derive(Serialize)]
struct ConfigResponse {
    /// Absolute path of the config file the server loaded.
    config_path: String,
    /// Directory in which test commands execute (the project root).
    working_dir: String,
    /// Raw shell command that runs the tests, with $LISTOPT/$IDLIST
    /// placeholders still present.
    test_command: String,
    /// When true, inq_list_tests and filtered inq_run can discover test IDs.
    supports_listing: bool,
    /// When true, inq_run can run a specific subset of tests by ID.
    supports_targeted_runs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_list_option: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_id_option: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_output_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_regex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_run_concurrency: Option<String>,
}

/// One row in the run-listing response. Compact — callers can fetch full
/// metadata with inq_info if they want git_commit, command, etc.
#[derive(Serialize)]
struct RunListEntry {
    id: String,
    timestamp: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct ListRunsResponse {
    count: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<usize>,
    runs: Vec<RunListEntry>,
}

/// One row in the per-test history response.
#[derive(Serialize)]
struct TestHistoryEntry {
    run_id: String,
    timestamp: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_first_line: Option<String>,
}

#[derive(Serialize)]
struct TestHistoryResponse {
    test_id: String,
    /// Number of runs inspected (may be less than `limit` when the repository
    /// has fewer runs). Runs where the test was absent are skipped and **not**
    /// counted here.
    count: usize,
    /// Newest run first.
    history: Vec<TestHistoryEntry>,
}

#[derive(Serialize)]
struct RunningEntry {
    id: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    elapsed_secs: i64,
    /// Historical test count. Omitted when no history is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_expected: Option<usize>,
    /// Fraction complete in [0, 1]. Omitted when total_expected is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    percent_complete: Option<f64>,
    /// Rough ETA in seconds, from the historical durations of tests not yet
    /// observed in this run. Omitted when historical data is thin.
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_remaining_secs: Option<f64>,
}

#[derive(Serialize)]
struct RunningResponse {
    count: usize,
    runs: Vec<RunningEntry>,
}

#[derive(Serialize)]
struct WaitMatchingTest {
    test_id: String,
    status: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum WaitResponse {
    Completed {
        status: &'static str,
        message: &'static str,
    },
    EarlyReturn {
        status: &'static str,
        reason: &'static str,
        run_id: String,
        total_tests: usize,
        passed: usize,
        failed: usize,
        matching_tests: Vec<WaitMatchingTest>,
        #[serde(skip_serializing_if = "Option::is_none")]
        matching_truncated: Option<usize>,
    },
    Timeout {
        status: &'static str,
        message: String,
        still_running: Vec<String>,
    },
}

#[derive(Serialize)]
struct CancelResponse {
    status: &'static str,
    run_id: String,
}

#[derive(Serialize)]
struct InitResponse {
    status: &'static str,
    path: String,
}

#[derive(Serialize)]
struct AutoResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct AnalyzeIsolationResponse {
    exit_code: i32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    output: Vec<String>,
}

/// Parse status filter strings into a set of TestStatus values.
///
/// Thin wrapper over `TestStatus::parse_filters` that maps the generic
/// error into `ErrorData::invalid_params` for MCP responses.
fn parse_status_filters(filters: &[String]) -> Result<Vec<TestStatus>, ErrorData> {
    TestStatus::parse_filters(filters).map_err(|e| ErrorData::invalid_params(e.to_string(), None))
}

#[tool_router]
impl InquestMcpService {
    /// Show repository statistics including run count and latest run summary.
    #[tool(
        description = "Show repository statistics including run count and latest run summary",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_stats(&self) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_count = repo.count().map_err(to_mcp_err)?;
        let run_ids = repo.list_run_ids().map_err(to_mcp_err)?;

        let latest_run = if run_ids.is_empty() {
            None
        } else {
            let latest = repo.get_latest_run().map_err(to_mcp_err)?;
            Some(RunSummary {
                id: latest.id.as_str().to_string(),
                total_tests: latest.total_tests(),
                passed: latest.count_successes(),
                failed: latest.count_failures(),
                duration_secs: latest.total_duration().map(duration_secs),
            })
        };

        ok_json(&StatsResponse {
            run_count,
            latest_run,
        })
    }

    /// List currently failing tests from the repository.
    #[tool(
        description = "List currently failing tests from the repository. Paginated — default limit is 100.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_failing(
        &self,
        params: Parameters<ListParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let failing = repo.get_failing_tests().map_err(to_mcp_err)?;

        let total = failing.len();
        let offset = params.0.offset.unwrap_or(0);
        let limit = params.0.limit.unwrap_or(100);

        let tests: Vec<String> = failing
            .iter()
            .skip(offset)
            .take(limit)
            .map(|id| id.as_str().to_string())
            .collect();
        let returned = tests.len();
        let truncated = total.saturating_sub(offset + returned);

        ok_json(&FailingResponse {
            count: returned,
            total,
            truncated: (truncated > 0).then_some(truncated),
            tests,
        })
    }

    /// Show results from the last (or a specific) test run.
    #[tool(
        description = "Show results from the last (or a specific) test run including pass/fail counts, duration, and failing test details",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_last(&self, params: Parameters<RunIdParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        let all_failing: Vec<String> = test_run
            .get_failing_tests()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        let (failing_tests, failing_extra) = take_limited(all_failing, FAILING_LIST_LIMIT);

        ok_json(&LastResponse {
            id: test_run.id.as_str().to_string(),
            timestamp: test_run.timestamp.to_rfc3339(),
            total_tests: test_run.total_tests(),
            passed: test_run.count_successes(),
            failed: test_run.count_failures(),
            duration_secs: test_run.total_duration().map(duration_secs),
            failing_tests,
            failing_truncated: (failing_extra > 0).then_some(failing_extra),
            interruption: test_run.interruption.as_ref().map(|i| i.to_string()),
        })
    }

    /// Compare two test runs and show what changed.
    #[tool(
        description = "Compare two test runs and show new failures, new passes, added/removed tests, and status changes",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_diff(&self, params: Parameters<DiffParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;

        let (id1, id2) = match (&params.0.run1, &params.0.run2) {
            (Some(r1), Some(r2)) => (
                resolve_run_id(&*repo, Some(r1)).map_err(to_mcp_err)?,
                resolve_run_id(&*repo, Some(r2)).map_err(to_mcp_err)?,
            ),
            (Some(r1), None) => {
                let id1 = resolve_run_id(&*repo, Some(r1)).map_err(to_mcp_err)?;
                let id2 = resolve_run_id(&*repo, None).map_err(to_mcp_err)?;
                (id1, id2)
            }
            (None, None) => {
                let ids = repo.list_run_ids().map_err(to_mcp_err)?;
                if ids.len() < 2 {
                    return Err(ErrorData::invalid_params(
                        "Need at least 2 test runs to diff".to_string(),
                        None,
                    ));
                }
                (ids[ids.len() - 2].clone(), ids[ids.len() - 1].clone())
            }
            (None, Some(_)) => {
                return Err(ErrorData::invalid_params(
                    "run1 must be provided if run2 is specified".to_string(),
                    None,
                ));
            }
        };

        let run1 = repo.get_test_run(&id1).map_err(to_mcp_err)?;
        let run2 = repo.get_test_run(&id2).map_err(to_mcp_err)?;

        use std::collections::BTreeSet;
        let ids1: BTreeSet<&crate::repository::TestId> = run1.results.keys().collect();
        let ids2: BTreeSet<&crate::repository::TestId> = run2.results.keys().collect();

        let mut new_failures: Vec<String> = Vec::new();
        let mut new_passes: Vec<String> = Vec::new();
        let mut status_changed: Vec<StatusChange> = Vec::new();

        for id in ids1.intersection(&ids2) {
            let r1 = &run1.results[*id];
            let r2 = &run2.results[*id];
            if r1.status == r2.status {
                continue;
            }
            if r2.status.is_failure() && r1.status.is_success() {
                new_failures.push(r2.test_id.as_str().to_string());
            } else if r2.status.is_success() && r1.status.is_failure() {
                new_passes.push(r2.test_id.as_str().to_string());
            } else {
                status_changed.push(StatusChange {
                    test_id: r2.test_id.as_str().to_string(),
                    old_status: r1.status.to_string(),
                    new_status: r2.status.to_string(),
                });
            }
        }

        let added: Vec<String> = ids2
            .difference(&ids1)
            .map(|id| id.as_str().to_string())
            .collect();
        let removed: Vec<String> = ids1
            .difference(&ids2)
            .map(|id| id.as_str().to_string())
            .collect();

        let limit = params.0.limit.unwrap_or(50);
        let (new_failures, nf_extra) = take_limited(new_failures, limit);
        let (new_passes, np_extra) = take_limited(new_passes, limit);
        let (status_changed, sc_extra) = take_limited(status_changed, limit);
        let (added_tests, ad_extra) = take_limited(added, limit);
        let (removed_tests, rm_extra) = take_limited(removed, limit);

        ok_json(&DiffResponse {
            run1: id1.as_str().to_string(),
            run2: id2.as_str().to_string(),
            new_failures,
            new_passes,
            status_changed,
            added_tests,
            removed_tests,
            truncated: DiffTruncated {
                new_failures: (nf_extra > 0).then_some(nf_extra),
                new_passes: (np_extra > 0).then_some(np_extra),
                status_changed: (sc_extra > 0).then_some(sc_extra),
                added_tests: (ad_extra > 0).then_some(ad_extra),
                removed_tests: (rm_extra > 0).then_some(rm_extra),
            },
        })
    }

    /// Show the slowest tests from the last run.
    #[tool(
        description = "Show the slowest tests from the last run with timing information",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_slowest(
        &self,
        params: Parameters<SlowestParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let test_run = repo.get_latest_run().map_err(to_mcp_err)?;

        let mut tests_with_duration: Vec<(&str, Duration)> = test_run
            .results
            .values()
            .filter_map(|r| r.duration.map(|dur| (r.test_id.as_str(), dur)))
            .collect();

        tests_with_duration.sort_by(|a, b| b.1.cmp(&a.1));

        let total_secs: f64 = tests_with_duration
            .iter()
            .map(|&(_, d)| d.as_secs_f64())
            .sum();
        let count = params.0.count.unwrap_or(10).min(tests_with_duration.len());

        let tests: Vec<SlowTest> = tests_with_duration
            .iter()
            .take(count)
            .map(|&(id, dur)| {
                let secs = dur.as_secs_f64();
                SlowTest {
                    test_id: id.to_string(),
                    duration_secs: secs,
                    percentage: if total_secs > 0.0 {
                        (secs / total_secs) * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect();

        ok_json(&SlowestResponse {
            total_time_secs: total_secs,
            tests,
        })
    }

    /// Rank tests by flakiness across recorded runs.
    #[tool(
        description = "Rank tests by flakiness across recorded runs. Flakiness is measured \
                        by pass↔fail transitions in consecutive runs in which the test ran, \
                        so chronically broken tests rank low and genuinely flapping ones \
                        rank high. Each entry includes raw counts (runs, failures, \
                        transitions) plus normalised flakiness_score and failure_rate \
                        in [0, 1]. Tests with fewer than min_runs recorded runs are \
                        excluded — defaults: count=10, min_runs=5.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_flaky(&self, params: Parameters<FlakyParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let min_runs = params.0.min_runs.unwrap_or(5);
        let count = params.0.count.unwrap_or(10);

        let stats = repo.get_flakiness(min_runs).map_err(to_mcp_err)?;
        let total_candidates = stats.len();
        let returned = count.min(total_candidates);
        let truncated = total_candidates.saturating_sub(returned);

        let tests: Vec<FlakyTest> = stats
            .into_iter()
            .take(returned)
            .map(|s| FlakyTest {
                test_id: s.test_id.as_str().to_string(),
                runs: s.runs,
                failures: s.failures,
                transitions: s.transitions,
                flakiness_score: s.flakiness_score,
                failure_rate: s.failure_rate,
            })
            .collect();

        ok_json(&FlakyResponse {
            total_candidates,
            count: returned,
            truncated: (truncated > 0).then_some(truncated),
            min_runs,
            tests,
        })
    }

    /// Show test details and tracebacks.
    #[tool(
        description = "Show test details (status, duration) for matching tests. Paginated — \
                        default limit is 20. Failure messages and tracebacks are omitted \
                        unless include_details=true.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_log(&self, params: Parameters<LogParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;

        let raw_stream = repo.get_test_run_raw(&run_id).map_err(to_mcp_err)?;

        let test_run = subunit_stream::parse_stream_with_progress(
            raw_stream,
            run_id.clone(),
            |_test_id, _status| {},
            |_bytes| {},
            subunit_stream::OutputFilter::All,
        )
        .map_err(to_mcp_err)?;

        let patterns: Vec<glob::Pattern> = params
            .0
            .test_patterns
            .unwrap_or_default()
            .iter()
            .map(|p| glob::Pattern::new(p))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::invalid_params(format!("Invalid glob pattern: {}", e), None))?;

        let status_filters = parse_status_filters(&params.0.status_filter.unwrap_or_default())?;
        let limit = params.0.limit.unwrap_or(20);
        let include_details = params.0.include_details.unwrap_or(false);
        let max_detail_lines = params.0.max_detail_lines.unwrap_or(60);

        let mut matching: Vec<_> = test_run
            .results
            .values()
            .filter(|r| {
                let name_match =
                    patterns.is_empty() || patterns.iter().any(|p| p.matches(r.test_id.as_str()));
                let status_match = status_filters.is_empty() || status_filters.contains(&r.status);
                name_match && status_match
            })
            .collect();
        matching.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));

        let total = matching.len();
        let truncated = total.saturating_sub(limit);

        let results: Vec<LogEntry> = matching
            .iter()
            .take(limit)
            .map(|r| LogEntry {
                test_id: r.test_id.as_str().to_string(),
                status: r.status.to_string(),
                duration_secs: r.duration.map(duration_secs),
                message: if include_details {
                    r.message
                        .as_ref()
                        .map(|m| head_tail_truncate(m, max_detail_lines))
                } else {
                    None
                },
                details: if include_details {
                    r.details
                        .as_ref()
                        .map(|d| head_tail_truncate(d, max_detail_lines))
                } else {
                    None
                },
            })
            .collect();

        ok_json(&LogResponse {
            run_id: run_id.as_str().to_string(),
            count: results.len(),
            total,
            truncated: (truncated > 0).then_some(truncated),
            results,
        })
    }

    /// Concise triage view of a run's failures.
    #[tool(
        description = "List a run's failing tests with the first line of each failure message. \
                        Default limit is 25. Use this for triage before drilling into a specific \
                        failure with inq_test.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_failure_summary(
        &self,
        params: Parameters<FailureSummaryParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        let mut failing: Vec<&TestResult> = test_run
            .results
            .values()
            .filter(|r| r.status.is_failure())
            .collect();
        failing.sort_by(|a, b| a.test_id.as_str().cmp(b.test_id.as_str()));
        let total_failing = failing.len();

        let limit = params.0.limit.unwrap_or(FAILURE_SUMMARY_LIMIT);
        let truncated = total_failing.saturating_sub(limit);

        let failures: Vec<FailureSummaryEntry> = failing
            .iter()
            .take(limit)
            .map(|r| FailureSummaryEntry {
                test_id: r.test_id.as_str().to_string(),
                status: r.status.to_string(),
                message_first_line: r.message.as_deref().and_then(first_nonempty_line),
            })
            .collect();

        ok_json(&FailureSummaryResponse {
            run_id: run_id.as_str().to_string(),
            total_failing,
            count: failures.len(),
            truncated: (truncated > 0).then_some(truncated),
            failures,
        })
    }

    /// Fetch the full detail of a single test from a run.
    #[tool(
        description = "Fetch one test's status, duration, and (truncated) message/details by \
                        exact test ID. Typically used after inq_failure_summary surfaces a test \
                        you want to investigate. Errors if the test ID isn't present in the run.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_test(
        &self,
        params: Parameters<TestLookupParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        let test_id = crate::repository::TestId::new(&params.0.test_id);
        let result = test_run.results.get(&test_id).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "Test '{}' not found in run '{}'",
                    params.0.test_id,
                    run_id.as_str()
                ),
                None,
            )
        })?;

        let max_detail_lines = params.0.max_detail_lines.unwrap_or(60);

        ok_json(&TestLookupResponse {
            run_id: run_id.as_str().to_string(),
            test_id: result.test_id.as_str().to_string(),
            status: result.status.to_string(),
            duration_secs: result.duration.map(duration_secs),
            message: result
                .message
                .as_ref()
                .map(|m| head_tail_truncate(m, max_detail_lines)),
            details: result
                .details
                .as_ref()
                .map(|d| head_tail_truncate(d, max_detail_lines)),
        })
    }

    /// Fetch details for several tests from the same run in one call.
    #[tool(
        description = "Fetch status/duration/message/details for multiple tests from a single \
                        run. Prefer this to calling inq_test in a loop when triaging several \
                        failures. Unknown test IDs are reported in `not_found` instead of \
                        failing the call; duplicate IDs are de-duplicated.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_test_batch(
        &self,
        params: Parameters<TestBatchLookupParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        let max_detail_lines = params.0.max_detail_lines.unwrap_or(60);

        // De-duplicate while preserving first-seen order so callers can reason
        // about positional output even though we don't guarantee it.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut unique_ids: Vec<String> = Vec::with_capacity(params.0.test_ids.len());
        for id in params.0.test_ids {
            if seen.insert(id.clone()) {
                unique_ids.push(id);
            }
        }

        let mut results = Vec::new();
        let mut not_found = Vec::new();
        for id_str in unique_ids {
            let test_id = crate::repository::TestId::new(&id_str);
            match test_run.results.get(&test_id) {
                Some(r) => results.push(TestBatchEntry {
                    test_id: r.test_id.as_str().to_string(),
                    status: r.status.to_string(),
                    duration_secs: r.duration.map(duration_secs),
                    message: r
                        .message
                        .as_ref()
                        .map(|m| head_tail_truncate(m, max_detail_lines)),
                    details: r
                        .details
                        .as_ref()
                        .map(|d| head_tail_truncate(d, max_detail_lines)),
                }),
                None => not_found.push(id_str),
            }
        }

        ok_json(&TestBatchResponse {
            run_id: run_id.as_str().to_string(),
            count: results.len(),
            results,
            not_found,
        })
    }

    /// Execute tests and return results.
    #[tool(
        description = "Execute tests and return structured results. Requires a configuration file (inquest.toml or .testr.conf) in the project directory. \
                        \n\n\
                        Long-running runs: when the client attaches a progressToken to the request's _meta, the server emits periodic notifications/progress \
                        messages so compliant MCP clients reset their tool-call timer — this keeps foreground runs alive past the default timeout. \
                        If you can't rely on the client handling progress notifications (Claude Code's MCP_TOOL_TIMEOUT defaults to 2 minutes), \
                        use background=true instead: the call returns immediately with a run ID, and you can poll with inq_wait / inq_running and read \
                        results with inq_last / inq_log.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn inq_run(
        &self,
        params: Parameters<RunParam>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, ErrorData> {
        let reporter = ProgressReporter::from_meta(&peer, &meta);
        self.inq_run_impl(params.0, reporter).await
    }

    async fn inq_run_impl(
        &self,
        params: RunParam,
        reporter: Option<ProgressReporter>,
    ) -> Result<CallToolResult, ErrorData> {
        let base = &self.directory;

        struct NullUI;
        impl crate::ui::UI for NullUI {
            fn output(&mut self, _: &str) -> crate::error::Result<()> {
                Ok(())
            }
            fn error(&mut self, _: &str) -> crate::error::Result<()> {
                Ok(())
            }
            fn warning(&mut self, _: &str) -> crate::error::Result<()> {
                Ok(())
            }
        }

        let failing_only = params.failing_only.unwrap_or(false);
        let test_order = match &params.order {
            Some(s) => Some(
                s.parse::<crate::ordering::TestOrder>()
                    .map_err(to_mcp_err)?,
            ),
            None => None,
        };
        let partial = failing_only;
        let test_filters = params.test_filters.filter(|f| !f.is_empty());
        let background = params.background.unwrap_or(false);

        if background {
            let mut repo = open_or_init_repository(Some(&self.dir_str()), true, &mut NullUI)
                .map_err(|e| {
                    ErrorData::internal_error(format!("Failed to open repository: {}", e), None)
                })?;

            let base_path = base.to_string_lossy().to_string();
            let test_cmd = TestCommand::from_directory(base).map_err(|e| {
                ErrorData::internal_error(format!("Failed to load config: {}", e), None)
            })?;

            let historical_times = repo.get_test_times().map_err(to_mcp_err)?;

            // Resolve test IDs before spawning (needs repo)
            let mut test_ids = if failing_only {
                let failing = repo.get_failing_tests().map_err(to_mcp_err)?;
                if failing.is_empty() {
                    return ok_json(&RunResponse {
                        exit_code: 0,
                        message: Some("No failing tests to run".to_string()),
                        id: None,
                        total_tests: None,
                        passed: None,
                        failed: None,
                        duration_secs: None,
                        failing_tests: Vec::new(),
                        failing_truncated: None,
                        interruption: None,
                        error_output: None,
                    });
                }
                Some(failing)
            } else {
                None
            };

            if let Some(filters) = test_filters {
                let compiled: Vec<regex::Regex> = filters
                    .iter()
                    .map(|p| regex::Regex::new(p))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| {
                        ErrorData::invalid_params(format!("Invalid test filter regex: {}", e), None)
                    })?;
                let all_ids = if let Some(ids) = test_ids.take() {
                    ids
                } else {
                    test_cmd.list_tests().map_err(|e| {
                        ErrorData::internal_error(format!("Failed to list tests: {}", e), None)
                    })?
                };
                test_ids = Some(
                    all_ids
                        .into_iter()
                        .filter(|id| compiled.iter().any(|re| re.is_match(id.as_str())))
                        .collect(),
                );
            }

            let concurrency = params.concurrency.unwrap_or(1);

            // Pre-allocate the run — this creates the lock file so inq_running sees it
            let (run_id, writer) = repo.begin_test_run_raw().map_err(to_mcp_err)?;
            let run_id_for_response = run_id.clone();

            // Create cancellation token and store it for inq_cancel
            let cancel_token = CancellationToken::new();
            self.cancel_tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(run_id_for_response.clone(), cancel_token.clone());

            // Drop the repo — the background thread will open its own for persistence
            let dir_for_persist = self.dir_str();
            let cancel_tokens = self.cancel_tokens.clone();
            let run_id_for_cleanup = run_id_for_response.clone();
            drop(repo);

            tokio::task::spawn_blocking(move || {
                let mut ui = NullUI;
                let config = crate::test_executor::TestExecutorConfig {
                    base_path: Some(base_path),
                    all_output: false,
                    test_args: None,
                    cancellation_token: Some(cancel_token),
                    max_restarts: None,
                    stderr_capture: None,
                };
                let executor = crate::test_executor::TestExecutor::new(&config);

                let output = if concurrency > 1 {
                    let dir = config.base_path.as_ref().unwrap().clone();
                    executor.run_parallel(
                        &mut ui,
                        &test_cmd,
                        test_ids.as_deref(),
                        concurrency,
                        None,
                        None,
                        None,
                        run_id,
                        &historical_times,
                        || {
                            let mut repo = crate::commands::utils::open_repository(Some(&dir))?;
                            repo.begin_test_run_raw().map(|(_, w)| w)
                        },
                    )
                } else {
                    executor.run_serial(
                        &mut ui,
                        &test_cmd,
                        test_ids.as_deref(),
                        None,
                        None,
                        None,
                        run_id,
                        writer,
                        &historical_times,
                    )
                };

                match output {
                    Ok(output) => {
                        match crate::commands::utils::open_repository(Some(&dir_for_persist)) {
                            Ok(mut repo) => {
                                if let Err(e) = crate::commands::utils::persist_and_display_run(
                                    &mut ui,
                                    repo.as_mut(),
                                    output,
                                    partial,
                                    &historical_times,
                                    &[],
                                ) {
                                    tracing::error!(
                                        "Failed to persist background run results: {}",
                                        e
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to reopen repository for background run: {}",
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Background test execution failed: {}", e);
                    }
                }

                cancel_tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&run_id_for_cleanup);
            });

            return ok_json(&BackgroundStartedResponse {
                status: "started",
                run_id: run_id_for_response.as_str().to_string(),
            });
        }

        // Foreground execution. Blocking work lives on a dedicated thread so
        // the MCP service loop can still handle pings, cancellations, and
        // other tool calls while a long suite runs. When the client attaches
        // a progressToken, `run_blocking_with_progress` also keeps the
        // client's tool-call timer alive with periodic progress notifications.
        let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let stderr_capture_for_task = stderr_capture.clone();
        let base_path = base.to_string_lossy().to_string();
        let concurrency = params.concurrency;
        let background_after = params.background_after.map(Duration::from_secs);

        // Shared slot so this async context can observe the run ID as soon as
        // `RunCommand` allocates it — needed by the `background_after` path to
        // hand back a handle before the run finishes.
        let run_id_slot: Arc<Mutex<Option<crate::repository::RunId>>> = Arc::new(Mutex::new(None));
        let run_id_slot_for_cmd = run_id_slot.clone();
        // Token forwarded into the executor so `inq_cancel` can stop a run we
        // hand off to the caller on a `background_after` timeout.
        let cancel_token = CancellationToken::new();
        let cancel_token_for_cmd = cancel_token.clone();

        let run_work = move || {
            let mut ui = NullUI;
            let cmd = crate::commands::RunCommand {
                base_path: Some(base_path),
                partial,
                failing_only,
                force_init: true,
                concurrency,
                test_filters,
                test_order,
                stderr_capture: Some(stderr_capture_for_task),
                run_id_slot: Some(run_id_slot_for_cmd),
                cancellation_token: Some(cancel_token_for_cmd),
                ..Default::default()
            };
            cmd.execute_returning_run_id(&mut ui)
        };

        let cli_output = if let Some(timeout) = background_after {
            // Race the run against the background_after timeout. If the timer
            // wins and we already know the run ID, hand the caller a handle
            // and let the blocking work finish in a detached task. If the run
            // ID isn't allocated yet (timeout hit during repo setup or test
            // discovery), fall through and wait for the work as usual.
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let keepalive = reporter
                .clone()
                .map(|r| tokio::spawn(progress_keepalive_loop(r, stop_rx, "running tests")));
            let mut join_handle = tokio::task::spawn_blocking(run_work);

            let completed = tokio::select! {
                result = &mut join_handle => Some(result),
                _ = tokio::time::sleep(timeout) => None,
            };
            let _ = stop_tx.send(());
            if let Some(h) = keepalive {
                let _ = h.await;
            }

            match completed {
                Some(result) => result
                    .map_err(|e| {
                        ErrorData::internal_error(format!("Test execution panicked: {}", e), None)
                    })?
                    .map_err(|e| {
                        ErrorData::internal_error(format!("Test execution failed: {}", e), None)
                    })?,
                None => {
                    let run_id = run_id_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(rid) = run_id {
                        self.cancel_tokens
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(rid.clone(), cancel_token.clone());
                        let cancel_tokens = self.cancel_tokens.clone();
                        let rid_for_cleanup = rid.clone();
                        tokio::spawn(async move {
                            if let Err(e) = join_handle.await {
                                tracing::error!("Backgrounded test run task panicked: {}", e);
                            }
                            cancel_tokens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&rid_for_cleanup);
                        });
                        return ok_json(&BackgroundStartedResponse {
                            status: "running",
                            run_id: rid.as_str().to_string(),
                        });
                    }
                    // Run ID not allocated yet — wait it out.
                    join_handle
                        .await
                        .map_err(|e| {
                            ErrorData::internal_error(
                                format!("Test execution panicked: {}", e),
                                None,
                            )
                        })?
                        .map_err(|e| {
                            ErrorData::internal_error(format!("Test execution failed: {}", e), None)
                        })?
                }
            }
        } else {
            run_blocking_with_progress(reporter, "running tests", run_work)
                .await
                .map_err(|e| {
                    ErrorData::internal_error(format!("Test execution panicked: {}", e), None)
                })?
                .map_err(|e| {
                    ErrorData::internal_error(format!("Test execution failed: {}", e), None)
                })?
        };

        // Pull out captured stderr once. Exposed only when the run failed —
        // a successful run's noisy stderr is rarely useful and would just
        // balloon the response.
        let error_output = if cli_output.exit_code != 0 {
            let bytes = stderr_capture.lock().map(|g| g.clone()).unwrap_or_default();
            if bytes.is_empty() {
                None
            } else {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Some(head_tail_truncate(&text, ERROR_OUTPUT_LINE_BUDGET))
            }
        } else {
            None
        };

        if let Some(ref run_id) = cli_output.run_id {
            let repo = self.open_repo()?;
            let test_run = repo.get_test_run(run_id).map_err(to_mcp_err)?;

            let all_failing: Vec<String> = test_run
                .get_failing_tests()
                .iter()
                .map(|id| id.as_str().to_string())
                .collect();
            let (failing_tests, failing_extra) = take_limited(all_failing, FAILING_LIST_LIMIT);

            ok_json(&RunResponse {
                exit_code: cli_output.exit_code,
                id: Some(run_id.as_str().to_string()),
                total_tests: Some(test_run.total_tests()),
                passed: Some(test_run.count_successes()),
                failed: Some(test_run.count_failures()),
                duration_secs: test_run.total_duration().map(duration_secs),
                failing_tests,
                failing_truncated: (failing_extra > 0).then_some(failing_extra),
                interruption: test_run.interruption.as_ref().map(|i| i.to_string()),
                message: None,
                error_output,
            })
        } else {
            ok_json(&RunResponse {
                exit_code: cli_output.exit_code,
                id: None,
                total_tests: None,
                passed: None,
                failed: None,
                duration_secs: None,
                failing_tests: Vec::new(),
                failing_truncated: None,
                interruption: None,
                message: Some("No tests were executed".to_string()),
                error_output,
            })
        }
    }

    /// List available tests.
    #[tool(
        description = "List all available tests discovered by the test command. \
                        Paginated — default limit is 100.",
        annotations(read_only_hint = true, idempotent_hint = false, open_world_hint = true)
    )]
    async fn inq_list_tests(
        &self,
        params: Parameters<ListParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let test_cmd = TestCommand::from_directory(&self.directory).map_err(|e| {
            ErrorData::internal_error(format!("Failed to load config: {}", e), None)
        })?;

        let test_ids = test_cmd
            .list_tests()
            .map_err(|e| ErrorData::internal_error(format!("Failed to list tests: {}", e), None))?;

        let total = test_ids.len();
        let offset = params.0.offset.unwrap_or(0);
        let limit = params.0.limit.unwrap_or(100);
        let tests: Vec<String> = test_ids
            .iter()
            .skip(offset)
            .take(limit)
            .map(|id| id.as_str().to_string())
            .collect();
        let returned = tests.len();
        let truncated = total.saturating_sub(offset + returned);

        ok_json(&ListTestsResponse {
            count: returned,
            total,
            truncated: (truncated > 0).then_some(truncated),
            tests,
        })
    }

    /// Show detailed information about a test run including metadata.
    #[tool(
        description = "Show detailed information about a test run including git commit, command, concurrency, duration, and exit code",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_info(&self, params: Parameters<RunIdParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;
        let metadata = repo.get_run_metadata(&run_id).map_err(to_mcp_err)?;

        ok_json(&InfoResponse {
            id: run_id.as_str().to_string(),
            timestamp: test_run.timestamp.to_rfc3339(),
            git_commit: metadata.git_commit,
            git_dirty: metadata.git_dirty,
            command: metadata.command,
            concurrency: metadata.concurrency,
            wall_duration_secs: metadata.duration_secs,
            exit_code: metadata.exit_code,
            total_tests: test_run.total_tests(),
            passed: test_run.count_successes(),
            failed: test_run.count_failures(),
            total_test_time_secs: test_run.total_duration().map(duration_secs),
        })
    }

    /// Show the project's test command configuration.
    #[tool(
        description = "Show how inq_run will execute tests in this project: the resolved config \
                        file path, test_command with substitution placeholders, and whether the \
                        project supports test listing or targeted runs. Use this before calling \
                        inq_run when you need to understand the runtime, or before inq_list_tests \
                        to confirm the project exposes a listing option. Errors if no config \
                        file (inquest.toml, .inquest.toml, .testr.conf) is present.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_config(&self) -> Result<CallToolResult, ErrorData> {
        let (config, config_path) = crate::config::TestrConfig::find_in_directory(&self.directory)
            .map_err(|e| {
                ErrorData::invalid_params(format!("Failed to load test config: {}", e), None)
            })?;

        ok_json(&ConfigResponse {
            config_path: config_path.to_string_lossy().into_owned(),
            working_dir: self.directory.to_string_lossy().into_owned(),
            supports_listing: config.test_list_option.is_some(),
            supports_targeted_runs: config.test_id_option.is_some(),
            test_command: config.test_command,
            test_list_option: config.test_list_option,
            test_id_option: config.test_id_option,
            test_timeout: config.test_timeout,
            max_duration: config.max_duration,
            no_output_timeout: config.no_output_timeout,
            group_regex: config.group_regex,
            test_run_concurrency: config.test_run_concurrency,
        })
    }

    /// List recent test runs with compact per-run summaries.
    #[tool(
        description = "List recent test runs (newest first) with timestamp, pass/fail counts, \
                        duration, and exit code. Paginated — default limit is 20. Use inq_info \
                        to fetch full per-run metadata (git commit, command, etc.).",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_list_runs(
        &self,
        params: Parameters<ListRunsParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let mut run_ids = repo.list_run_ids().map_err(to_mcp_err)?;
        // Repository returns oldest-first; callers almost always want newest.
        run_ids.reverse();

        let total = run_ids.len();
        let offset = params.0.offset.unwrap_or(0);
        let limit = params.0.limit.unwrap_or(20);

        let runs: Vec<RunListEntry> = run_ids
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(|run_id| {
                let test_run = repo.get_test_run(run_id).ok()?;
                // Metadata is optional for legacy repos — fall back to None fields.
                let metadata = repo.get_run_metadata(run_id).unwrap_or_default();
                Some(RunListEntry {
                    id: run_id.as_str().to_string(),
                    timestamp: test_run.timestamp.to_rfc3339(),
                    total_tests: test_run.total_tests(),
                    passed: test_run.count_successes(),
                    failed: test_run.count_failures(),
                    duration_secs: test_run.total_duration().map(duration_secs),
                    exit_code: metadata.exit_code,
                })
            })
            .collect();

        let returned = runs.len();
        let truncated = total.saturating_sub(offset + returned);

        ok_json(&ListRunsResponse {
            count: returned,
            total,
            truncated: (truncated > 0).then_some(truncated),
            runs,
        })
    }

    /// Show the per-run history of a single test.
    #[tool(
        description = "Walk recent runs (newest first) and report this test's status in each. \
                        Useful for questions like 'when did test X start failing?'. Runs where \
                        the test wasn't recorded are skipped. Default limit scans the last 20 \
                        runs; unscanned older runs are not counted.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inq_test_history(
        &self,
        params: Parameters<TestHistoryParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let mut run_ids = repo.list_run_ids().map_err(to_mcp_err)?;
        run_ids.reverse();

        let limit = params.0.limit.unwrap_or(20);
        let test_id = crate::repository::TestId::new(&params.0.test_id);

        let history: Vec<TestHistoryEntry> = run_ids
            .iter()
            .take(limit)
            .filter_map(|run_id| {
                let test_run = repo.get_test_run(run_id).ok()?;
                let result = test_run.results.get(&test_id)?;
                Some(TestHistoryEntry {
                    run_id: run_id.as_str().to_string(),
                    timestamp: test_run.timestamp.to_rfc3339(),
                    status: result.status.to_string(),
                    duration_secs: result.duration.map(duration_secs),
                    message_first_line: result.message.as_deref().and_then(first_nonempty_line),
                })
            })
            .collect();

        ok_json(&TestHistoryResponse {
            test_id: params.0.test_id,
            count: history.len(),
            history,
        })
    }

    /// Show currently in-progress test runs.
    #[tool(
        description = "Show currently in-progress test runs with their status and progress",
        annotations(
            read_only_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn inq_running(&self) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_ids = repo.get_running_run_ids().map_err(to_mcp_err)?;

        // Historical test durations; used to derive total_expected and an ETA.
        // Empty map is fine — the helper below falls back to emitting None.
        let historical = repo.get_test_times().unwrap_or_default();

        let now = chrono::Utc::now();
        let runs: Vec<RunningEntry> = run_ids
            .iter()
            .filter_map(|run_id| {
                let test_run = repo.get_test_run(run_id).ok()?;
                let observed = test_run.total_tests();

                let (total_expected, percent_complete, estimated_remaining_secs) =
                    estimate_progress(&historical, &test_run);

                Some(RunningEntry {
                    id: run_id.as_str().to_string(),
                    total_tests: observed,
                    passed: test_run.count_successes(),
                    failed: test_run.count_failures(),
                    elapsed_secs: (now - test_run.timestamp).num_seconds(),
                    total_expected,
                    percent_complete,
                    estimated_remaining_secs,
                })
            })
            .collect();

        ok_json(&RunningResponse {
            count: runs.len(),
            runs,
        })
    }

    /// Wait for background test runs to complete.
    #[tool(
        description = "Wait for background test runs to complete. Returns when all runs finish, \
                        or early if status_filter is set and a running test matches that status \
                        (e.g. \"failing\"). Much more efficient than polling inq_running in a loop. \
                        Emits notifications/progress periodically when the client supplies a \
                        progressToken, so long waits don't trip the client's tool-call timeout.",
        annotations(
            read_only_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn inq_wait(
        &self,
        params: Parameters<WaitParam>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, ErrorData> {
        let reporter = ProgressReporter::from_meta(&peer, &meta);
        self.inq_wait_impl(params.0, reporter).await
    }

    async fn inq_wait_impl(
        &self,
        params: WaitParam,
        reporter: Option<ProgressReporter>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = Duration::from_secs(params.timeout_secs.unwrap_or(600));
        let target_run_id = params.run_id.as_ref().map(crate::repository::RunId::new);
        let status_filter = if let Some(ref filters) = params.status_filter {
            Some(parse_status_filters(filters)?)
        } else {
            None
        };

        let poll_interval = Duration::from_secs(2);
        let start = std::time::Instant::now();
        let mut next_progress_at = PROGRESS_KEEPALIVE_INTERVAL;
        let mut progress_counter: u64 = 0;

        loop {
            let repo = self.open_repo()?;
            let running_ids = repo.get_running_run_ids().map_err(to_mcp_err)?;

            let still_running = if let Some(ref target) = target_run_id {
                running_ids.contains(target)
            } else {
                !running_ids.is_empty()
            };

            if !still_running {
                return ok_json(&WaitResponse::Completed {
                    status: "completed",
                    message: "No matching runs are in progress",
                });
            }

            if let Some(ref statuses) = status_filter {
                let target_slice;
                let ids_to_check: &[crate::repository::RunId] =
                    if let Some(ref target) = target_run_id {
                        target_slice = [target.clone()];
                        &target_slice
                    } else {
                        &running_ids
                    };
                for run_id in ids_to_check {
                    if let Ok(test_run) = repo.get_test_run(run_id) {
                        let all_matching: Vec<WaitMatchingTest> = test_run
                            .results
                            .iter()
                            .filter(|(_, r)| statuses.contains(&r.status))
                            .map(|(id, r)| WaitMatchingTest {
                                test_id: id.as_str().to_string(),
                                status: format!("{:?}", r.status),
                            })
                            .collect();
                        if !all_matching.is_empty() {
                            let (matching_tests, matching_extra) =
                                take_limited(all_matching, MATCHING_LIST_LIMIT);
                            return ok_json(&WaitResponse::EarlyReturn {
                                status: "early_return",
                                reason: "Tests matching status filter found while run is still in progress",
                                run_id: run_id.as_str().to_string(),
                                total_tests: test_run.total_tests(),
                                passed: test_run.count_successes(),
                                failed: test_run.count_failures(),
                                matching_tests,
                                matching_truncated: (matching_extra > 0).then_some(matching_extra),
                            });
                        }
                    }
                }
            }

            drop(repo);

            if start.elapsed() >= timeout {
                return ok_json(&WaitResponse::Timeout {
                    status: "timeout",
                    message: format!(
                        "Timed out after {} seconds, runs still in progress",
                        timeout.as_secs()
                    ),
                    still_running: running_ids
                        .iter()
                        .map(|id| id.as_str().to_string())
                        .collect(),
                });
            }

            // Keep the client's tool-call timer alive on long waits. Throttled
            // to PROGRESS_KEEPALIVE_INTERVAL rather than every poll so we
            // don't spam a client that's waiting on a fast-changing run.
            if let Some(ref reporter) = reporter {
                let elapsed = start.elapsed();
                if elapsed >= next_progress_at {
                    progress_counter += 1;
                    reporter
                        .notify(
                            progress_counter,
                            format!("waiting for test runs (elapsed: {}s)", elapsed.as_secs()),
                        )
                        .await;
                    next_progress_at = elapsed + PROGRESS_KEEPALIVE_INTERVAL;
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Cancel a running background test execution.
    #[tool(
        description = "Cancel a background test run. Use inq_running to find the run ID of in-progress runs.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn inq_cancel(
        &self,
        params: Parameters<CancelParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let run_id = &params.0.run_id;
        let run_id_key = crate::repository::RunId::new(run_id);

        let token = self
            .cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&run_id_key)
            .cloned();

        if let Some(token) = token {
            token.cancel();
            ok_json(&CancelResponse {
                status: "cancelling",
                run_id: run_id.clone(),
            })
        } else {
            Err(ErrorData::invalid_params(
                format!(
                    "No cancellable background run with ID '{}'. \
                     Only runs started with background=true can be cancelled via this tool.",
                    run_id
                ),
                None,
            ))
        }
    }

    /// Initialize a new test repository.
    #[tool(
        description = "Initialize a new .inquest test repository in the project directory",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn inq_init(&self) -> Result<CallToolResult, ErrorData> {
        let factory = InquestRepositoryFactory;
        match factory.initialise(&self.directory) {
            Ok(_) => ok_json(&InitResponse {
                status: "initialized",
                path: self
                    .directory
                    .join(".inquest")
                    .to_string_lossy()
                    .into_owned(),
            }),
            Err(e) => Err(ErrorData::internal_error(
                format!("Failed to initialize repository: {}", e),
                None,
            )),
        }
    }

    /// Auto-detect project type and generate configuration.
    #[tool(
        description = "Auto-detect project type (Cargo, pytest, unittest, Perl/prove) and generate an inquest.toml configuration file",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn inq_auto(&self) -> Result<CallToolResult, ErrorData> {
        let dir = self.dir_str();
        let (exit_code, ui) = tokio::task::spawn_blocking(move || {
            let mut ui = CollectUI::new();
            let cmd = crate::commands::AutoCommand::new(Some(dir));
            use crate::commands::Command;
            let result = cmd.execute(&mut ui);
            (result, ui)
        })
        .await
        .map(|(result, ui)| result.map(|code| (code, ui)))
        .map_err(|e| ErrorData::internal_error(format!("inq_auto panicked: {}", e), None))?
        .map_err(to_mcp_err)?;

        if exit_code != 0 {
            let msg = if ui.errors.is_empty() {
                "Auto-detection failed".to_string()
            } else {
                ui.errors.join("; ")
            };
            return Err(ErrorData::internal_error(msg, None));
        }

        let message = ui.output.join("\n");
        ok_json(&AutoResponse {
            status: "created",
            message: (!message.is_empty()).then_some(message),
        })
    }

    /// Analyze test isolation issues using bisection.
    #[tool(
        description = "Analyze test isolation issues by bisecting the test suite to find which tests cause a target test to fail when run together but pass in isolation",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn inq_analyze_isolation(
        &self,
        params: Parameters<AnalyzeIsolationParam>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, ErrorData> {
        let reporter = ProgressReporter::from_meta(&peer, &meta);
        self.inq_analyze_isolation_impl(params.0, reporter).await
    }

    async fn inq_analyze_isolation_impl(
        &self,
        params: AnalyzeIsolationParam,
        reporter: Option<ProgressReporter>,
    ) -> Result<CallToolResult, ErrorData> {
        let dir = self.dir_str();
        let test = params.test;
        let (exit_code, ui) =
            run_blocking_with_progress(reporter, "analyzing test isolation", move || {
                let mut ui = CollectUI::new();
                let cmd = crate::commands::AnalyzeIsolationCommand::new(Some(dir), test);
                use crate::commands::Command;
                let result = cmd.execute(&mut ui);
                (result, ui)
            })
            .await
            .map(|(result, ui)| result.map(|code| (code, ui)))
            .map_err(|e| {
                ErrorData::internal_error(format!("inq_analyze_isolation panicked: {}", e), None)
            })?
            .map_err(to_mcp_err)?;

        let mut all_output = ui.output;
        all_output.extend(ui.errors);
        ok_json(&AnalyzeIsolationResponse {
            exit_code,
            output: all_output,
        })
    }
}

#[tool_handler]
impl ServerHandler for InquestMcpService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("inquest", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Inquest test repository server. Provides access to test results, \
                 failing tests, statistics, and test execution. \
                 For failure triage, prefer inq_failure_summary to get a compact \
                 list of failing tests with a one-line message each, then inq_test \
                 to expand a specific failure's full (truncated) traceback. Use \
                 inq_log only when you need pattern-based search across a run."
                    .to_string(),
            )
    }
}

/// Run the MCP server over stdio.
pub async fn serve(directory: PathBuf) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let service = InquestMcpService::new(directory);
    let server = service.serve(rmcp::transport::io::stdio()).await?;
    server.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, RunMetadata, TestResult, TestRun};
    use tempfile::TempDir;

    fn parse_result(result: &CallToolResult) -> serde_json::Value {
        let text_content = result.content[0].as_text().expect("Expected text content");
        serde_json::from_str(&text_content.text).unwrap()
    }

    fn setup_repo_with_run(temp: &TempDir) -> Box<dyn Repository> {
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::success("test_pass").with_duration(std::time::Duration::from_secs(2)),
        );
        run.add_result(TestResult::failure("test_fail", "assertion failed"));
        repo.insert_test_run(run).unwrap();

        repo.set_run_metadata(
            &RunId::new("0"),
            RunMetadata {
                git_commit: Some("abc123".to_string()),
                git_dirty: Some(false),
                command: Some("cargo test".to_string()),
                concurrency: Some(4),
                duration_secs: Some(5.0),
                exit_code: Some(1),
            },
        )
        .unwrap();

        repo
    }

    #[tokio::test]
    async fn test_inq_stats() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service.inq_stats().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["run_count"], 1);
        assert_eq!(json["latest_run"]["total_tests"], 2);
        assert_eq!(json["latest_run"]["passed"], 1);
        assert_eq!(json["latest_run"]["failed"], 1);
    }

    #[tokio::test]
    async fn test_inq_failing() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_failing(Parameters(ListParam::default()))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 1);
        assert_eq!(json["total"], 1);
        assert_eq!(json["tests"][0], "test_fail");
    }

    #[tokio::test]
    async fn test_inq_last() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_last(Parameters(RunIdParam { run_id: None }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["id"], "0");
        assert_eq!(json["total_tests"], 2);
        assert_eq!(json["passed"], 1);
        assert_eq!(json["failed"], 1);
        assert_eq!(json["failing_tests"][0], "test_fail");
    }

    #[tokio::test]
    async fn test_inq_slowest() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_slowest(Parameters(SlowestParam { count: Some(5) }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["tests"][0]["test_id"], "test_pass");
        assert_eq!(json["tests"][0]["duration_secs"], 2.0);
    }

    #[tokio::test]
    async fn test_inq_info() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_info(Parameters(RunIdParam { run_id: None }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["id"], "0");
        assert_eq!(json["git_commit"], "abc123");
        assert_eq!(json["git_dirty"], false);
        assert_eq!(json["command"], "cargo test");
        assert_eq!(json["concurrency"], 4);
        assert_eq!(json["wall_duration_secs"], 5.0);
        assert_eq!(json["exit_code"], 1);
        assert_eq!(json["total_tests"], 2);
        assert_eq!(json["passed"], 1);
        assert_eq!(json["failed"], 1);
    }

    #[tokio::test]
    async fn test_inq_info_specific_run() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_info(Parameters(RunIdParam {
                run_id: Some("0".to_string()),
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["id"], "0");
        assert_eq!(json["git_commit"], "abc123");
    }

    #[tokio::test]
    async fn test_inq_running_no_runs() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service.inq_running().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 0);
        assert_eq!(json["runs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_inq_running_progress_estimate() {
        use crate::subunit_stream;
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Seed historical test times for 4 tests, 2s each.
        let mut times = std::collections::HashMap::new();
        for i in 0..4 {
            times.insert(
                crate::repository::TestId::new(format!("test_{i}")),
                std::time::Duration::from_secs(2),
            );
        }
        repo.update_test_times(&times).unwrap();

        // Start an in-progress run with 1 of 4 tests observed (test_0 passed).
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        let mut run = TestRun::new(run_id.clone());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test_0"));
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        use std::io::Write;
        writer.flush().unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_running().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 1);
        let entry = &json["runs"][0];
        assert_eq!(entry["total_expected"], 4);
        // 1/4 observed → 0.25.
        let pct = entry["percent_complete"].as_f64().unwrap();
        assert!((pct - 0.25).abs() < 1e-6, "got {}", pct);
        // 3 tests not yet observed × 2s each = 6s remaining.
        let eta = entry["estimated_remaining_secs"].as_f64().unwrap();
        assert!((eta - 6.0).abs() < 1e-6, "got {}", eta);

        drop(writer);
    }

    #[tokio::test]
    async fn test_inq_running_omits_estimate_without_history() {
        use crate::subunit_stream;
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        // No historical times — fields should be omitted.
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        let run = TestRun::new(run_id.clone());
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        use std::io::Write;
        writer.flush().unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_running().await.unwrap();
        let json = parse_result(&result);

        let entry = &json["runs"][0];
        assert!(
            entry.get("total_expected").is_none(),
            "total_expected should be omitted: {:?}",
            entry
        );
        assert!(entry.get("percent_complete").is_none());
        assert!(entry.get("estimated_remaining_secs").is_none());

        drop(writer);
    }

    #[tokio::test]
    async fn test_inq_init() {
        let temp = TempDir::new().unwrap();
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service.inq_init().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["status"], "initialized");
        assert!(temp.path().join(".inquest").exists());
    }

    #[tokio::test]
    async fn test_inq_init_already_exists() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_init().await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inq_auto_cargo_project() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        )
        .unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_auto().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["status"], "created");
        assert!(temp.path().join("inquest.toml").exists());
    }

    #[tokio::test]
    async fn test_inq_auto_no_project() {
        let temp = TempDir::new().unwrap();
        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_auto().await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inq_log() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: Some(vec!["test_fail".to_string()]),
                status_filter: None,
                limit: None,
                include_details: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 1);
        assert_eq!(json["results"][0]["test_id"], "test_fail");
        assert_eq!(json["results"][0]["status"], "failure");
    }

    #[tokio::test]
    async fn test_inq_log_status_filter() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        // Filter for failures only
        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: Some(vec!["failure".to_string()]),
                limit: None,
                include_details: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        assert_eq!(json["count"], 1);
        assert_eq!(json["results"][0]["test_id"], "test_fail");

        // Filter for successes only
        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: Some(vec!["success".to_string()]),
                limit: None,
                include_details: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        assert_eq!(json["count"], 1);
        assert_eq!(json["results"][0]["test_id"], "test_pass");

        // Use "failing" group alias
        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: Some(vec!["failing".to_string()]),
                limit: None,
                include_details: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        assert_eq!(json["count"], 1);
        assert_eq!(json["results"][0]["status"], "failure");
    }

    #[tokio::test]
    async fn test_inq_log_invalid_status_filter() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: Some(vec!["bogus".to_string()]),
                limit: None,
                include_details: None,
                max_detail_lines: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_head_tail_truncate_short_input() {
        // Short input: returned unchanged (apart from per-line trim, but no line is long).
        assert_eq!(head_tail_truncate("one\ntwo\nthree", 10), "one\ntwo\nthree");
    }

    #[test]
    fn test_head_tail_truncate_elides_middle() {
        let input: String = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = head_tail_truncate(&input, 6);
        // With max_lines=6 → head 4, tail 2, 14 omitted.
        assert!(out.contains("line1"));
        assert!(out.contains("line4"));
        assert!(!out.contains("line5"));
        assert!(!out.contains("line18"));
        assert!(out.contains("line19"));
        assert!(out.contains("line20"));
        assert!(out.contains("14 lines omitted"));
    }

    #[test]
    fn test_head_tail_truncate_zero_disables() {
        let input: String = (1..=200)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(head_tail_truncate(&input, 0), input);
    }

    #[test]
    fn test_head_tail_truncate_caps_long_line() {
        // A single line longer than MAX_LINE_CHARS (500) gets trimmed even when under max_lines.
        let long_line = "x".repeat(1000);
        let out = head_tail_truncate(&long_line, 10);
        assert!(out.contains("chars omitted"));
        assert!(out.chars().count() < 700);
    }

    #[tokio::test]
    async fn test_inq_log_truncates_details() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let long_traceback: String = (1..=200)
            .map(|i| format!("  at frame {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::failure("test_fail", "boom").with_details(long_traceback.clone()),
        );
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: None,
                limit: None,
                include_details: Some(true),
                max_detail_lines: None, // default 60
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        let details = json["results"][0]["details"].as_str().unwrap();
        // 200 lines truncated to 60 → 140 elided.
        assert!(details.contains("lines omitted"));
        // Confirm output is substantially shorter than input.
        assert!(details.lines().count() < 200);
        assert!(details.lines().count() <= 62); // 60 kept lines + elision marker lines
    }

    #[tokio::test]
    async fn test_inq_log_truncation_opt_out() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let long_traceback: String = (1..=200)
            .map(|i| format!("  at frame {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::failure("test_fail", "boom").with_details(long_traceback.clone()),
        );
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_log(Parameters(LogParam {
                run_id: None,
                test_patterns: None,
                status_filter: None,
                limit: None,
                include_details: Some(true),
                max_detail_lines: Some(0), // opt out
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        let details = json["results"][0]["details"].as_str().unwrap();
        assert!(!details.contains("lines omitted"));
        assert_eq!(details.lines().count(), 200);
    }

    #[test]
    fn test_first_nonempty_line_skips_blank_prefix() {
        assert_eq!(
            first_nonempty_line("\n\n  first real line\nsecond\n"),
            Some("first real line".to_string())
        );
    }

    #[test]
    fn test_first_nonempty_line_truncates_long() {
        let long = "x".repeat(500);
        let out = first_nonempty_line(&long).unwrap();
        assert!(out.chars().count() <= SUMMARY_FIRST_LINE_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn test_first_nonempty_line_all_blank() {
        assert_eq!(first_nonempty_line("\n\n   \n"), None);
    }

    #[tokio::test]
    async fn test_inq_failure_summary_lists_failing_only() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test_pass"));
        run.add_result(
            TestResult::failure("test_boom", "boom")
                .with_details("AssertionError: expected 1 got 2\n  at frame 1\n  at frame 2"),
        );
        run.add_result(
            TestResult::failure("test_other", "nope")
                .with_details("TypeError: cannot add int to str"),
        );
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_failure_summary(Parameters(FailureSummaryParam::default()))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["total_failing"], 2);
        assert_eq!(json["count"], 2);
        assert!(json.get("truncated").is_none() || json["truncated"].is_null());
        let failures = json["failures"].as_array().unwrap();
        // Sorted by test_id.
        assert_eq!(failures[0]["test_id"], "test_boom");
        assert_eq!(failures[0]["status"], "failure");
        assert_eq!(
            failures[0]["message_first_line"],
            "AssertionError: expected 1 got 2"
        );
        assert_eq!(failures[1]["test_id"], "test_other");
        assert_eq!(
            failures[1]["message_first_line"],
            "TypeError: cannot add int to str"
        );
    }

    #[tokio::test]
    async fn test_inq_failure_summary_truncates() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        for i in 0..100 {
            run.add_result(
                TestResult::failure(format!("test_{i:03}"), "boom").with_details("E: boom"),
            );
        }
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_failure_summary(Parameters(FailureSummaryParam {
                run_id: None,
                limit: None, // default 25
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["total_failing"], 100);
        assert_eq!(json["count"], FAILURE_SUMMARY_LIMIT);
        assert_eq!(
            json["truncated"].as_u64().unwrap() as usize,
            100 - FAILURE_SUMMARY_LIMIT
        );
    }

    #[tokio::test]
    async fn test_inq_test_hit() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::failure("pkg::test_x", "boom")
                .with_duration(std::time::Duration::from_millis(500))
                .with_details("AssertionError: line 1\nline 2\nline 3"),
        );
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test(Parameters(TestLookupParam {
                test_id: "pkg::test_x".to_string(),
                run_id: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["test_id"], "pkg::test_x");
        assert_eq!(json["status"], "failure");
        assert!(json["duration_secs"].as_f64().unwrap() > 0.4);
        assert!(json["details"].as_str().unwrap().contains("AssertionError"));
    }

    #[tokio::test]
    async fn test_inq_test_miss() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test(Parameters(TestLookupParam {
                test_id: "does_not_exist".to_string(),
                run_id: None,
                max_detail_lines: None,
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inq_test_batch_happy_path() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::failure("a", "boom").with_details("AssertionError: a"));
        run.add_result(TestResult::failure("b", "boom").with_details("AssertionError: b"));
        run.add_result(TestResult::success("c"));
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_batch(Parameters(TestBatchLookupParam {
                test_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                run_id: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 3);
        let results = json["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["test_id"], "a");
        assert_eq!(results[1]["test_id"], "b");
        assert_eq!(results[2]["test_id"], "c");
        assert!(json.get("not_found").is_none() || json["not_found"].is_null());
    }

    #[tokio::test]
    async fn test_inq_test_batch_partial_miss_and_dedup() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::failure("a", "boom"));
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_batch(Parameters(TestBatchLookupParam {
                // "a" is present, "ghost" is not, "a" again is a duplicate.
                test_ids: vec!["a".to_string(), "ghost".to_string(), "a".to_string()],
                run_id: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 1);
        assert_eq!(json["results"][0]["test_id"], "a");
        assert_eq!(json["not_found"].as_array().unwrap().len(), 1);
        assert_eq!(json["not_found"][0], "ghost");
    }

    #[tokio::test]
    async fn test_inq_test_batch_empty_input() {
        let temp = TempDir::new().unwrap();
        let _repo = setup_repo_with_run(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_batch(Parameters(TestBatchLookupParam {
                test_ids: vec![],
                run_id: None,
                max_detail_lines: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 0);
        assert_eq!(json["results"].as_array().unwrap().len(), 0);
        assert!(json.get("not_found").is_none() || json["not_found"].is_null());
    }

    fn setup_runnable_project(temp: &TempDir) {
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        // Create an inquest.toml with a test command that produces valid subunit v2 output.
        // An empty stream is valid subunit (0 tests).
        let config = "test_command = \"echo test\"\n";
        std::fs::write(temp.path().join("inquest.toml"), config).unwrap();
    }

    #[tokio::test]
    async fn test_inq_run_foreground() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: None,
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);

        assert!(json.get("exit_code").is_some());
        assert!(json.get("id").is_some());
    }

    #[tokio::test]
    async fn test_inq_run_rejects_invalid_order() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let err = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: None,
                    order: Some("nonsense-order".to_string()),
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.message.contains("unknown test order"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_inq_run_accepts_alphabetical_order() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: None,
                    order: Some("alphabetical".to_string()),
                },
                None,
            )
            .await
            .unwrap();

        let json = parse_result(&result);
        assert!(json.get("exit_code").is_some());
        assert!(json.get("id").is_some());
    }

    #[tokio::test]
    async fn test_inq_run_captures_stderr_on_failure() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        // A test command that succeeds at listing (empty output = 0 tests) but
        // fails during execution by writing to stderr and exiting non-zero.
        // This lands in the "exit_code != 0, stderr non-empty" branch that
        // error_output was designed to surface.
        let config = "test_command = \"sh -c 'if [ -n \\\"$LISTOPT\\\" ]; then \
             exit 0; else echo boom-from-stderr 1>&2; exit 3; fi' -- $LISTOPT\"\n\
             test_list_option = \"--list\"\n";
        std::fs::write(temp.path().join("inquest.toml"), config).unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: None,
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_ne!(json["exit_code"].as_i64().unwrap(), 0);
        let error_output = json["error_output"]
            .as_str()
            .expect("error_output should be populated on non-zero exit with stderr");
        assert!(
            error_output.contains("boom-from-stderr"),
            "expected error_output to contain stderr, got: {:?}",
            error_output
        );
    }

    #[tokio::test]
    async fn test_inq_run_background() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: Some(true),
                    background_after: None,
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["status"], "started");
        assert!(json.get("run_id").is_some());

        // Wait for background task to complete
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // The run should no longer be in progress
        let running = service.inq_running().await.unwrap();
        let running_json = parse_result(&running);
        assert_eq!(running_json["count"], 0);
    }

    #[tokio::test]
    async fn test_inq_run_background_after_timeout() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        // Test command that produces empty (valid) subunit output but takes
        // long enough that a 1-second background_after timer will fire first.
        let config = "test_command = \"sh -c 'sleep 3'\"\n";
        std::fs::write(temp.path().join("inquest.toml"), config).unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: Some(1),
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["status"], "running");
        let run_id = json["run_id"]
            .as_str()
            .expect("run_id should be populated when timing out")
            .to_string();
        assert!(!run_id.is_empty());

        // Follow-up: inq_wait should complete once the detached run finishes.
        let wait_result = service
            .inq_wait_impl(
                WaitParam {
                    run_id: Some(run_id),
                    status_filter: None,
                    timeout_secs: Some(15),
                },
                None,
            )
            .await
            .unwrap();
        let wait_json = parse_result(&wait_result);
        assert_eq!(wait_json["status"], "completed");
    }

    #[tokio::test]
    async fn test_inq_run_background_after_unused_when_fast() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        // Run is fast enough to finish before background_after fires; response
        // should be the normal RunResponse (with exit_code), not the handle.
        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: None,
                    background_after: Some(30),
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);

        assert!(json.get("exit_code").is_some());
        assert!(json.get("status").is_none());
    }

    #[tokio::test]
    async fn test_inq_cancel_nonexistent_run() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_cancel(Parameters(CancelParam {
                run_id: "nonexistent".to_string(),
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inq_cancel_background_run() {
        use crate::test_executor::CancellationToken;

        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        // Manually insert a cancellation token to simulate a background run
        let token = CancellationToken::new();
        service
            .cancel_tokens
            .lock()
            .unwrap()
            .insert(crate::repository::RunId::new("42"), token.clone());

        // Cancel it
        let cancel_result = service
            .inq_cancel(Parameters(CancelParam {
                run_id: "42".to_string(),
            }))
            .await
            .unwrap();
        let cancel_json = parse_result(&cancel_result);
        assert_eq!(cancel_json["status"], "cancelling");
        assert_eq!(cancel_json["run_id"], "42");

        // Verify the token was actually cancelled
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_inq_wait_no_runs() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_wait_impl(
                WaitParam {
                    run_id: None,
                    status_filter: None,
                    timeout_secs: Some(5),
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);
        assert_eq!(json["status"], "completed");
    }

    #[tokio::test]
    async fn test_inq_wait_background_run() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        // Start a background run
        let result = service
            .inq_run_impl(
                RunParam {
                    failing_only: None,
                    concurrency: None,
                    test_filters: None,
                    background: Some(true),
                    background_after: None,
                    order: None,
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&result);
        let run_id = json["run_id"].as_str().unwrap().to_string();

        // Wait for it to complete
        let wait_result = service
            .inq_wait_impl(
                WaitParam {
                    run_id: Some(run_id),
                    status_filter: None,
                    timeout_secs: Some(10),
                },
                None,
            )
            .await
            .unwrap();
        let wait_json = parse_result(&wait_result);
        assert_eq!(wait_json["status"], "completed");
    }

    #[tokio::test]
    async fn test_inq_wait_truncates_matching_tests() {
        use crate::subunit_stream;
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Seed a run with 150 failing tests and keep the writer alive so the lock
        // file stays — this makes the run appear "in progress" to inq_wait.
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        let mut run = TestRun::new(run_id.clone());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        for i in 0..150 {
            run.add_result(TestResult::failure(format!("test_{i}"), "boom"));
        }
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        use std::io::Write;
        writer.flush().unwrap();
        // Hold the writer (and therefore the lock file) for the duration of the test.
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let wait_result = service
            .inq_wait_impl(
                WaitParam {
                    run_id: Some(run_id.as_str().to_string()),
                    status_filter: Some(vec!["failing".to_string()]),
                    timeout_secs: Some(5),
                },
                None,
            )
            .await
            .unwrap();
        let json = parse_result(&wait_result);
        assert_eq!(json["status"], "early_return");
        let matching = json["matching_tests"].as_array().unwrap();
        assert_eq!(matching.len(), MATCHING_LIST_LIMIT);
        assert_eq!(
            json["matching_truncated"].as_u64().unwrap() as usize,
            150 - MATCHING_LIST_LIMIT
        );

        drop(writer); // release lock
    }

    #[tokio::test]
    async fn test_inq_last_truncates_failing_tests() {
        let factory = InquestRepositoryFactory;
        let temp = TempDir::new().unwrap();
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        for i in 0..150 {
            run.add_result(TestResult::failure(format!("test_{i:03}"), "boom"));
        }
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_last(Parameters(RunIdParam { run_id: None }))
            .await
            .unwrap();
        let json = parse_result(&result);
        let failing = json["failing_tests"].as_array().unwrap();
        assert_eq!(failing.len(), FAILING_LIST_LIMIT);
        assert_eq!(
            json["failing_truncated"].as_u64().unwrap() as usize,
            150 - FAILING_LIST_LIMIT
        );
    }

    #[tokio::test]
    async fn test_inq_stats_empty_repo() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_stats().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["run_count"], 0);
        assert!(json.get("latest_run").is_none());
    }

    /// Helper: seed a repo with `n` sequential runs, each containing the tests
    /// produced by `per_run_tests(run_index)`. Returns the opened repo closed
    /// for further writes.
    fn seed_repo_with_runs<F>(temp: &TempDir, n: usize, per_run_tests: F)
    where
        F: Fn(usize) -> Vec<TestResult>,
    {
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();
        for i in 0..n {
            let run_id = RunId::new(i.to_string());
            let mut run = TestRun::new(run_id.clone());
            // Distinct timestamps so newest-first ordering is observable.
            run.timestamp =
                chrono::DateTime::from_timestamp(1_000_000_000 + i as i64 * 3600, 0).unwrap();
            for r in per_run_tests(i) {
                run.add_result(r);
            }
            repo.insert_test_run(run).unwrap();
            repo.set_run_metadata(
                &run_id,
                RunMetadata {
                    exit_code: Some(if i % 2 == 0 { 1 } else { 0 }),
                    ..RunMetadata::default()
                },
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn test_inq_list_runs_newest_first() {
        let temp = TempDir::new().unwrap();
        seed_repo_with_runs(&temp, 3, |_| vec![TestResult::success("a")]);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_list_runs(Parameters(ListRunsParam::default()))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["total"], 3);
        assert_eq!(json["count"], 3);
        assert!(json.get("truncated").is_none() || json["truncated"].is_null());
        let runs = json["runs"].as_array().unwrap();
        // Newest first: ids 2, 1, 0.
        assert_eq!(runs[0]["id"], "2");
        assert_eq!(runs[1]["id"], "1");
        assert_eq!(runs[2]["id"], "0");
        // exit_code from metadata comes through.
        assert_eq!(runs[0]["exit_code"], 1); // index 2 → even → exit 1
        assert_eq!(runs[1]["exit_code"], 0);
    }

    #[tokio::test]
    async fn test_inq_list_runs_pagination() {
        let temp = TempDir::new().unwrap();
        seed_repo_with_runs(&temp, 5, |_| vec![TestResult::success("a")]);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_list_runs(Parameters(ListRunsParam {
                limit: Some(2),
                offset: Some(1),
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["total"], 5);
        assert_eq!(json["count"], 2);
        // Newest is id 4; offset 1 skips it; limit 2 takes ids 3 and 2.
        assert_eq!(json["runs"][0]["id"], "3");
        assert_eq!(json["runs"][1]["id"], "2");
        // 5 total - 1 offset - 2 returned = 2 remaining.
        assert_eq!(json["truncated"], 2);
    }

    #[tokio::test]
    async fn test_inq_test_history_tracks_status_changes() {
        let temp = TempDir::new().unwrap();
        // Run 0: passes. Run 1: fails. Run 2: passes. Run 3: test absent.
        // Note: the subunit roundtrip only preserves the `details` attachment,
        // so attach the message as details to make it observable in the response.
        seed_repo_with_runs(&temp, 4, |i| match i {
            0 => vec![TestResult::success("pkg::flaky")],
            1 => vec![TestResult::failure("pkg::flaky", "sometimes fails")
                .with_details("sometimes fails")],
            2 => vec![TestResult::success("pkg::flaky")],
            3 => vec![TestResult::success("other")],
            _ => vec![],
        });

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_history(Parameters(TestHistoryParam {
                test_id: "pkg::flaky".to_string(),
                limit: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        // Run 3 is skipped (test absent); remaining history has 3 entries, newest first.
        assert_eq!(json["count"], 3);
        let history = json["history"].as_array().unwrap();
        assert_eq!(history[0]["run_id"], "2");
        assert_eq!(history[0]["status"], "success");
        assert_eq!(history[1]["run_id"], "1");
        assert_eq!(history[1]["status"], "failure");
        assert_eq!(history[1]["message_first_line"], "sometimes fails");
        assert_eq!(history[2]["run_id"], "0");
        assert_eq!(history[2]["status"], "success");
    }

    #[tokio::test]
    async fn test_inq_test_history_respects_limit() {
        let temp = TempDir::new().unwrap();
        seed_repo_with_runs(&temp, 10, |_| vec![TestResult::success("t")]);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_history(Parameters(TestHistoryParam {
                test_id: "t".to_string(),
                limit: Some(3),
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 3);
        // Newest three: 9, 8, 7.
        assert_eq!(json["history"][0]["run_id"], "9");
        assert_eq!(json["history"][2]["run_id"], "7");
    }

    #[tokio::test]
    async fn test_inq_test_history_unknown_test() {
        let temp = TempDir::new().unwrap();
        seed_repo_with_runs(&temp, 3, |_| vec![TestResult::success("real_test")]);

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service
            .inq_test_history(Parameters(TestHistoryParam {
                test_id: "never_seen".to_string(),
                limit: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 0);
        assert_eq!(json["history"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_inq_config_reports_loaded_config() {
        let temp = TempDir::new().unwrap();
        // Minimal config with listing and targeted-run support.
        let toml = r#"
test_command = "pytest $LISTOPT $IDLIST"
test_list_option = "--collect-only -q"
test_id_option = "-k"
test_timeout = "5m"
"#;
        std::fs::write(temp.path().join("inquest.toml"), toml).unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_config().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["test_command"], "pytest $LISTOPT $IDLIST");
        assert_eq!(json["test_list_option"], "--collect-only -q");
        assert_eq!(json["test_id_option"], "-k");
        assert_eq!(json["test_timeout"], "5m");
        assert_eq!(json["supports_listing"], true);
        assert_eq!(json["supports_targeted_runs"], true);
        assert!(json["config_path"]
            .as_str()
            .unwrap()
            .ends_with("inquest.toml"));
        // Fields not set in the config should be omitted via skip_serializing_if.
        assert!(json.get("max_duration").is_none() || json["max_duration"].is_null());
        assert!(json.get("group_regex").is_none() || json["group_regex"].is_null());
    }

    #[tokio::test]
    async fn test_inq_config_minimal_config_flags_false() {
        let temp = TempDir::new().unwrap();
        // No list/id options — advertise capabilities accordingly.
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"test_command = "cargo test""#,
        )
        .unwrap();

        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_config().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["test_command"], "cargo test");
        assert_eq!(json["supports_listing"], false);
        assert_eq!(json["supports_targeted_runs"], false);
    }

    #[tokio::test]
    async fn test_inq_config_missing_errors_out() {
        let temp = TempDir::new().unwrap();
        // Deliberately no config file written.
        let service = InquestMcpService::new(temp.path().to_path_buf());
        let result = service.inq_config().await;
        assert!(result.is_err());
    }

    /// When no reporter is attached (client didn't opt in to progress), the
    /// helper should still drive the blocking work to completion and return
    /// its result unchanged, without spawning a keepalive task.
    #[tokio::test]
    async fn test_run_blocking_with_progress_without_reporter() {
        let value = run_blocking_with_progress(None, "noop", || 42u32)
            .await
            .unwrap();
        assert_eq!(value, 42);
    }

    /// Panics inside the blocking closure must surface as `JoinError` rather
    /// than taking down the whole task.
    #[tokio::test]
    async fn test_run_blocking_with_progress_propagates_panic() {
        let result = run_blocking_with_progress(None, "boom", || panic!("boom")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_panic());
    }
}
