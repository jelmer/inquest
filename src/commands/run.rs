//! Run tests and load results into the repository

use crate::commands::utils::{init_repository, open_repository};
use crate::commands::Command;
use crate::error::Result;
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::path::Path;

pub struct RunCommand {
    base_path: Option<String>,
    failing_only: bool,
    force_init: bool,
    partial: bool,
    load_list: Option<String>,
    concurrency: Option<usize>,
    until_failure: bool,
}

impl RunCommand {
    pub fn new(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            failing_only: false,
            force_init: false,
            partial: false,
            load_list: None,
            concurrency: None,
            until_failure: false,
        }
    }

    pub fn with_failing_only(base_path: Option<String>) -> Self {
        RunCommand {
            base_path,
            failing_only: true,
            force_init: false,
            partial: true, // --failing implies partial mode
            load_list: None,
            concurrency: None,
            until_failure: false,
        }
    }

    pub fn with_force_init(base_path: Option<String>, failing_only: bool) -> Self {
        RunCommand {
            base_path,
            failing_only,
            force_init: true,
            partial: failing_only, // --failing implies partial mode
            load_list: None,
            concurrency: None,
            until_failure: false,
        }
    }

    pub fn with_partial(
        base_path: Option<String>,
        partial: bool,
        failing_only: bool,
        force_init: bool,
    ) -> Self {
        RunCommand {
            base_path,
            failing_only,
            force_init,
            partial,
            load_list: None,
            concurrency: None,
            until_failure: false,
        }
    }

    pub fn with_all_options(
        base_path: Option<String>,
        partial: bool,
        failing_only: bool,
        force_init: bool,
        load_list: Option<String>,
        concurrency: Option<usize>,
        until_failure: bool,
    ) -> Self {
        RunCommand {
            base_path,
            failing_only,
            force_init,
            partial,
            load_list,
            concurrency,
            until_failure,
        }
    }

    /// Run tests serially (single process)
    fn run_serial(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
        run_id: String,
    ) -> Result<i32> {
        use std::process::{Command, Stdio};

        // Build command with test IDs if provided
        let (cmd_str, _temp_file) = test_cmd.build_command(test_ids, false)?;

        // Execute test command
        ui.output(&format!("Running: {}", cmd_str))?;

        let output = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test command: {}",
                    e
                ))
            })?;

        // Check if command succeeded
        let command_failed = !output.status.success();

        // Parse subunit output
        let test_run = subunit_stream::parse_stream(output.stdout.as_slice(), run_id)?;

        // Store results
        if self.partial {
            repo.insert_test_run_partial(test_run.clone(), true)?;
        } else {
            repo.insert_test_run(test_run.clone())?;
        }

        // Display summary
        let total = test_run.total_tests();
        let failures = test_run.count_failures();
        let successes = test_run.count_successes();

        ui.output("\nTest run complete:")?;
        ui.output(&format!("  Total:   {}", total))?;
        ui.output(&format!("  Passed:  {}", successes))?;
        ui.output(&format!("  Failed:  {}", failures))?;

        // Return exit code based on results
        if failures > 0 || command_failed {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Run tests in parallel across multiple workers
    fn run_parallel(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
        run_id: String,
        concurrency: usize,
    ) -> Result<i32> {
        use std::collections::HashMap;
        use std::process::{Command, Stdio};

        // Get the list of tests to run
        let all_tests = if let Some(ids) = test_ids {
            ids.to_vec()
        } else {
            // Need to list all tests
            test_cmd.list_tests()?
        };

        if all_tests.is_empty() {
            ui.output("No tests to run")?;
            return Ok(0);
        }

        // Get historical test durations
        let durations = repo.get_test_times()?;

        // Partition tests across workers
        let partitions = crate::partition::partition_tests(&all_tests, &durations, concurrency);

        ui.output(&format!(
            "Running {} tests across {} workers",
            all_tests.len(),
            concurrency
        ))?;

        // Spawn worker processes
        let mut workers = Vec::new();
        for (worker_id, partition) in partitions.iter().enumerate() {
            if partition.is_empty() {
                continue;
            }

            ui.output(&format!("Worker {}: {} tests", worker_id, partition.len()))?;

            // Build command for this partition
            let (cmd_str, _temp_file) = test_cmd.build_command(Some(partition), false)?;

            // Spawn the worker process
            let child = Command::new("sh")
                .arg("-c")
                .arg(&cmd_str)
                .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    crate::error::Error::CommandExecution(format!(
                        "Failed to spawn worker {}: {}",
                        worker_id, e
                    ))
                })?;

            workers.push((worker_id, child, _temp_file));
        }

        // Wait for all workers to complete and collect results
        let mut all_results = HashMap::new();
        let mut any_failed = false;

        for (worker_id, child, _temp_file) in workers {
            let output = child.wait_with_output().map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to wait for worker {}: {}",
                    worker_id, e
                ))
            })?;

            if !output.status.success() {
                ui.warning(&format!("Worker {} exited with non-zero status", worker_id))?;
                any_failed = true;
            }

            // Parse worker results
            let worker_run_id = format!("{}-{}", run_id, worker_id);
            let mut worker_run =
                subunit_stream::parse_stream(output.stdout.as_slice(), worker_run_id)?;

            // Add worker tag to all results
            let worker_tag = format!("worker-{}", worker_id);
            for (_, result) in worker_run.results.iter_mut() {
                if !result.tags.contains(&worker_tag) {
                    result.tags.push(worker_tag.clone());
                }
            }

            // Collect results
            for (test_id, result) in worker_run.results {
                all_results.insert(test_id, result);
            }
        }

        // Create combined test run
        let mut combined_run = crate::repository::TestRun::new(run_id);
        combined_run.timestamp = chrono::Utc::now();

        for (_, result) in all_results {
            combined_run.add_result(result);
        }

        // Store combined results
        if self.partial {
            repo.insert_test_run_partial(combined_run.clone(), true)?;
        } else {
            repo.insert_test_run(combined_run.clone())?;
        }

        // Display summary
        let total = combined_run.total_tests();
        let failures = combined_run.count_failures();
        let successes = combined_run.count_successes();

        ui.output("\nTest run complete:")?;
        ui.output(&format!("  Total:   {}", total))?;
        ui.output(&format!("  Passed:  {}", successes))?;
        ui.output(&format!("  Failed:  {}", failures))?;

        // Return exit code based on results
        if failures > 0 || any_failed {
            Ok(1)
        } else {
            Ok(0)
        }
    }
}

