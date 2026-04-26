//! Run tests and load results into the repository

use crate::commands::utils::open_or_init_repository;
use crate::commands::Command;
use crate::config::TimeoutSetting;
use crate::error::Result;
use crate::ordering::{apply_order, OrderingContext, TestOrder};
use crate::test_executor::{self, TestExecutor, TestExecutorConfig};
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::path::Path;
use std::time::Duration;

/// Result of running tests via the CLI, including the run ID for callers that need it.
pub struct CliRunOutput {
    /// Process exit code (0 = success, non-zero = failure).
    pub exit_code: i32,
    /// The run ID assigned to this execution, if any.
    pub run_id: Option<crate::repository::RunId>,
}

impl CliRunOutput {
    /// Outcome for paths that finished without allocating a run (e.g. no
    /// failing tests to re-run, hit the iteration cap, auto-config failed).
    fn no_run(exit_code: i32) -> Self {
        Self {
            exit_code,
            run_id: None,
        }
    }
}

/// Command to run tests and load results into the repository.
///
/// All fields default to false/None/Disabled, so callers only need to set
/// the fields they care about.
#[derive(Default)]
pub struct RunCommand {
    /// Repository path (defaults to current directory)
    pub base_path: Option<String>,
    /// Only run previously failing tests
    pub failing_only: bool,
    /// Initialize the repository if it doesn't exist
    pub force_init: bool,
    /// Add/update failing tests without clearing previous failures
    pub partial: bool,
    /// Auto-detect and generate config if missing
    pub auto: bool,
    /// Path to a file containing test IDs to run
    pub load_list: Option<String>,
    /// Number of parallel test workers
    pub concurrency: Option<usize>,
    /// Run tests repeatedly until they fail
    pub until_failure: bool,
    /// Maximum number of iterations for until_failure mode
    pub max_iterations: Option<usize>,
    /// Run each test in a separate process
    pub isolated: bool,
    /// Output in subunit format instead of showing progress
    pub subunit: bool,
    /// Show all test output instead of just failures
    pub all_output: bool,
    /// Test patterns to filter
    pub test_filters: Option<Vec<String>>,
    /// Additional arguments to pass to the test command
    pub test_args: Option<Vec<String>>,
    /// Per-test timeout setting
    pub test_timeout: TimeoutSetting,
    /// Overall run timeout setting
    pub max_duration: TimeoutSetting,
    /// Kill test if no output for this duration
    pub no_output_timeout: Option<Duration>,
    /// Maximum number of restarts on timeout or crash (None = default).
    pub max_restarts: Option<usize>,
    /// Optional explicit ordering strategy. If `None`, the strategy comes
    /// from the configuration file (defaulting to discovery order).
    pub test_order: Option<TestOrder>,
    /// Optional shared buffer for capturing child-process stderr in addition
    /// to terminal forwarding. Used by the MCP server to surface stderr in
    /// the failure response.
    pub stderr_capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    /// Optional slot that receives the run ID as soon as it is allocated,
    /// before the tests finish executing. Used by the MCP server's
    /// `background_after` feature to return a run handle to callers while
    /// the run continues in the background.
    pub run_id_slot: Option<std::sync::Arc<std::sync::Mutex<Option<crate::repository::RunId>>>>,
    /// Optional cancellation token forwarded to the test executor. Lets
    /// the MCP server's `inq_cancel` tool stop a run that was handed off
    /// to the caller via `background_after`.
    pub cancellation_token: Option<crate::test_executor::CancellationToken>,
}

impl RunCommand {
    /// Creates a new run command with default settings.
    pub fn new(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            ..Default::default()
        }
    }

    /// Creates a run command that only runs previously failing tests.
    pub fn with_failing_only(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            failing_only: true,
            partial: true,
            ..Default::default()
        }
    }

    fn executor_config(&self) -> TestExecutorConfig {
        TestExecutorConfig {
            base_path: self.base_path.clone(),
            all_output: self.all_output,
            test_args: self.test_args.clone(),
            cancellation_token: self.cancellation_token.clone(),
            max_restarts: self.max_restarts,
            stderr_capture: self.stderr_capture.clone(),
        }
    }
}

impl Command for RunCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let output = self.execute_returning_run_id(ui)?;
        Ok(output.exit_code)
    }

    fn name(&self) -> &str {
        "run"
    }

    fn help(&self) -> &str {
        "Run tests and load results into the repository"
    }
}

