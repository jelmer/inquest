//! Run tests and load results into the repository

use crate::commands::utils::open_or_init_repository;
use crate::commands::Command;
use crate::config::TimeoutSetting;
use crate::error::Result;
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
    pub run_id: Option<String>,
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
            cancellation_token: None,
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
    /// Execute tests and return both the exit code and the run ID.
    pub fn execute_returning_run_id(&self, ui: &mut dyn UI) -> Result<CliRunOutput> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        if self.auto && crate::config::TestrConfig::find_in_directory(base).is_err() {
            let auto_cmd = crate::commands::auto::AutoCommand::new(self.base_path.clone());
            let exit_code = auto_cmd.execute(ui)?;
            if exit_code != 0 {
                return Ok(CliRunOutput {
                    exit_code,
                    run_id: None,
                });
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
                return Ok(CliRunOutput {
                    exit_code: 0,
                    run_id: None,
                });
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

        let config = self.executor_config();
        let executor = TestExecutor::new(&config);

        if self.subunit {
            let (run_id, writer) = repo.begin_test_run_raw()?;
            let output =
                executor.run_subunit(ui, &test_cmd, test_ids.as_deref(), run_id, writer)?;
            let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                ui,
                repo.as_mut(),
                output,
                self.partial,
                &historical_times,
            )?;
            return Ok(CliRunOutput {
                exit_code,
                run_id: Some(run_id),
            });
        }

        let (concurrency, concurrency_source) = test_cmd.resolve_concurrency(self.concurrency)?;
        match &concurrency_source {
            crate::testcommand::ConcurrencySource::AutoDetected(cpus) => {
                ui.output(&format!("Auto-detected {} CPUs for parallel execution", cpus))?;
            }
            crate::testcommand::ConcurrencySource::ConfigCallout(c) => {
                ui.output(&format!("Using concurrency from test_run_concurrency: {}", c))?;
            }
            _ => {}
        }

        if self.isolated {
            let all_tests = if let Some(ids) = test_ids {
                ids
            } else {
                test_cmd.list_tests()?
            };

            if all_tests.is_empty() {
                ui.output("No tests to run")?;
                return Ok(CliRunOutput {
                    exit_code: 0,
                    run_id: None,
                });
            }

            let run_id = repo.get_next_run_id()?.to_string();

            if self.until_failure {
                let mut iteration = 1;
                loop {
                    if self.max_iterations.is_some_and(|max| iteration > max) {
                        ui.output(&format!(
                            "\nReached maximum iteration limit ({}), stopping",
                            self.max_iterations.unwrap()
                        ))?;
                        return Ok(CliRunOutput {
                            exit_code: 0,
                            run_id: None,
                        });
                    }
                    ui.output(&format!("\n=== Iteration {} ===", iteration))?;
                    let iter_run_id = if iteration == 1 {
                        run_id.clone()
                    } else {
                        repo.get_next_run_id()?.to_string()
                    };
                    let output = executor.run_isolated(
                        ui,
                        &test_cmd,
                        &all_tests,
                        test_timeout_fn.as_ref(),
                        max_duration_value,
                        iter_run_id,
                    )?;
                    let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                        ui,
                        repo.as_mut(),
                        output,
                        self.partial,
                        &historical_times,
                    )?;

                    if exit_code != 0 {
                        ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                        return Ok(CliRunOutput {
                            exit_code,
                            run_id: Some(run_id),
                        });
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
                let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                    ui,
                    repo.as_mut(),
                    output,
                    self.partial,
                    &historical_times,
                )?;
                Ok(CliRunOutput {
                    exit_code,
                    run_id: Some(run_id),
                })
            }
        } else if self.until_failure {
            let mut iteration = 1;
            loop {
                if self.max_iterations.is_some_and(|max| iteration > max) {
                    ui.output(&format!(
                        "\nReached maximum iteration limit ({}), stopping",
                        self.max_iterations.unwrap()
                    ))?;
                    return Ok(CliRunOutput {
                        exit_code: 0,
                        run_id: None,
                    });
                }
                ui.output(&format!("\n=== Iteration {} ===", iteration))?;

                let output = if concurrency > 1 {
                    let run_id = repo.get_next_run_id()?.to_string();
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

                let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                    ui,
                    repo.as_mut(),
                    output,
                    self.partial,
                    &historical_times,
                )?;

                if exit_code != 0 {
                    ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                    return Ok(CliRunOutput {
                        exit_code,
                        run_id: Some(run_id),
                    });
                }

                iteration += 1;
            }
        } else if concurrency > 1 {
            let run_id = repo.get_next_run_id()?.to_string();
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
            let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                ui,
                repo.as_mut(),
                output,
                self.partial,
                &historical_times,
            )?;
            Ok(CliRunOutput {
                exit_code,
                run_id: Some(run_id),
            })
        } else {
            let (run_id, writer) = repo.begin_test_run_raw()?;
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
            let (exit_code, run_id) = crate::commands::utils::persist_and_display_run(
                ui,
                repo.as_mut(),
                output,
                self.partial,
                &historical_times,
            )?;
            Ok(CliRunOutput {
                exit_code,
                run_id: Some(run_id),
            })
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

        let mut test_run = crate::repository::TestRun::new("0".to_string());
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
