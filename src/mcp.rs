//! MCP (Model Context Protocol) server for inquest
//!
//! Provides structured JSON access to test repository data via the MCP protocol.
//! Start with `inq mcp` to run the server over stdio.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorData, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Serialize;

use crate::commands::utils::{open_or_init_repository, open_repository, resolve_run_id};
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, TestStatus};
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
    tool_router: ToolRouter<Self>,
    /// Cancellation tokens for background runs, keyed by run ID.
    cancel_tokens: Arc<Mutex<HashMap<crate::repository::RunId, CancellationToken>>>,
}

impl InquestMcpService {
    /// Create a new MCP service for the given directory.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            tool_router: Self::tool_router(),
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
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2)
    pub run_id: Option<String>,
}

/// Parameters for the slowest tests tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SlowestParam {
    /// Number of slowest tests to return (default 10)
    pub count: Option<usize>,
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
    /// Filter by test status. Valid values: "success", "failure", "error", "skip", "xfail",
    /// "uxsuccess". Also accepts "failing" (equivalent to failure+error+uxsuccess) and
    /// "passing" (equivalent to success+skip+xfail). If empty, shows all statuses.
    pub status_filter: Option<Vec<String>>,
    /// Max number of results to return (default 20). Use a larger value when you need
    /// the full list; the response reports `total` and `truncated` so you know if more exist.
    pub limit: Option<usize>,
    /// Include failure messages and full tracebacks in each result. Off by default to
    /// keep responses small — enable when investigating a specific failure.
    pub include_details: Option<bool>,
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
    pub background: Option<bool>,
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
    /// Optional status filter. If specified, returns early when any test result matches
    /// this status (e.g. "failing", "failure", "error"). Uses the same filter syntax as
    /// other tools: "failing" = failure+error+uxsuccess, "passing" = success+skip+xfail,
    /// or individual statuses like "failure", "error", "success", "skip", "xfail", "uxsuccess".
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
    interruption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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