impl RunCommand {
    /// If a `run_id_slot` was supplied, store the newly allocated run ID in it
    /// so external observers (e.g. the MCP server's `background_after` logic)
    /// can see the ID before the run finishes.
    fn publish_run_id(&self, run_id: &crate::repository::RunId) {
        if let Some(ref slot) = self.run_id_slot {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(run_id.clone());
            }
        }
    }

    /// Persist the executor output into the repository, display the summary,
    /// and repack the result as a `CliRunOutput`.
    fn persist(
        &self,
        ui: &mut dyn UI,
        repo: &mut dyn crate::repository::Repository,
        output: crate::test_executor::RunOutput,
        historical_times: &std::collections::HashMap<crate::repository::TestId, Duration>,
    ) -> Result<CliRunOutput> {
        let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
            ui,
            repo,
            output,
            self.partial,
            historical_times,
        )?;
        Ok(CliRunOutput {
            exit_code,
            run_id: Some(run_id),
        })
    }

    /// Execute tests and return both the exit code and the run ID.
    pub fn execute_returning_run_id(&self, ui: &mut dyn UI) -> Result<CliRunOutput> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        if self.auto && crate::config::TestrConfig::find_in_directory(base).is_err() {
            let auto_cmd = crate::commands::auto::AutoCommand::new(self.base_path.clone());
            let exit_code = auto_cmd.execute(ui)?;
            if exit_code != 0 {
                return Ok(CliRunOutput::no_run(exit_code));
            }
        }

        let mut repo =
            open_or_init_repository(self.base_path.as_deref(), self.force_init || self.auto, ui)?;

        let test_cmd = TestCommand::from_directory(base)?;

        let (test_timeout, max_duration, no_output_timeout) = test_executor::resolve_timeouts(
            &self.test_timeout,
            &self.max_duration,
            self.no_output_timeout,
            &test_cmd,
        )?;

        let mut test_ids = if self.failing_only {
            let failing = repo.get_failing_tests()?;
            if failing.is_empty() {
                ui.output("No failing tests to run")?;
                return Ok(CliRunOutput::no_run(0));
            }
            Some(failing)
        } else {
            None
        };

        if let Some(ref load_list_path) = self.load_list {
            let load_list_ids = crate::testlist::parse_list_file(Path::new(load_list_path))?;

            if let Some(existing_ids) = test_ids {
                let load_list_set: std::collections::HashSet<_> = load_list_ids.iter().collect();
                test_ids = Some(
                    existing_ids
                        .into_iter()
                        .filter(|id| load_list_set.contains(id))
                        .collect(),
                );
            } else {
                test_ids = Some(load_list_ids);
            }
        }

        if let Some(ref filters) = self.test_filters {
            use regex::Regex;

            let compiled_filters: Result<Vec<Regex>> = filters
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
            let compiled_filters = compiled_filters?;

            let all_test_ids = if let Some(ids) = test_ids {
                ids
            } else {
                test_cmd.list_tests()?
            };

            let filtered_ids: Vec<_> = all_test_ids
                .into_iter()
                .filter(|test_id| {
                    compiled_filters
                        .iter()
                        .any(|re| re.is_match(test_id.as_str()))
                })
                .collect();

            test_ids = Some(filtered_ids);
        }

        let historical_times = repo.get_test_times().unwrap_or_default();
        let max_duration_value =
            test_executor::compute_max_duration(&max_duration, &historical_times);
        let test_timeout_fn =
            test_executor::build_test_timeout_fn(&test_timeout, &historical_times);

        // Resolve the ordering strategy: CLI override > config > Default.
        let resolved_order = match &self.test_order {
            Some(o) => o.clone(),
            None => test_cmd.config().parsed_test_order()?,
        };

        // Apply ordering. If the strategy is non-default we need a concrete
        // list of tests to reorder, so materialise via discovery if the
        // earlier filtering didn't already produce one.
        if resolved_order != TestOrder::Discovery {
            let materialised = match test_ids.take() {
                Some(ids) => ids,
                None => test_cmd.list_tests()?,
            };
            let failing_for_order =
                if matches!(resolved_order, TestOrder::FailingFirst) && !self.failing_only {
                    repo.get_failing_tests().unwrap_or_default()
                } else {
                    Vec::new()
                };
            let ctx = OrderingContext {
                failing_tests: &failing_for_order,
                historical_times: &historical_times,
                group_regex: test_cmd.config().group_regex.as_deref(),
            };
            test_ids = Some(apply_order(materialised, &resolved_order, &ctx)?);
        }

        let config = self.executor_config();
        let executor = TestExecutor::new(&config);

        if self.subunit {
            let (run_id, writer) = repo.begin_test_run_raw()?;
            self.publish_run_id(&run_id);
            let output =
                executor.run_subunit(ui, &test_cmd, test_ids.as_deref(), run_id, writer)?;
            return self.persist(ui, repo.as_mut(), output, &historical_times);
        }

        let concurrency = if let Some(explicit_concurrency) = self.concurrency {
            if explicit_concurrency == 0 {
                let cpu_count = num_cpus::get();
                ui.output(&format!(
                    "Auto-detected {} CPUs for parallel execution",
                    cpu_count
                ))?;
                cpu_count
            } else {
                explicit_concurrency
            }
        } else if let Some(callout_concurrency) = test_cmd.get_concurrency()? {
            ui.output(&format!(
                "Using concurrency from test_run_concurrency: {}",
                callout_concurrency
            ))?;
            callout_concurrency
        } else {
            1
        };

        if self.isolated {
            let all_tests = if let Some(ids) = test_ids {
                ids
            } else {
                test_cmd.list_tests()?
            };

            if all_tests.is_empty() {
                ui.output("No tests to run")?;
                return Ok(CliRunOutput::no_run(0));
            }

            let run_id = repo.get_next_run_id()?;
            if !self.until_failure {
                // until_failure publishes per-iteration below.
                self.publish_run_id(&run_id);
            }

            if self.until_failure {
                let mut iteration = 1;
                loop {
                    if let Some(max) = self.max_iterations {
                        if iteration > max {
                            ui.output(&format!(
                                "\nReached maximum iteration limit ({}), stopping",
                                max
                            ))?;
                            return Ok(CliRunOutput::no_run(0));
                        }
                    }
                    ui.output(&format!("\n=== Iteration {} ===", iteration))?;
                    let iter_run_id = if iteration == 1 {
                        run_id.clone()
                    } else {
                        repo.get_next_run_id()?
                    };
                    self.publish_run_id(&iter_run_id);
                    let output = executor.run_isolated(
                        ui,
                        &test_cmd,
                        &all_tests,
                        test_timeout_fn.as_ref(),
                        max_duration_value,
                        iter_run_id,
                    )?;
                    let result = self.persist(ui, repo.as_mut(), output, &historical_times)?;

                    if result.exit_code != 0 {
                        ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                        return Ok(result);
                    }

                    iteration += 1;
                }
            } else {
                let output = executor.run_isolated(
                    ui,
                    &test_cmd,
                    &all_tests,
                    test_timeout_fn.as_ref(),
                    max_duration_value,
                    run_id,
                )?;
                self.persist(ui, repo.as_mut(), output, &historical_times)
            }
        } else if self.until_failure {
            let mut iteration = 1;
            loop {
                if let Some(max) = self.max_iterations {
                    if iteration > max {
                        ui.output(&format!(
                            "\nReached maximum iteration limit ({}), stopping",
                            max
                        ))?;
                        return Ok(CliRunOutput::no_run(0));
                    }
                }
                ui.output(&format!("\n=== Iteration {} ===", iteration))?;

                let output = if concurrency > 1 {
                    let run_id = repo.get_next_run_id()?;
                    self.publish_run_id(&run_id);
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
                        || repo.begin_test_run_raw().map(|(_, w)| w),
                    )?
                } else {
                    let (run_id, writer) = repo.begin_test_run_raw()?;
                    self.publish_run_id(&run_id);
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
                    )?
                };

                let result = self.persist(ui, repo.as_mut(), output, &historical_times)?;

                if result.exit_code != 0 {
                    ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                    return Ok(result);
                }

                iteration += 1;
            }
        } else if concurrency > 1 {
            let run_id = repo.get_next_run_id()?;
            self.publish_run_id(&run_id);
            let output = executor.run_parallel(
                ui,
                &test_cmd,
                test_ids.as_deref(),
                concurrency,
                max_duration_value,
                no_output_timeout,
                test_timeout_fn.as_ref(),
                run_id,
                &historical_times,
                || repo.begin_test_run_raw().map(|(_, w)| w),
            )?;
            self.persist(ui, repo.as_mut(), output, &historical_times)
        } else {
            let (run_id, writer) = repo.begin_test_run_raw()?;
            self.publish_run_id(&run_id);
            let output = executor.run_serial(
                ui,
                &test_cmd,
                test_ids.as_deref(),
                max_duration_value,
                no_output_timeout,
                test_timeout_fn.as_ref(),
                run_id,
                writer,
                &historical_times,
            )?;
            self.persist(ui, repo.as_mut(), output, &historical_times)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::RepositoryFactory;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_run_command_no_config() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert!(result.is_err());
    }

    #[test]
    fn test_run_command_with_failing_only_no_failures() {
        let temp = TempDir::new().unwrap();

        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut test_run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        test_run.add_result(crate::repository::TestResult {
            test_id: crate::repository::TestId::new("test1"),
            status: crate::repository::TestStatus::Success,
            duration: None,
            message: None,
            details: None,
            tags: vec![],
        });
        repo.insert_test_run(test_run).unwrap();

        let config = r#"
[DEFAULT]
test_command=echo "test1"
"#;
        fs::write(temp.path().join(".testr.conf"), config).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::with_failing_only(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 1);
        assert_eq!(ui.output[0], "No failing tests to run");
    }

    #[test]
    fn test_run_command_name() {
        let cmd = RunCommand::new(None);
        assert_eq!(cmd.name(), "run");
    }
}
