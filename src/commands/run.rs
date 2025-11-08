//! Run tests and load results into the repository

use crate::commands::utils::{init_repository, open_repository};
use crate::commands::Command;
use crate::error::Result;
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;

pub struct RunCommand {
    base_path: Option<String>,
    failing_only: bool,
    force_init: bool,
    partial: bool,
    load_list: Option<String>,
    concurrency: Option<usize>,
    until_failure: bool,
    isolated: bool,
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
            isolated: false,
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
            isolated: false,
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
            isolated: false,
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
            isolated: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_all_options(
        base_path: Option<String>,
        partial: bool,
        failing_only: bool,
        force_init: bool,
        load_list: Option<String>,
        concurrency: Option<usize>,
        until_failure: bool,
        isolated: bool,
    ) -> Self {
        RunCommand {
            base_path,
            failing_only,
            force_init,
            partial,
            load_list,
            concurrency,
            until_failure,
            isolated,
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

        // Create progress spinner
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        spinner.set_message("Running tests...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(100));

        // Execute test command
        let output = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                spinner.finish_and_clear();
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test command: {}",
                    e
                ))
            })?;

        // Check if command succeeded
        let command_failed = !output.status.success();

        // Parse subunit output with progress reporting
        spinner.set_message("Running tests...");
        let test_run = subunit_stream::parse_stream_with_progress(
            output.stdout.as_slice(),
            run_id,
            |test_id, status| {
                let indicator = status.indicator();
                if !indicator.is_empty() {
                    spinner.set_message(format!("{} {}", indicator, test_id));
                }
            },
        )?;
        spinner.finish_and_clear();

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

        // Get historical test durations for these specific tests
        let durations = repo.get_test_times_for_ids(&all_tests)?;

        // Get group_regex from config if present
        let group_regex = test_cmd.config().group_regex.as_deref();

        // Partition tests across workers
        let partitions = crate::partition::partition_tests_with_grouping(
            &all_tests,
            &durations,
            concurrency,
            group_regex,
        )
        .map_err(|e| crate::error::Error::Config(format!("Invalid group_regex pattern: {}", e)))?;

        // Create multi-progress for tracking all workers
        let multi_progress = indicatif::MultiProgress::new();
        let overall_bar = multi_progress.add(ProgressBar::new(all_tests.len() as u64));
        overall_bar.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} tests")
                .unwrap()
                .progress_chars("█▓▒░  "),
        );
        overall_bar.set_message(format!("Running across {} workers", concurrency));

        // Provision instances if configured
        let instance_ids = test_cmd.provision_instances(concurrency)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output(&format!("Provisioned {} instances", instance_ids.len()))?;
        }

        // Ensure instances are disposed even if we panic or error
        let dispose_guard = InstanceDisposeGuard {
            test_cmd,
            instance_ids: &instance_ids,
        };

        // Spawn worker processes with progress bars and streaming parsers
        let mut workers = Vec::new();
        let mut parse_threads = Vec::new();

        for (worker_id, partition) in partitions.iter().enumerate() {
            if partition.is_empty() {
                continue;
            }

            // Create a progress bar for this worker
            let worker_bar = multi_progress.add(ProgressBar::new(partition.len() as u64));
            worker_bar.set_style(
                ProgressStyle::default_bar()
                    .template(&format!(
                        "Worker {}: [{{bar:20.green/blue}}] {{pos}}/{{len}} {{msg}}",
                        worker_id
                    ))
                    .unwrap()
                    .progress_chars("█▓▒░  "),
            );

            // Build command for this partition with instance ID
            let instance_id = instance_ids.get(worker_id).map(|s| s.as_str());
            let (cmd_str, _temp_file) =
                test_cmd.build_command_with_instance(Some(partition), false, instance_id)?;

            // Spawn the worker process
            let mut child = Command::new("sh")
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

            // Take stdout/stderr for streaming
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");

            // Spawn thread to parse output in real-time
            let worker_bar_clone = worker_bar.clone();
            let overall_bar_clone = overall_bar.clone();
            let worker_run_id = format!("{}-{}", run_id, worker_id);

            let parse_thread = std::thread::spawn(move || {
                // Parse stdout stream directly for real-time progress
                let result = subunit_stream::parse_stream_with_progress(
                    stdout,
                    worker_run_id.clone(),
                    |test_id, status| {
                        let indicator = status.indicator();
                        if !indicator.is_empty() {
                            worker_bar_clone.inc(1);
                            overall_bar_clone.inc(1);
                            let short_name = if test_id.len() > 40 {
                                &test_id[test_id.len() - 40..]
                            } else {
                                test_id
                            };
                            worker_bar_clone.set_message(format!("{} {}", indicator, short_name));
                        }
                    },
                );

                // If stdout parsing failed and stderr has content, try stderr
                if result.is_err() {
                    use std::io::Read;
                    let mut stderr_data = Vec::new();
                    if std::io::BufReader::new(stderr).read_to_end(&mut stderr_data).is_ok() && !stderr_data.is_empty() {
                        return subunit_stream::parse_stream_with_progress(
                            &stderr_data[..],
                            worker_run_id,
                            |test_id, status| {
                                let indicator = status.indicator();
                                if !indicator.is_empty() {
                                    worker_bar_clone.inc(1);
                                    overall_bar_clone.inc(1);
                                    let short_name = if test_id.len() > 40 {
                                        &test_id[test_id.len() - 40..]
                                    } else {
                                        test_id
                                    };
                                    worker_bar_clone.set_message(format!("{} {}", indicator, short_name));
                                }
                            },
                        );
                    }
                }

                result
            });

            parse_threads.push((worker_id, worker_bar, parse_thread));
            workers.push((worker_id, child, _temp_file));
        }

        // Wait for all workers and parse threads to complete
        let mut all_results = HashMap::new();
        let mut any_failed = false;

        for (worker_id, mut child, _temp_file) in workers {
            let status = child.wait().map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to wait for worker {}: {}",
                    worker_id, e
                ))
            })?;

            if !status.success() {
                any_failed = true;
            }
        }

        // Collect results from parse threads
        for (worker_id, worker_bar, parse_thread) in parse_threads {
            let worker_run = parse_thread.join().map_err(|_| {
                crate::error::Error::CommandExecution(format!(
                    "Parse thread {} panicked",
                    worker_id
                ))
            })??;

            worker_bar.finish_with_message("done");

            // Add worker tag to all results
            let worker_tag = format!("worker-{}", worker_id);
            let mut worker_run = worker_run;
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

        // Finish progress bars
        overall_bar.finish_and_clear();

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

        // Dispose instances (done explicitly before drop to handle errors)
        drop(dispose_guard);
        test_cmd.dispose_instances(&instance_ids)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output("Disposed instances")?;
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

    /// Run each test in complete isolation (one test per process)
    fn run_isolated(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: &[crate::repository::TestId],
        run_id: String,
    ) -> Result<i32> {
        use std::collections::HashMap;
        use std::process::{Command, Stdio};

        ui.output(&format!(
            "Running {} tests in isolated mode (one test per process)",
            test_ids.len()
        ))?;

        let mut all_results = HashMap::new();
        let mut any_failed = false;

        for (idx, test_id) in test_ids.iter().enumerate() {
            ui.output(&format!("  [{}/{}] {}", idx + 1, test_ids.len(), test_id))?;

            // Build command for this single test
            let (cmd_str, _temp_file) =
                test_cmd.build_command(Some(std::slice::from_ref(test_id)), false)?;

            // Spawn process for this test
            let output = Command::new("sh")
                .arg("-c")
                .arg(&cmd_str)
                .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(|e| {
                    crate::error::Error::CommandExecution(format!(
                        "Failed to execute test {}: {}",
                        test_id, e
                    ))
                })?;

            if !output.status.success() {
                any_failed = true;
            }

            // Parse test results
            let test_run_id = format!("{}-{}", run_id, idx);
            let test_run = subunit_stream::parse_stream(output.stdout.as_slice(), test_run_id)?;

            // Collect results
            for (test_id, result) in test_run.results {
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

        // Determine concurrency level
        // Priority: 1) explicit --parallel flag, 2) test_run_concurrency callout, 3) default to 1
        let concurrency = if let Some(explicit_concurrency) = self.concurrency {
            if explicit_concurrency == 0 {
                // --parallel was given without a value, detect CPU count
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

        // For isolated mode, we need a list of tests
        if self.isolated {
            let all_tests = if let Some(ids) = test_ids {
                ids
            } else {
                // Need to list all tests
                test_cmd.list_tests()?
            };

            if all_tests.is_empty() {
                ui.output("No tests to run")?;
                return Ok(0);
            }

            // Run in isolated mode with optional until-failure loop
            if self.until_failure {
                let mut iteration = 1;
                loop {
                    ui.output(&format!("\n=== Iteration {} ===", iteration))?;
                    let run_id = repo.get_next_run_id()?.to_string();
                    let exit_code =
                        self.run_isolated(ui, &mut repo, &test_cmd, &all_tests, run_id)?;

                    if exit_code != 0 {
                        ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                        return Ok(exit_code);
                    }

                    iteration += 1;
                }
            } else {
                let run_id = repo.get_next_run_id()?.to_string();
                self.run_isolated(ui, &mut repo, &test_cmd, &all_tests, run_id)
            }
        } else if self.until_failure {
            // Run tests in a loop until failure (non-isolated)
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
            // Single run (non-isolated, non-looping)
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

/// RAII guard to ensure test instances are disposed
///
/// This struct ensures that test instances are properly cleaned up even if
/// an error occurs or panic happens during test execution.
struct InstanceDisposeGuard<'a> {
    test_cmd: &'a TestCommand,
    instance_ids: &'a [String],
}

impl<'a> Drop for InstanceDisposeGuard<'a> {
    fn drop(&mut self) {
        // Best effort cleanup - ignore errors during drop
        let _ = self.test_cmd.dispose_instances(self.instance_ids);
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
