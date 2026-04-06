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
use crate::repository::{Repository, RepositoryFactory};
use crate::subunit_stream;
use crate::testcommand::TestCommand;

use std::path::PathBuf;
use std::time::Duration;

/// MCP server for inquest test repositories.
#[derive(Debug, Clone)]
pub struct InquestMcpService {
    directory: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl InquestMcpService {
    /// Create a new MCP service for the given directory.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            tool_router: Self::tool_router(),
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

/// Parameters for the log tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LogParam {
    /// Run ID to query (defaults to latest; supports negative indices like -1, -2)
    pub run_id: Option<String>,
    /// Test ID patterns to match (glob-style wildcards). If empty, shows all tests.
    pub test_patterns: Option<Vec<String>>,
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
}

fn to_mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn duration_secs(d: Duration) -> f64 {
    d.as_secs_f64()
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

        let mut matching: Vec<_> = test_run
            .results
            .values()
            .filter(|r| {
                if patterns.is_empty() {
                    true
                } else {
                    patterns.iter().any(|p| p.matches(r.test_id.as_str()))
                }
            })
            .collect();
        matching.sort_by_key(|r| r.test_id.as_str().to_string());

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
        let mut ui = NullUI;

        let failing_only = params.0.failing_only.unwrap_or(false);
        let partial = failing_only;
        let test_filters = params.0.test_filters.filter(|f| !f.is_empty());

        let cmd = crate::commands::RunCommand {
            base_path: Some(base.to_string_lossy().to_string()),
            partial,
            failing_only,
            force_init: true,
            concurrency: params.0.concurrency,
            test_filters,
            ..Default::default()
        };

        use crate::commands::Command;
        let exit_code = cmd.execute(&mut ui).map_err(|e| {
            ErrorData::internal_error(format!("Test execution failed: {}", e), None)
        })?;

        // After running, get the latest results
        let repo = self.open_repo()?;
        let test_run = repo.get_latest_run().map_err(to_mcp_err)?;

        let failing_tests: Vec<&str> = test_run
            .get_failing_tests()
            .iter()
            .map(|id| id.as_str())
            .collect();

        let duration = test_run.total_duration().map(duration_secs);
        let interruption = test_run.interruption.as_ref().map(|i| i.to_string());
        let result = serde_json::json!({
            "exit_code": exit_code,
            "id": test_run.id,
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
        struct CollectUI {
            output: Vec<String>,
            errors: Vec<String>,
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

        let mut ui = CollectUI {
            output: Vec::new(),
            errors: Vec::new(),
        };

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
        struct CollectUI {
            output: Vec<String>,
        }
        impl crate::ui::UI for CollectUI {
            fn output(&mut self, msg: &str) -> crate::error::Result<()> {
                self.output.push(msg.to_string());
                Ok(())
            }
            fn error(&mut self, msg: &str) -> crate::error::Result<()> {
                self.output.push(msg.to_string());
                Ok(())
            }
            fn warning(&mut self, msg: &str) -> crate::error::Result<()> {
                self.output.push(msg.to_string());
                Ok(())
            }
        }

        let mut ui = CollectUI { output: Vec::new() };

        let cmd =
            crate::commands::AnalyzeIsolationCommand::new(Some(self.dir_str()), params.0.test);
        use crate::commands::Command;
        let exit_code = cmd.execute(&mut ui).map_err(to_mcp_err)?;

        let result = serde_json::json!({
            "exit_code": exit_code,
            "output": ui.output,
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