#[derive(Serialize)]
struct RunningEntry {
    id: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    elapsed_secs: i64,
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
/// Accepts individual status names ("success", "failure", "error", "skip", "xfail", "uxsuccess")
/// and group aliases ("failing" = failure+error+uxsuccess, "passing" = success+skip+xfail).
fn parse_status_filters(filters: &[String]) -> Result<Vec<TestStatus>, ErrorData> {
    let mut statuses = Vec::new();
    for f in filters {
        match f.to_lowercase().as_str() {
            "failing" => {
                statuses.extend([
                    TestStatus::Failure,
                    TestStatus::Error,
                    TestStatus::UnexpectedSuccess,
                ]);
            }
            "passing" => {
                statuses.extend([
                    TestStatus::Success,
                    TestStatus::Skip,
                    TestStatus::ExpectedFailure,
                ]);
            }
            "success" => statuses.push(TestStatus::Success),
            "failure" => statuses.push(TestStatus::Failure),
            "error" => statuses.push(TestStatus::Error),
            "skip" => statuses.push(TestStatus::Skip),
            "xfail" => statuses.push(TestStatus::ExpectedFailure),
            "uxsuccess" => statuses.push(TestStatus::UnexpectedSuccess),
            other => {
                return Err(ErrorData::invalid_params(
                    format!(
                        "Unknown status filter: '{}'. Valid values: success, failure, error, skip, xfail, uxsuccess, failing, passing",
                        other
                    ),
                    None,
                ));
            }
        }
    }
    Ok(statuses)
}

#[tool_router]
impl InquestMcpService {
    /// Show repository statistics including run count and latest run summary.
    #[tool(description = "Show repository statistics including run count and latest run summary")]
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
        description = "List currently failing tests from the repository. Paginated — default limit is 100."
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
        description = "Show results from the last (or a specific) test run including pass/fail counts, duration, and failing test details"
    )]
    async fn inq_last(&self, params: Parameters<RunIdParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        ok_json(&LastResponse {
            id: test_run.id.as_str().to_string(),
            timestamp: test_run.timestamp.to_rfc3339(),
            total_tests: test_run.total_tests(),
            passed: test_run.count_successes(),
            failed: test_run.count_failures(),
            duration_secs: test_run.total_duration().map(duration_secs),
            failing_tests: test_run
                .get_failing_tests()
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            interruption: test_run.interruption.as_ref().map(|i| i.to_string()),
        })
    }

    /// Compare two test runs and show what changed.
    #[tool(
        description = "Compare two test runs and show new failures, new passes, added/removed tests, and status changes"
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
    #[tool(description = "Show the slowest tests from the last run with timing information")]
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

    /// Show test details and tracebacks.
    #[tool(
        description = "Show test details (status, duration) for matching tests. Paginated — \
                        default limit is 20. Failure messages and tracebacks are omitted \
                        unless include_details=true."
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
                    r.message.clone()
                } else {
                    None
                },
                details: if include_details {
                    r.details.clone()
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

    /// Execute tests and return results.
    #[tool(
        description = "Execute tests and return structured results. Requires a configuration file (inquest.toml or .testr.conf) in the project directory."
    )]
    async fn inq_run(&self, params: Parameters<RunParam>) -> Result<CallToolResult, ErrorData> {
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

        let failing_only = params.0.failing_only.unwrap_or(false);
        let partial = failing_only;
        let test_filters = params.0.test_filters.filter(|f| !f.is_empty());
        let background = params.0.background.unwrap_or(false);

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
                        interruption: None,
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

            let concurrency = params.0.concurrency.unwrap_or(1);

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

        // Foreground execution
        let mut ui = NullUI;

        let cmd = crate::commands::RunCommand {
            base_path: Some(base.to_string_lossy().to_string()),
            partial,
            failing_only,
            force_init: true,
            concurrency: params.0.concurrency,
            test_filters,
            ..Default::default()
        };

        let cli_output = cmd.execute_returning_run_id(&mut ui).map_err(|e| {
            ErrorData::internal_error(format!("Test execution failed: {}", e), None)
        })?;

        if let Some(ref run_id) = cli_output.run_id {
            let repo = self.open_repo()?;
            let test_run = repo.get_test_run(run_id).map_err(to_mcp_err)?;

            ok_json(&RunResponse {
                exit_code: cli_output.exit_code,
                id: Some(run_id.as_str().to_string()),
                total_tests: Some(test_run.total_tests()),
                passed: Some(test_run.count_successes()),
                failed: Some(test_run.count_failures()),
                duration_secs: test_run.total_duration().map(duration_secs),
                failing_tests: test_run
                    .get_failing_tests()
                    .iter()
                    .map(|id| id.as_str().to_string())
                    .collect(),
                interruption: test_run.interruption.as_ref().map(|i| i.to_string()),
                message: None,
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
                interruption: None,
                message: Some("No tests were executed".to_string()),
            })
        }
    }

    /// List available tests.
    #[tool(
        description = "List all available tests discovered by the test command. \
                        Paginated — default limit is 100."
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
        description = "Show detailed information about a test run including git commit, command, concurrency, duration, and exit code"
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

    /// Show currently in-progress test runs.
    #[tool(description = "Show currently in-progress test runs with their status and progress")]
    async fn inq_running(&self) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_ids = repo.get_running_run_ids().map_err(to_mcp_err)?;

        let now = chrono::Utc::now();
        let runs: Vec<RunningEntry> = run_ids
            .iter()
            .filter_map(|run_id| {
                let test_run = repo.get_test_run(run_id).ok()?;
                Some(RunningEntry {
                    id: run_id.as_str().to_string(),
                    total_tests: test_run.total_tests(),
                    passed: test_run.count_successes(),
                    failed: test_run.count_failures(),
                    elapsed_secs: (now - test_run.timestamp).num_seconds(),
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
                        (e.g. \"failing\"). Much more efficient than polling inq_running in a loop."
    )]
    async fn inq_wait(&self, params: Parameters<WaitParam>) -> Result<CallToolResult, ErrorData> {
        let timeout = Duration::from_secs(params.0.timeout_secs.unwrap_or(600));
        let target_run_id = params
            .0
            .run_id
            .as_ref()
            .map(|id| crate::repository::RunId::new(id));
        let status_filter = if let Some(ref filters) = params.0.status_filter {
            Some(parse_status_filters(filters)?)
        } else {
            None
        };

        let poll_interval = Duration::from_secs(2);
        let start = std::time::Instant::now();

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
                let ids_to_check = if let Some(ref target) = target_run_id {
                    vec![target.clone()]
                } else {
                    running_ids.clone()
                };
                for run_id in &ids_to_check {
                    if let Ok(test_run) = repo.get_test_run(run_id) {
                        let matching: Vec<WaitMatchingTest> = test_run
                            .results
                            .iter()
                            .filter(|(_, r)| statuses.contains(&r.status))
                            .map(|(id, r)| WaitMatchingTest {
                                test_id: id.as_str().to_string(),
                                status: format!("{:?}", r.status),
                            })
                            .collect();
                        if !matching.is_empty() {
                            return ok_json(&WaitResponse::EarlyReturn {
                                status: "early_return",
                                reason: "Tests matching status filter found while run is still in progress",
                                run_id: run_id.as_str().to_string(),
                                total_tests: test_run.total_tests(),
                                passed: test_run.count_successes(),
                                failed: test_run.count_failures(),
                                matching_tests: matching,
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

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Cancel a running background test execution.
    #[tool(
        description = "Cancel a background test run. Use inq_running to find the run ID of in-progress runs."
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
    #[tool(description = "Initialize a new .inquest test repository in the project directory")]
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
        description = "Auto-detect project type (Cargo, pytest, unittest) and generate an inquest.toml configuration file"
    )]
    async fn inq_auto(&self) -> Result<CallToolResult, ErrorData> {
        let mut ui = CollectUI::new();

        let cmd = crate::commands::AutoCommand::new(Some(self.dir_str()));
        use crate::commands::Command;
        let exit_code = cmd.execute(&mut ui).map_err(to_mcp_err)?;

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
        description = "Analyze test isolation issues by bisecting the test suite to find which tests cause a target test to fail when run together but pass in isolation"
    )]
    async fn inq_analyze_isolation(
        &self,
        params: Parameters<AnalyzeIsolationParam>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut ui = CollectUI::new();

        let cmd =
            crate::commands::AnalyzeIsolationCommand::new(Some(self.dir_str()), params.0.test);
        use crate::commands::Command;
        let exit_code = cmd.execute(&mut ui).map_err(to_mcp_err)?;

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
             failing tests, statistics, and test execution."
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
            }))
            .await;
        assert!(result.is_err());
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
            .inq_run(Parameters(RunParam {
                failing_only: None,
                concurrency: None,
                test_filters: None,
                background: None,
            }))
            .await
            .unwrap();
        let json = parse_result(&result);

        assert!(json.get("exit_code").is_some());
        assert!(json.get("id").is_some());
    }

    #[tokio::test]
    async fn test_inq_run_background() {
        let temp = TempDir::new().unwrap();
        setup_runnable_project(&temp);
        let service = InquestMcpService::new(temp.path().to_path_buf());

        let result = service
            .inq_run(Parameters(RunParam {
                failing_only: None,
                concurrency: None,
                test_filters: None,
                background: Some(true),
            }))
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
            .inq_wait(Parameters(WaitParam {
                run_id: None,
                status_filter: None,
                timeout_secs: Some(5),
            }))
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
            .inq_run(Parameters(RunParam {
                failing_only: None,
                concurrency: None,
                test_filters: None,
                background: Some(true),
            }))
            .await
            .unwrap();
        let json = parse_result(&result);
        let run_id = json["run_id"].as_str().unwrap().to_string();

        // Wait for it to complete
        let wait_result = service
            .inq_wait(Parameters(WaitParam {
                run_id: Some(run_id),
                status_filter: None,
                timeout_secs: Some(10),
            }))
            .await
            .unwrap();
        let wait_json = parse_result(&wait_result);
        assert_eq!(wait_json["status"], "completed");
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
}