impl Command for RunCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        // Open repository
        let mut repo = if self.force_init {
            // Try to open, if it fails, initialize
            open_repository(self.base_path.as_deref())
                .or_else(|_| init_repository(self.base_path.as_deref()))?
        } else {
            open_repository(self.base_path.as_deref())?
        };

        // Load test command configuration
        let test_cmd = TestCommand::from_directory(base)?;

        // Determine which tests to run
        let mut test_ids = if self.failing_only {
            let latest = repo.get_latest_run()?;
            let failing = latest.get_failing_tests();
            if failing.is_empty() {
                ui.output("No failing tests to run")?;
                return Ok(0);
            }
            Some(failing.iter().map(|id| (*id).clone()).collect::<Vec<_>>())
        } else {
            None
        };

        // Apply --load-list filter if provided
        if let Some(ref load_list_path) = self.load_list {
            let load_list_ids = crate::testlist::parse_list_file(Path::new(load_list_path))?;

            if let Some(existing_ids) = test_ids {
                // Intersect with existing list (e.g., from --failing)
                let load_list_set: std::collections::HashSet<_> = load_list_ids.iter().collect();
                test_ids = Some(
                    existing_ids
                        .into_iter()
                        .filter(|id| load_list_set.contains(id))
                        .collect(),
                );
            } else {
                // Use load-list verbatim
                test_ids = Some(load_list_ids);
            }
        }

        // Check if we should run in parallel
        let concurrency = self.concurrency.unwrap_or(1);

        // Run tests in a loop if --until-failure is set
        if self.until_failure {
            let mut iteration = 1;
            loop {
                ui.output(&format!("\n=== Iteration {} ===", iteration))?;

                // Get the next run ID for this iteration
                let run_id = repo.get_next_run_id()?.to_string();

                let exit_code = if concurrency > 1 {
                    self.run_parallel(
                        ui,
                        &mut repo,
                        &test_cmd,
                        test_ids.as_deref(),
                        run_id,
                        concurrency,
                    )?
                } else {
                    self.run_serial(ui, &mut repo, &test_cmd, test_ids.as_deref(), run_id)?
                };

                // Stop if tests failed
                if exit_code != 0 {
                    ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                    return Ok(exit_code);
                }

                iteration += 1;
            }
        } else {
            // Single run
            let run_id = repo.get_next_run_id()?.to_string();

            if concurrency > 1 {
                // Parallel execution
                self.run_parallel(
                    ui,
                    &mut repo,
                    &test_cmd,
                    test_ids.as_deref(),
                    run_id,
                    concurrency,
                )
            } else {
                // Serial execution
                self.run_serial(ui, &mut repo, &test_cmd, test_ids.as_deref(), run_id)
            }
        }
    }

    fn name(&self) -> &str {
        "run"
    }

    fn help(&self) -> &str {
        "Run tests and load results into the repository"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::file::FileRepositoryFactory;
    use crate::repository::RepositoryFactory;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_run_command_no_config() {
        let temp = TempDir::new().unwrap();

        // Initialize repo but no .testr.conf
        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        // Should fail due to missing config
        assert!(result.is_err());
    }

    #[test]
    fn test_run_command_with_failing_only_no_failures() {
        let temp = TempDir::new().unwrap();

        // Initialize repo
        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Add a passing test run
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

        // Need a .testr.conf file
        let config = r#"
[DEFAULT]
test_command=echo "test1"
"#;
        fs::write(temp.path().join(".testr.conf"), config).unwrap();

        let mut ui = TestUI::new();
        let cmd = RunCommand::with_failing_only(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        // Should succeed with "No failing tests to run"
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
