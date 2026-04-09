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

use crate::commands::utils::{open_repository, resolve_run_id};
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
}

/// Parameters for the log tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LogParam {
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2)
    pub run_id: Option<String>,
    /// Test ID patterns to match (glob-style wildcards). If empty, shows all tests.
    pub test_patterns: Option<Vec<String>>,
    /// Filter by test status. Valid values: "success", "failure", "error", "skip", "xfail",
    /// "uxsuccess". Also accepts "failing" (equivalent to failure+error+uxsuccess) and
    /// "passing" (equivalent to success+skip+xfail). If empty, shows all statuses.
    pub status_filter: Option<Vec<String>>,
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
    /// Use inq_running to check progress. Use inq_last or inq_log to see
    /// partial results while running or final results when done.
    pub background: Option<bool>,
}

/// Parameters for the cancel tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CancelParam {
    /// Run ID of the background test run to cancel.
    pub run_id: String,
}

fn to_mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn duration_secs(d: Duration) -> f64 {
    d.as_secs_f64()
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

        let mut result = serde_json::json!({
            "run_count": run_count,
        });

        if !run_ids.is_empty() {
            let latest = repo.get_latest_run().map_err(to_mcp_err)?;
            let duration = latest.total_duration().map(duration_secs);
            result["latest_run"] = serde_json::json!({
                "id": latest.id,
                "total_tests": latest.total_tests(),
                "passed": latest.count_successes(),
                "failed": latest.count_failures(),
                "duration_secs": duration,
            });
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    /// List currently failing tests from the repository.
    #[tool(description = "List currently failing tests from the repository")]
    async fn inq_failing(&self) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let failing = repo.get_failing_tests().map_err(to_mcp_err)?;

        let test_ids: Vec<&str> = failing.iter().map(|id| id.as_str()).collect();
        let result = serde_json::json!({
            "count": test_ids.len(),
            "tests": test_ids,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    /// Show results from the last (or a specific) test run.
    #[tool(
        description = "Show results from the last (or a specific) test run including pass/fail counts, duration, and failing test details"
    )]
    async fn inq_last(&self, params: Parameters<RunIdParam>) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_id = resolve_run_id(&*repo, params.0.run_id.as_deref()).map_err(to_mcp_err)?;
        let test_run = repo.get_test_run(&run_id).map_err(to_mcp_err)?;

        let failing_tests: Vec<&str> = test_run
            .get_failing_tests()
            .iter()
            .map(|id| id.as_str())
            .collect();

        let duration = test_run.total_duration().map(duration_secs);
        let interruption = test_run.interruption.as_ref().map(|i| i.to_string());
        let result = serde_json::json!({
            "id": test_run.id,
            "timestamp": test_run.timestamp.to_rfc3339(),
            "total_tests": test_run.total_tests(),
            "passed": test_run.count_successes(),
            "failed": test_run.count_failures(),
            "duration_secs": duration,
            "failing_tests": failing_tests,
            "interruption": interruption,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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

        let mut new_failures = Vec::new();
        let mut new_passes = Vec::new();
        let mut status_changed = Vec::new();

        for id in ids1.intersection(&ids2) {
            let r1 = &run1.results[*id];
            let r2 = &run2.results[*id];
            if r1.status == r2.status {
                continue;
            }
            if r2.status.is_failure() && r1.status.is_success() {
                new_failures.push(r2.test_id.as_str());
            } else if r2.status.is_success() && r1.status.is_failure() {
                new_passes.push(r2.test_id.as_str());
            } else {
                status_changed.push(serde_json::json!({
                    "test_id": r2.test_id.as_str(),
                    "old_status": r1.status.to_string(),
                    "new_status": r2.status.to_string(),
                }));
            }
        }

        let added: Vec<&str> = ids2.difference(&ids1).map(|id| id.as_str()).collect();
        let removed: Vec<&str> = ids1.difference(&ids2).map(|id| id.as_str()).collect();

        let result = serde_json::json!({
            "run1": id1,
            "run2": id2,
            "new_failures": new_failures,
            "new_passes": new_passes,
            "status_changed": status_changed,
            "added_tests": added,
            "removed_tests": removed,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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

        let slowest: Vec<serde_json::Value> = tests_with_duration
            .iter()
            .take(count)
            .map(|&(id, dur)| {
                let secs = dur.as_secs_f64();
                serde_json::json!({
                    "test_id": id,
                    "duration_secs": secs,
                    "percentage": if total_secs > 0.0 { (secs / total_secs) * 100.0 } else { 0.0 },
                })
            })
            .collect();

        let result = serde_json::json!({
            "total_time_secs": total_secs,
            "tests": slowest,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    /// Show test details and tracebacks.
    #[tool(
        description = "Show test details including status, duration, and tracebacks for matching tests"
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

        let results: Vec<serde_json::Value> = matching
            .iter()
            .map(|r| {
                let dur = r.duration.map(duration_secs);
                serde_json::json!({
                    "test_id": r.test_id.as_str(),
                    "status": r.status.to_string(),
                    "duration_secs": dur,
                    "message": r.message,
                    "details": r.details,
                })
            })
            .collect();

        let result = serde_json::json!({
            "run_id": run_id,
            "count": results.len(),
            "results": results,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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
            let mut repo = self.open_repo()?;

            let base_path = base.to_string_lossy().to_string();
            let test_cmd = TestCommand::from_directory(base).map_err(|e| {
                ErrorData::internal_error(format!("Failed to load config: {}", e), None)
            })?;

            let historical_times = repo.get_test_times().map_err(to_mcp_err)?;

            // Resolve test IDs before spawning (needs repo)
            let mut test_ids = if failing_only {
                let failing = repo.get_failing_tests().map_err(to_mcp_err)?;
                if failing.is_empty() {
                    let result = serde_json::json!({
                        "exit_code": 0,
                        "message": "No failing tests to run",
                    });
                    return Ok(CallToolResult::success(vec![Content::text(
                        serde_json::to_string_pretty(&result).unwrap(),
                    )]));
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

            let result = serde_json::json!({
                "status": "started",
                "run_id": run_id_for_response,
            });

            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap(),
            )]));
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

            let failing_tests: Vec<&str> = test_run
                .get_failing_tests()
                .iter()
                .map(|id| id.as_str())
                .collect();

            let duration = test_run.total_duration().map(duration_secs);
            let interruption = test_run.interruption.as_ref().map(|i| i.to_string());
            let result = serde_json::json!({
                "exit_code": cli_output.exit_code,
                "id": run_id,
                "total_tests": test_run.total_tests(),
                "passed": test_run.count_successes(),
                "failed": test_run.count_failures(),
                "duration_secs": duration,
                "failing_tests": failing_tests,
                "interruption": interruption,
            });

            Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap(),
            )]))
        } else {
            let result = serde_json::json!({
                "exit_code": cli_output.exit_code,
                "message": "No tests were executed",
            });

            Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap(),
            )]))
        }
    }

    /// List available tests.
    #[tool(description = "List all available tests discovered by the test command")]
    async fn inq_list_tests(&self) -> Result<CallToolResult, ErrorData> {
        let test_cmd = TestCommand::from_directory(&self.directory).map_err(|e| {
            ErrorData::internal_error(format!("Failed to load config: {}", e), None)
        })?;

        let test_ids = test_cmd
            .list_tests()
            .map_err(|e| ErrorData::internal_error(format!("Failed to list tests: {}", e), None))?;

        let ids: Vec<&str> = test_ids.iter().map(|id| id.as_str()).collect();
        let result = serde_json::json!({
            "count": ids.len(),
            "tests": ids,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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

        let duration = test_run.total_duration().map(duration_secs);
        let result = serde_json::json!({
            "id": run_id,
            "timestamp": test_run.timestamp.to_rfc3339(),
            "git_commit": metadata.git_commit,
            "git_dirty": metadata.git_dirty,
            "command": metadata.command,
            "concurrency": metadata.concurrency,
            "wall_duration_secs": metadata.duration_secs,
            "exit_code": metadata.exit_code,
            "total_tests": test_run.total_tests(),
            "passed": test_run.count_successes(),
            "failed": test_run.count_failures(),
            "total_test_time_secs": duration,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
    }

    /// Show currently in-progress test runs.
    #[tool(description = "Show currently in-progress test runs with their status and progress")]
    async fn inq_running(&self) -> Result<CallToolResult, ErrorData> {
        let repo = self.open_repo()?;
        let run_ids = repo.get_running_run_ids().map_err(to_mcp_err)?;

        let now = chrono::Utc::now();
        let runs: Vec<serde_json::Value> = run_ids
            .iter()
            .filter_map(|run_id| {
                let test_run = repo.get_test_run(run_id).ok()?;
                let elapsed_secs = (now - test_run.timestamp).num_seconds();
                Some(serde_json::json!({
                    "id": run_id,
                    "total_tests": test_run.total_tests(),
                    "passed": test_run.count_successes(),
                    "failed": test_run.count_failures(),
                    "elapsed_secs": elapsed_secs,
                }))
            })
            .collect();

        let result = serde_json::json!({
            "count": runs.len(),
            "runs": runs,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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
            let result = serde_json::json!({
                "status": "cancelling",
                "run_id": run_id,
            });
            Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap(),
            )]))
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
            Ok(_) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "initialized",
                    "path": self.directory.join(".inquest").to_string_lossy(),
                }))
                .unwrap(),
            )])),
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

        let result = serde_json::json!({
            "status": "created",
            "message": ui.output.join("\n"),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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
        let result = serde_json::json!({
            "exit_code": exit_code,
            "output": all_output,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap(),
        )]))
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

        let result = service.inq_failing().await.unwrap();
        let json = parse_result(&result);

        assert_eq!(json["count"], 1);
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
