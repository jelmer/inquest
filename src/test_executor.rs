//! Test execution engine
//!
//! Core logic for running tests. This module is independent of the repository
//! and CLI — the caller is responsible for allocating run IDs, creating writers,
//! fetching historical times, and persisting results after execution.

use crate::config::{
    TimeoutSetting, AUTO_MAX_DURATION_MINIMUM, AUTO_TIMEOUT_MULTIPLIER, MAX_TEST_TIMEOUT_RESTARTS,
};
use crate::error::Result;
use crate::repository::{TestId, TestResult, TestRun};
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use crate::watchdog::{wait_with_timeout, TestWatchdog, TimeoutReason};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Buffer size for the channel between I/O threads and the parse thread.
const STREAM_BUFFER_SIZE: usize = 100;

/// Fixed-width portion of the progress bar (elapsed time, counters, spacing).
const PROGRESS_FIXED_WIDTH: usize = 25;

/// Extra padding subtracted when computing bar width.
const PROGRESS_PADDING: usize = 30;

/// Maximum number of workers whose individual bars are scaled to fit the terminal.
const MAX_DISPLAY_WORKERS: usize = 4;

/// Type alias for the per-test timeout lookup function.
pub type TestTimeoutFn = Arc<dyn Fn(&str) -> Option<Duration> + Send + Sync>;

/// Result of a test execution, containing everything the caller needs for persistence.
pub struct RunOutput {
    /// The run ID assigned to this execution.
    pub run_id: String,
    /// Collected test results, keyed by test ID.
    pub results: HashMap<TestId, TestResult>,
    /// Whether any test command exited with failure status.
    pub any_command_failed: bool,
    /// Wall-clock duration of the entire execution.
    pub duration: Duration,
    /// The test command string that was executed.
    pub test_command: String,
    /// Number of parallel workers used (1 for serial/subunit/isolated).
    pub concurrency: u32,
}

impl RunOutput {
    /// Compute the exit code from the results.
    pub fn exit_code(&self) -> i32 {
        let has_failures = self.results.values().any(|r| {
            matches!(
                r.status,
                crate::repository::TestStatus::Failure | crate::repository::TestStatus::Error
            )
        });
        if has_failures || self.any_command_failed {
            1
        } else {
            0
        }
    }

    /// Build a `TestRun` suitable for insertion into the repository.
    pub fn into_test_run(self) -> TestRun {
        let mut run = TestRun::new(self.run_id);
        run.timestamp = chrono::Utc::now();
        for (_, result) in self.results {
            run.add_result(result);
        }
        run
    }
}

/// Configuration for test execution, independent of CLI argument parsing.
pub struct TestExecutorConfig {
    /// Working directory for test execution.
    pub base_path: Option<String>,
    /// If true, show all test output instead of just failures.
    pub all_output: bool,
    /// Additional arguments to pass to the test command.
    pub test_args: Option<Vec<String>>,
}

/// Executes tests without touching the repository.
///
/// The caller is responsible for:
/// - Allocating run IDs and writers via the repository
/// - Fetching historical test times
/// - Persisting the returned `RunOutput` to the repository
pub struct TestExecutor<'a> {
    config: &'a TestExecutorConfig,
}

impl<'a> TestExecutor<'a> {
    /// Create a new executor with the given configuration.
    pub fn new(config: &'a TestExecutorConfig) -> Self {
        Self { config }
    }

    /// Run tests and output raw subunit stream (no progress bars).
    ///
    /// The caller must pre-allocate the run via `repo.begin_test_run_raw()` and
    /// pass the resulting `(run_id, writer)`.
    pub fn run_subunit(
        &self,
        ui: &mut dyn UI,
        test_cmd: &TestCommand,
        test_ids: Option<&[TestId]>,
        run_id: String,
        raw_writer: Box<dyn std::io::Write + Send>,
    ) -> Result<RunOutput> {
        use std::io::Write;

        let (cmd_str, _temp_file) =
            test_cmd.build_command_full(test_ids, false, None, self.config.test_args.as_deref())?;

        let mut child = spawn_in_process_group(
            &cmd_str,
            Path::new(self.config.base_path.as_deref().unwrap_or(".")),
        )
        .map_err(|e| {
            crate::error::Error::CommandExecution(format!("Failed to execute test command: {}", e))
        })?;

        let mut stdout = child.stdout.take().expect("stdout was piped");

        // Tee to three destinations: repo writer, UI output, and in-memory buffer for parsing
        let mut buffer = Vec::new();

        struct TeeWriter3<'a, W1: Write, W2: Write> {
            writer1: W1,
            writer2: W2,
            buffer: &'a mut Vec<u8>,
        }

        impl<W1: Write, W2: Write> Write for TeeWriter3<'_, W1, W2> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.writer1.write_all(buf)?;
                self.writer2.write_all(buf)?;
                self.buffer.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.writer1.flush()?;
                self.writer2.flush()?;
                Ok(())
            }
        }

        struct UIWriter<'a> {
            ui: &'a mut dyn UI,
        }

        impl Write for UIWriter<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.ui.output_bytes(buf).map_err(std::io::Error::other)?;
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut tee = TeeWriter3 {
            writer1: raw_writer,
            writer2: UIWriter { ui },
            buffer: &mut buffer,
        };

        let start_time = std::time::Instant::now();

        std::io::copy(&mut stdout, &mut tee).map_err(crate::error::Error::Io)?;
        tee.flush().map_err(crate::error::Error::Io)?;

        let status = child.wait().map_err(|e| {
            crate::error::Error::CommandExecution(format!("Failed to wait for test command: {}", e))
        })?;

        let duration = start_time.elapsed();
        let any_command_failed = !status.success();

        drop(_temp_file);

        // Parse the buffered stream to extract test results
        let test_run = subunit_stream::parse_stream(buffer.as_slice(), run_id.clone())?;

        Ok(RunOutput {
            run_id,
            results: test_run.results,
            any_command_failed,
            duration,
            test_command: test_cmd.config().test_command.clone(),
            concurrency: 1,
        })
    }

    /// Run tests serially (single process), with per-test timeout and restart.
    ///
    /// The caller must pre-allocate the run via `repo.begin_test_run_raw()` and
    /// pass the resulting `(run_id, writer)`.
    #[allow(clippy::too_many_arguments)]
    pub fn run_serial(
        &self,
        _ui: &mut dyn UI,
        test_cmd: &TestCommand,
        test_ids: Option<&[TestId]>,
        max_duration: Option<Duration>,
        no_output_timeout: Option<Duration>,
        test_timeout_fn: Option<&TestTimeoutFn>,
        run_id: String,
        raw_writer: Box<dyn std::io::Write + Send>,
        historical_times: &HashMap<TestId, Duration>,
    ) -> Result<RunOutput> {
        let estimated_total: Duration = historical_times.values().sum();

        let mut remaining_tests: Option<Vec<TestId>> = test_ids.map(|ids| ids.to_vec());
        let mut all_results: HashMap<TestId, TestResult> = HashMap::new();
        let mut restarts = 0;
        let mut any_command_failed = false;

        let total_test_count = if let Some(ids) = test_ids {
            ids.len()
        } else {
            test_cmd.list_tests()?.len()
        };

        let start_time = std::time::Instant::now();
        let mut next_raw_writer: Option<Box<dyn std::io::Write + Send>> = Some(raw_writer);

        let term_width = console::Term::stdout().size().1 as usize;
        let layout = compute_progress_layout(term_width);
        let bar_width = layout.bar_width;
        let max_msg_len = layout.max_msg_len;

        let progress_bar = create_progress_bar(total_test_count as u64, bar_width);
        progress_bar.set_position(all_results.len() as u64);

        let output_filter = if self.config.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        loop {
            let current_ids = remaining_tests.as_deref();
            let (cmd_str, _temp_file) = test_cmd.build_command_full(
                current_ids,
                false,
                None,
                self.config.test_args.as_deref(),
            )?;
            let raw_writer: Box<dyn std::io::Write + Send> = next_raw_writer
                .take()
                .unwrap_or_else(|| Box::new(std::io::sink()));

            let mut child = spawn_in_process_group(
                &cmd_str,
                Path::new(self.config.base_path.as_deref().unwrap_or(".")),
            )
            .map_err(|e| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test command: {}",
                    e
                ))
            })?;

            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");

            let (tx, rx) = std::sync::mpsc::sync_channel(STREAM_BUFFER_SIZE);
            let activity_tracker =
                no_output_timeout.map(|_| crate::test_runner::ActivityTracker::new());
            let io_threads = IoThreads::spawn(
                stdout,
                stderr,
                raw_writer,
                tx,
                activity_tracker.as_ref(),
                progress_bar.clone(),
            );

            let watchdog = test_timeout_fn.as_ref().map(|_| TestWatchdog::new());
            let watchdog_for_thread = watchdog.clone();
            let test_timeout_fn_clone = test_timeout_fn.cloned();

            let progress_bar_clone = progress_bar.clone();
            let run_id_clone = run_id.clone();
            let channel_reader = crate::test_runner::ChannelReader::new(rx);
            let historical_times_for_thread = historical_times.clone();

            let parse_thread = std::thread::spawn(move || {
                let historical_times = historical_times_for_thread;
                let progress_bar_for_bytes = progress_bar_clone.clone();
                let mut tracker = ProgressTracker::new(
                    bar_width,
                    max_msg_len,
                    estimated_total,
                    start_time,
                    false,
                );

                subunit_stream::parse_stream_with_progress(
                    channel_reader,
                    run_id_clone,
                    |test_id, status| {
                        update_watchdog(
                            watchdog_for_thread.as_ref(),
                            test_timeout_fn_clone.as_ref(),
                            test_id,
                            status,
                        );

                        if !status.indicator().is_empty() {
                            tracker.on_test_complete(
                                &progress_bar_clone,
                                test_id,
                                status,
                                &historical_times,
                            );
                        }
                    },
                    |bytes| {
                        write_non_subunit_output(&progress_bar_for_bytes, bytes);
                    },
                    output_filter,
                )
            });

            let wait_result = wait_with_timeout(
                &mut child,
                max_duration.map(|d| d.saturating_sub(start_time.elapsed())),
                no_output_timeout,
                activity_tracker.as_ref(),
                watchdog.as_ref(),
            )
            .map_err(|e| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution(format!(
                    "Failed to wait for test command: {}",
                    e
                ))
            })?;

            drop(_temp_file);

            let partial_run = parse_thread.join().map_err(|_| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution("Parse thread panicked".to_string())
            })??;

            io_threads.join("serial")?;

            for (id, result) in partial_run.results {
                all_results.insert(id, result);
            }

            match wait_result {
                Err(TimeoutReason::TestTimeout(ref hung_test)) => {
                    tracing::warn!(
                        "test {} timed out, killing process and restarting",
                        hung_test
                    );
                    let test_id = TestId::new(hung_test);
                    all_results.insert(test_id.clone(), timeout_error_result(test_id));
                    any_command_failed = true;

                    if !test_cmd.supports_test_filtering() {
                        tracing::warn!(
                            "cannot restart: test command does not support \
                             filtering by test ID ($IDOPTION/$IDFILE/$IDLIST)"
                        );
                        break;
                    }

                    let completed_from_watchdog = watchdog
                        .as_ref()
                        .map(|wd| wd.completed_tests())
                        .unwrap_or_default();
                    let completed_in_results: HashSet<&str> =
                        all_results.keys().map(|id| id.as_str()).collect();
                    let discovered_tests;
                    let all_test_ids: &[TestId] = if let Some(ref ids) = remaining_tests {
                        ids
                    } else {
                        discovered_tests = test_cmd.list_tests()?;
                        &discovered_tests
                    };
                    let next_remaining = compute_remaining_tests(
                        all_test_ids,
                        &completed_from_watchdog,
                        &completed_in_results,
                        hung_test,
                    );

                    restarts += 1;
                    if restarts >= MAX_TEST_TIMEOUT_RESTARTS || next_remaining.is_empty() {
                        if restarts >= MAX_TEST_TIMEOUT_RESTARTS {
                            tracing::error!(
                                "exceeded maximum restart limit ({}), stopping",
                                MAX_TEST_TIMEOUT_RESTARTS
                            );
                        }
                        break;
                    }

                    tracing::warn!(
                        "restarting test runner with {} remaining tests",
                        next_remaining.len()
                    );
                    remaining_tests = Some(next_remaining);
                    continue;
                }
                Err(ref reason) => {
                    let elapsed = start_time.elapsed();
                    match reason {
                        TimeoutReason::Timeout => tracing::warn!(
                            "test run killed after {:.1}s (max duration exceeded)",
                            elapsed.as_secs_f64()
                        ),
                        TimeoutReason::NoOutput => tracing::warn!(
                            "test run killed after {:.1}s (no output for {:?})",
                            elapsed.as_secs_f64(),
                            no_output_timeout.unwrap()
                        ),
                        TimeoutReason::TestTimeout(_) => unreachable!(),
                    }
                    any_command_failed = true;
                    break;
                }
                Ok(status) => {
                    if !status.success() {
                        any_command_failed = true;
                    }
                    break;
                }
            }
        }

        progress_bar.finish_and_clear();

        Ok(RunOutput {
            run_id,
            results: all_results,
            any_command_failed,
            duration: start_time.elapsed(),
            test_command: test_cmd.config().test_command.clone(),
            concurrency: 1,
        })
    }

    /// Run tests in parallel across multiple workers, with per-test timeout and restart.
    ///
    /// The caller must pre-allocate `run_id` (e.g. via `repo.get_next_run_id().to_string()`).
    /// The `writer_factory` closure is called to create a writer for each worker on the
    /// first iteration; on restart iterations, workers write to `io::sink()`.
    #[allow(clippy::too_many_arguments)]
    pub fn run_parallel<F>(
        &self,
        ui: &mut dyn UI,
        test_cmd: &TestCommand,
        test_ids: Option<&[TestId]>,
        concurrency: usize,
        max_duration: Option<Duration>,
        no_output_timeout: Option<Duration>,
        test_timeout_fn: Option<&TestTimeoutFn>,
        run_id: String,
        historical_times: &HashMap<TestId, Duration>,
        mut writer_factory: F,
    ) -> Result<RunOutput>
    where
        F: FnMut() -> Result<Box<dyn std::io::Write + Send>>,
    {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let output_filter = if self.config.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        let start_time = std::time::Instant::now();

        let all_tests = if let Some(ids) = test_ids {
            ids.to_vec()
        } else {
            test_cmd.list_tests()?
        };

        if all_tests.is_empty() {
            ui.output("No tests to run")?;
            return Ok(RunOutput {
                run_id,
                results: HashMap::new(),
                any_command_failed: false,
                duration: start_time.elapsed(),
                test_command: test_cmd.config().test_command.clone(),
                concurrency: concurrency as u32,
            });
        }

        let durations: HashMap<TestId, Duration> = historical_times
            .iter()
            .filter(|(id, _)| all_tests.contains(id))
            .map(|(id, d)| (id.clone(), *d))
            .collect();

        let group_regex = test_cmd.config().group_regex.as_deref();

        let initial_partitions = crate::partition::partition_tests_with_grouping(
            &all_tests,
            &durations,
            concurrency,
            group_regex,
        )
        .map_err(|e| crate::error::Error::Config(format!("Invalid group_regex pattern: {}", e)))?;

        let partition_estimated_totals: Vec<Duration> = initial_partitions
            .iter()
            .map(|partition| partition.iter().filter_map(|id| durations.get(id)).sum())
            .collect();

        let term_width = console::Term::stdout().size().1 as usize;
        let overall_layout = compute_progress_layout(term_width);
        let overall_bar_width = overall_layout.bar_width;

        let multi_progress = indicatif::MultiProgress::new();
        let overall_bar = multi_progress.add(create_progress_bar(
            all_tests.len() as u64,
            overall_bar_width,
        ));

        let total_failures = Arc::new(AtomicUsize::new(0));

        let instance_ids = test_cmd.provision_instances(concurrency)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output(&format!("Provisioned {} instances", instance_ids.len()))?;
        }

        let dispose_guard = InstanceDisposeGuard {
            test_cmd,
            instance_ids: &instance_ids,
        };

        let mut all_results: HashMap<TestId, TestResult> = HashMap::new();
        let mut any_failed = false;
        let mut restarts = 0;
        let mut is_first_iteration = true;

        let mut pending_partitions: Vec<(usize, Vec<TestId>)> = initial_partitions
            .into_iter()
            .enumerate()
            .filter(|(_, p)| !p.is_empty())
            .collect();

        loop {
            let mut supervisors: Vec<(usize, std::thread::JoinHandle<SupervisorResult>)> =
                Vec::new();
            let mut worker_threads: Vec<WorkerThreads> = Vec::new();
            let mut _temp_files = Vec::new();

            for (worker_id, partition) in &pending_partitions {
                let worker_id = *worker_id;

                let worker_bar_width = ((term_width
                    .saturating_sub(PROGRESS_FIXED_WIDTH + PROGRESS_PADDING))
                    / concurrency.min(MAX_DISPLAY_WORKERS))
                .clamp(15, 40);
                let worker_max_msg = term_width
                    .saturating_sub(worker_bar_width + PROGRESS_FIXED_WIDTH)
                    .max(20);

                let worker_bar = multi_progress.add(ProgressBar::new(partition.len() as u64));
                worker_bar.set_style(
                    ProgressStyle::default_bar()
                        .template(&format!(
                            "Worker {}: [{{elapsed_precise}}] [{{bar:{}.green/blue}}] {{pos}}/{{len}} {{msg}}",
                            worker_id, worker_bar_width
                        ))
                        .unwrap()
                        .progress_chars("█▓▒░  "),
                );

                let instance_id = instance_ids.get(worker_id).map(|s| s.as_str());
                let (cmd_str, temp_file) = test_cmd.build_command_full(
                    Some(partition),
                    false,
                    instance_id,
                    self.config.test_args.as_deref(),
                )?;
                _temp_files.push(temp_file);

                let mut child = spawn_in_process_group(
                    &cmd_str,
                    Path::new(self.config.base_path.as_deref().unwrap_or(".")),
                )
                .map_err(|e| {
                    crate::error::Error::CommandExecution(format!(
                        "Failed to spawn worker {}: {}",
                        worker_id, e
                    ))
                })?;

                let stdout = child.stdout.take().expect("stdout was piped");
                let stderr = child.stderr.take().expect("stderr was piped");

                let worker_run_id = format!("{}-{}", run_id, worker_id);
                let raw_writer: Box<dyn std::io::Write + Send> = if is_first_iteration {
                    writer_factory()?
                } else {
                    Box::new(std::io::sink())
                };

                let (tx, rx) = std::sync::mpsc::sync_channel(STREAM_BUFFER_SIZE);
                let worker_activity =
                    no_output_timeout.map(|_| crate::test_runner::ActivityTracker::new());

                let io_threads = IoThreads::spawn(
                    stdout,
                    stderr,
                    raw_writer,
                    tx,
                    worker_activity.as_ref(),
                    worker_bar.clone(),
                );

                let channel_reader = crate::test_runner::ChannelReader::new(rx);

                let worker_watchdog = test_timeout_fn.as_ref().map(|_| TestWatchdog::new());
                let watchdog_for_thread = worker_watchdog.clone();
                let watchdog_for_supervisor = worker_watchdog.clone();
                let test_timeout_fn_clone = test_timeout_fn.cloned();

                let remaining_timeout =
                    max_duration.map(|d| d.saturating_sub(start_time.elapsed()));
                let supervisor = std::thread::spawn(move || {
                    wait_with_timeout(
                        &mut child,
                        remaining_timeout,
                        no_output_timeout,
                        worker_activity.as_ref(),
                        watchdog_for_supervisor.as_ref(),
                    )
                });

                let worker_bar_clone = worker_bar.clone();
                let overall_bar_clone = overall_bar.clone();
                let worker_run_id_clone = worker_run_id.clone();
                let total_failures_clone = Arc::clone(&total_failures);
                let worker_durations = durations.clone();
                let worker_estimated_total = partition_estimated_totals
                    .get(worker_id)
                    .copied()
                    .unwrap_or_default();

                let output_filter_clone = output_filter;
                let worker_start_time = std::time::Instant::now();
                let parse_thread = std::thread::spawn(move || {
                    let worker_bar_for_bytes = worker_bar_clone.clone();
                    let mut tracker = ProgressTracker::new(
                        worker_bar_width,
                        worker_max_msg,
                        worker_estimated_total,
                        worker_start_time,
                        true,
                    );

                    subunit_stream::parse_stream_with_progress(
                        channel_reader,
                        worker_run_id_clone,
                        |test_id, status| {
                            update_watchdog(
                                watchdog_for_thread.as_ref(),
                                test_timeout_fn_clone.as_ref(),
                                test_id,
                                status,
                            );

                            if !status.indicator().is_empty() {
                                overall_bar_clone.inc(1);
                                if matches!(
                                    status,
                                    subunit_stream::ProgressStatus::Failed
                                        | subunit_stream::ProgressStatus::UnexpectedSuccess
                                ) {
                                    let total =
                                        total_failures_clone.fetch_add(1, Ordering::Relaxed) + 1;
                                    let completed = overall_bar_clone.position();
                                    update_progress_bar_style(
                                        &overall_bar_clone,
                                        overall_bar_width,
                                        completed,
                                        total,
                                    );
                                    let msg = console::style(format!("failures: {}", total))
                                        .red()
                                        .to_string();
                                    overall_bar_clone.set_message(msg);
                                }

                                tracker.on_test_complete(
                                    &worker_bar_clone,
                                    test_id,
                                    status,
                                    &worker_durations,
                                );
                            }
                        },
                        |bytes| {
                            write_non_subunit_output(&worker_bar_for_bytes, bytes);
                        },
                        output_filter_clone,
                    )
                });

                supervisors.push((worker_id, supervisor));
                worker_threads.push(WorkerThreads {
                    worker_id,
                    bar: worker_bar,
                    parse: parse_thread,
                    io: io_threads,
                    watchdog: worker_watchdog,
                });
            }

            let supervisor_results = join_supervisors(supervisors)?;
            let worker_watchdogs = collect_worker_results(worker_threads, &mut all_results)?;

            let restart_partitions = compute_restart_partitions(
                &supervisor_results,
                &worker_watchdogs,
                &pending_partitions,
                &mut all_results,
                &mut any_failed,
                start_time,
                no_output_timeout,
            );

            is_first_iteration = false;

            if !restart_partitions.is_empty() && !test_cmd.supports_test_filtering() {
                tracing::warn!(
                    "cannot restart: test command does not support \
                     filtering by test ID ($IDOPTION/$IDFILE/$IDLIST)"
                );
                break;
            }

            restarts += 1;
            if restart_partitions.is_empty() || restarts >= MAX_TEST_TIMEOUT_RESTARTS {
                if restarts >= MAX_TEST_TIMEOUT_RESTARTS && !restart_partitions.is_empty() {
                    tracing::error!(
                        "exceeded maximum restart limit ({}), stopping",
                        MAX_TEST_TIMEOUT_RESTARTS
                    );
                }
                break;
            }

            tracing::warn!(
                "restarting {} workers with remaining tests",
                restart_partitions.len()
            );
            pending_partitions = restart_partitions;
        }

        overall_bar.finish_and_clear();

        // Explicitly dispose instances (with error propagation and UI feedback).
        // Forget the guard to avoid double-disposal.
        std::mem::forget(dispose_guard);
        test_cmd.dispose_instances(&instance_ids)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output("Disposed instances")?;
        }

        Ok(RunOutput {
            run_id,
            results: all_results,
            any_command_failed: any_failed,
            duration: start_time.elapsed(),
            test_command: test_cmd.config().test_command.clone(),
            concurrency: concurrency as u32,
        })
    }

    /// Run each test in complete isolation (one test per process).
    ///
    /// The caller must pre-allocate `run_id` (e.g. via `repo.get_next_run_id().to_string()`).
    #[allow(clippy::too_many_arguments)]
    pub fn run_isolated(
        &self,
        ui: &mut dyn UI,
        test_cmd: &TestCommand,
        test_ids: &[TestId],
        test_timeout_fn: Option<&TestTimeoutFn>,
        max_duration: Option<Duration>,
        run_id: String,
    ) -> Result<RunOutput> {
        let start_time = std::time::Instant::now();

        ui.output(&format!(
            "Running {} tests in isolated mode (one test per process)",
            test_ids.len()
        ))?;

        let mut all_results: HashMap<TestId, TestResult> = HashMap::new();
        let mut any_failed = false;

        for (idx, test_id) in test_ids.iter().enumerate() {
            if let Some(max_dur) = max_duration {
                if start_time.elapsed() >= max_dur {
                    tracing::warn!(
                        "max duration exceeded after {:.1}s, stopping after {}/{} tests",
                        start_time.elapsed().as_secs_f64(),
                        idx,
                        test_ids.len()
                    );
                    any_failed = true;
                    break;
                }
            }

            let per_test_timeout = test_timeout_fn.and_then(|f| f(test_id.as_str()));

            ui.output(&format!("  [{}/{}] {}", idx + 1, test_ids.len(), test_id))?;

            let (cmd_str, _temp_file) = test_cmd.build_command_full(
                Some(std::slice::from_ref(test_id)),
                false,
                None,
                self.config.test_args.as_deref(),
            )?;

            let test_start = std::time::Instant::now();
            let mut child = spawn_in_process_group(
                &cmd_str,
                Path::new(self.config.base_path.as_deref().unwrap_or(".")),
            )
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test {}: {}",
                    test_id, e
                ))
            })?;

            let stdout = child.stdout.take().expect("stdout was piped");
            let stdout_thread = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
                use std::io::Read;
                let mut buf = Vec::new();
                let mut stdout = stdout;
                stdout.read_to_end(&mut buf)?;
                Ok(buf)
            });

            let wait_result = wait_with_timeout(&mut child, per_test_timeout, None, None, None)
                .map_err(|e| {
                    crate::error::Error::CommandExecution(format!(
                        "Failed to wait for test {}: {}",
                        test_id, e
                    ))
                })?;

            drop(_temp_file);

            let stdout_bytes = stdout_thread
                .join()
                .map_err(|_| {
                    crate::error::Error::CommandExecution(
                        "stdout reader thread panicked".to_string(),
                    )
                })?
                .map_err(crate::error::Error::Io)?;

            if let Err(reason) = wait_result {
                let elapsed = test_start.elapsed();
                let msg = match reason {
                    TimeoutReason::Timeout | TimeoutReason::TestTimeout(_) => {
                        format!("test timed out after {:.1}s", elapsed.as_secs_f64())
                    }
                    TimeoutReason::NoOutput => {
                        format!("test killed: no output for {:?}", per_test_timeout.unwrap())
                    }
                };
                tracing::warn!(
                    "test {} killed after {:.1}s ({})",
                    test_id,
                    elapsed.as_secs_f64(),
                    msg
                );
                all_results.insert(
                    test_id.clone(),
                    TestResult::error(test_id.clone(), msg).with_duration(elapsed),
                );
                any_failed = true;
                continue;
            }

            if wait_result.is_ok_and(|s| !s.success()) {
                any_failed = true;
            }

            let test_run_id = format!("{}-{}", run_id, idx);
            let test_run = subunit_stream::parse_stream(stdout_bytes.as_slice(), test_run_id)?;

            for (test_id, result) in test_run.results {
                all_results.insert(test_id, result);
            }
        }

        Ok(RunOutput {
            run_id,
            results: all_results,
            any_command_failed: any_failed,
            duration: start_time.elapsed(),
            test_command: test_cmd.config().test_command.clone(),
            concurrency: 1,
        })
    }
}

/// Resolve timeout settings from explicit values and config file defaults.
pub fn resolve_timeouts(
    explicit_test_timeout: &TimeoutSetting,
    explicit_max_duration: &TimeoutSetting,
    explicit_no_output_timeout: Option<Duration>,
    test_cmd: &TestCommand,
) -> Result<(TimeoutSetting, TimeoutSetting, Option<Duration>)> {
    let test_timeout = if *explicit_test_timeout != TimeoutSetting::Disabled {
        explicit_test_timeout.clone()
    } else {
        test_cmd.config().parsed_test_timeout()?
    };
    let max_duration = if *explicit_max_duration != TimeoutSetting::Disabled {
        explicit_max_duration.clone()
    } else {
        test_cmd.config().parsed_max_duration()?
    };
    let no_output_timeout =
        explicit_no_output_timeout.or(test_cmd.config().parsed_no_output_timeout()?);

    if test_timeout != TimeoutSetting::Disabled {
        tracing::info!("per-test timeout: {:?}", test_timeout);
    }
    if max_duration != TimeoutSetting::Disabled {
        tracing::info!("max run duration: {:?}", max_duration);
    }
    if let Some(t) = no_output_timeout {
        tracing::info!("no-output timeout: {:?}", t);
    }

    Ok((test_timeout, max_duration, no_output_timeout))
}

/// Compute effective max_duration value from the setting and historical test times.
pub fn compute_max_duration(
    max_duration: &TimeoutSetting,
    historical_times: &HashMap<TestId, Duration>,
) -> Option<Duration> {
    match max_duration {
        TimeoutSetting::Disabled => None,
        TimeoutSetting::Fixed(d) => Some(*d),
        TimeoutSetting::Auto => {
            let total: Duration = historical_times.values().sum();
            if total.is_zero() {
                None
            } else {
                Some(
                    Duration::from_secs_f64(total.as_secs_f64() * AUTO_TIMEOUT_MULTIPLIER)
                        .max(AUTO_MAX_DURATION_MINIMUM),
                )
            }
        }
    }
}

/// Build a per-test timeout lookup closure from the setting and historical times.
pub fn build_test_timeout_fn(
    test_timeout: &TimeoutSetting,
    historical_times: &HashMap<TestId, Duration>,
) -> Option<TestTimeoutFn> {
    if *test_timeout == TimeoutSetting::Disabled {
        return None;
    }
    let tt = test_timeout.clone();
    let ht = historical_times.clone();
    Some(Arc::new(move |test_id: &str| {
        tt.effective_timeout(ht.get(&TestId::new(test_id)).copied())
    }))
}

/// Handles for the background I/O threads spawned for each test process.
struct IoThreads {
    tee: std::thread::JoinHandle<std::result::Result<(), std::io::Error>>,
    stderr: std::thread::JoinHandle<std::result::Result<(), std::io::Error>>,
}

impl IoThreads {
    /// Spawn tee (stdout capture) and stderr forwarding threads for a child process.
    fn spawn(
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
        raw_writer: Box<dyn std::io::Write + Send>,
        tx: std::sync::mpsc::SyncSender<Vec<u8>>,
        activity_tracker: Option<&crate::test_runner::ActivityTracker>,
        progress_bar: ProgressBar,
    ) -> Self {
        let tee = if let Some(tracker) = activity_tracker {
            crate::test_runner::spawn_stdout_tee_tracked(stdout, raw_writer, tx, tracker.clone())
        } else {
            crate::test_runner::spawn_stdout_tee(stdout, raw_writer, tx)
        };
        let stderr = crate::test_runner::spawn_stderr_forwarder(stderr, progress_bar);
        IoThreads { tee, stderr }
    }

    /// Join both threads, converting panics and I/O errors into our error type.
    fn join(self, context: &str) -> Result<()> {
        self.tee
            .join()
            .map_err(|_| {
                crate::error::Error::CommandExecution(format!("Tee thread {} panicked", context))
            })?
            .map_err(crate::error::Error::Io)?;
        self.stderr
            .join()
            .map_err(|_| {
                crate::error::Error::CommandExecution(format!("Stderr thread {} panicked", context))
            })?
            .map_err(crate::error::Error::Io)?;
        Ok(())
    }
}

/// Update watchdog state when a subunit progress event is received.
fn update_watchdog(
    watchdog: Option<&TestWatchdog>,
    test_timeout_fn: Option<&TestTimeoutFn>,
    test_id: &str,
    status: subunit_stream::ProgressStatus,
) {
    if let Some(wd) = watchdog {
        if matches!(status, subunit_stream::ProgressStatus::InProgress) {
            let timeout = test_timeout_fn.and_then(|f| f(test_id));
            wd.on_test_start(test_id, timeout);
        } else if !status.indicator().is_empty() {
            wd.on_test_complete(test_id);
        }
    }
}

/// Spawn a shell command in its own process group so the entire tree can be killed on timeout.
fn spawn_in_process_group(
    cmd_str: &str,
    working_dir: &Path,
) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(cmd_str)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Helper to truncate test name to fit in available space.
fn truncate_test_name(test_id: &str, max_len: usize, fail_msg_len: usize) -> String {
    let max_name = max_len.saturating_sub(2 + fail_msg_len);
    if test_id.len() > max_name {
        test_id[test_id.len().saturating_sub(max_name)..].to_string()
    } else {
        test_id.to_string()
    }
}

/// Helper to format failure message with color.
fn format_failure_msg(failures: usize, short_label: bool) -> String {
    if failures > 0 {
        let label = if short_label { "fail" } else { "failures" };
        console::style(format!(" [{}: {}]", label, failures))
            .red()
            .to_string()
    } else {
        String::new()
    }
}

/// Helper to write non-subunit bytes to stdout.
fn write_non_subunit_output(progress_bar: &ProgressBar, bytes: &[u8]) {
    use std::io::Write;
    progress_bar.suspend(|| {
        let _ = std::io::stdout().write_all(bytes);
        let _ = std::io::stdout().flush();
    });
}

/// Choose progress bar colors based on failure rate.
fn get_progress_bar_colors(failure_rate: f64) -> (&'static str, &'static str) {
    if failure_rate == 0.0 {
        ("green", "blue")
    } else if failure_rate < 0.1 {
        ("yellow", "blue")
    } else if failure_rate < 0.25 {
        ("yellow", "red")
    } else if failure_rate < 0.5 {
        ("red", "yellow")
    } else {
        ("red", "red")
    }
}

/// Format a duration as a human-readable string (e.g., "1m 23s", "45s", "2h 05m").
fn format_duration_short(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {:02}m", hours, mins)
    } else if secs >= 60 {
        let mins = secs / 60;
        let remaining_secs = secs % 60;
        format!("{}m {:02}s", mins, remaining_secs)
    } else {
        format!("{}s", secs)
    }
}

/// Format an ETA string based on historical test times.
fn format_eta(
    estimated_total: Duration,
    completed_duration: Duration,
    elapsed: Duration,
) -> String {
    if estimated_total.is_zero() || elapsed.is_zero() {
        return String::new();
    }

    let fraction_done = completed_duration.as_secs_f64() / estimated_total.as_secs_f64();
    if fraction_done <= 0.0 || fraction_done > 1.0 {
        return String::new();
    }

    let projected_total = elapsed.as_secs_f64() / fraction_done;
    let remaining = projected_total - elapsed.as_secs_f64();
    if remaining <= 0.0 {
        return String::new();
    }

    format!(
        " ETA: {}",
        format_duration_short(Duration::from_secs_f64(remaining))
    )
}

/// Update progress bar style based on current failure rate.
fn update_progress_bar_style(
    progress_bar: &ProgressBar,
    bar_width: usize,
    completed: u64,
    failures: usize,
) {
    let failure_rate = if completed > 0 {
        failures as f64 / completed as f64
    } else {
        0.0
    };

    let (filled_color, empty_color) = get_progress_bar_colors(failure_rate);

    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "[{{elapsed_precise}}] {{bar:{}.{}/{}}} {{pos}}/{{len}} {{msg}}",
                bar_width, filled_color, empty_color
            ))
            .unwrap()
            .progress_chars("█▓▒░  "),
    );
}

/// Computed progress bar layout dimensions for a terminal.
struct ProgressLayout {
    bar_width: usize,
    max_msg_len: usize,
}

/// Compute progress bar layout from terminal width.
fn compute_progress_layout(term_width: usize) -> ProgressLayout {
    let bar_width = term_width
        .saturating_sub(PROGRESS_FIXED_WIDTH + PROGRESS_PADDING)
        .clamp(20, 60);
    let max_msg_len = term_width
        .saturating_sub(bar_width + PROGRESS_FIXED_WIDTH)
        .max(30);
    ProgressLayout {
        bar_width,
        max_msg_len,
    }
}

/// Create a progress bar with the standard style.
fn create_progress_bar(total: u64, bar_width: usize) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!(
                "[{{elapsed_precise}}] {{bar:{}.cyan/blue}} {{pos}}/{{len}} {{msg}}",
                bar_width
            ))
            .unwrap()
            .progress_chars("█▓▒░  "),
    );
    pb
}

/// Create a timeout error result for a test that was killed.
fn timeout_error_result(test_id: TestId) -> TestResult {
    TestResult::error(test_id, "test timed out (killed after per-test timeout)")
}

/// Filter out tests that have already completed or timed out, returning the remaining tests.
fn compute_remaining_tests(
    all_test_ids: &[TestId],
    completed_from_watchdog: &HashSet<String>,
    completed_from_results: &HashSet<&str>,
    hung_test: &str,
) -> Vec<TestId> {
    all_test_ids
        .iter()
        .filter(|id| {
            !completed_from_watchdog.contains(id.as_str())
                && !completed_from_results.contains(id.as_str())
                && id.as_str() != hung_test
        })
        .cloned()
        .collect()
}

/// Update the progress bar message after a test completes.
struct ProgressTracker {
    failures: usize,
    completed_duration: Duration,
    bar_width: usize,
    max_msg_len: usize,
    estimated_total: Duration,
    start_time: std::time::Instant,
    short_fail_label: bool,
}

impl ProgressTracker {
    fn new(
        bar_width: usize,
        max_msg_len: usize,
        estimated_total: Duration,
        start_time: std::time::Instant,
        short_fail_label: bool,
    ) -> Self {
        ProgressTracker {
            failures: 0,
            completed_duration: Duration::ZERO,
            bar_width,
            max_msg_len,
            estimated_total,
            start_time,
            short_fail_label,
        }
    }

    fn on_test_complete(
        &mut self,
        progress_bar: &ProgressBar,
        test_id: &str,
        status: subunit_stream::ProgressStatus,
        historical_times: &HashMap<TestId, Duration>,
    ) {
        progress_bar.inc(1);

        if let Some(&dur) = historical_times.get(&TestId::new(test_id)) {
            self.completed_duration += dur;
        }
        if matches!(
            status,
            subunit_stream::ProgressStatus::Failed
                | subunit_stream::ProgressStatus::UnexpectedSuccess
        ) {
            self.failures += 1;
        }

        let completed = progress_bar.position();
        update_progress_bar_style(progress_bar, self.bar_width, completed, self.failures);

        let fail_msg = format_failure_msg(self.failures, self.short_fail_label);
        let eta_msg = format_eta(
            self.estimated_total,
            self.completed_duration,
            self.start_time.elapsed(),
        );
        let extra_len = if self.failures > 0 {
            let label = if self.short_fail_label {
                "fail"
            } else {
                "failures"
            };
            3 + label.len() + self.failures.to_string().len()
        } else {
            0
        } + eta_msg.len();
        let short_name = truncate_test_name(test_id, self.max_msg_len, extra_len);

        let indicator = status.indicator();
        progress_bar.set_message(format!(
            "{} {}{}{}",
            indicator, short_name, fail_msg, eta_msg
        ));
    }
}

/// Handles for a single parallel worker's background threads.
struct WorkerThreads {
    worker_id: usize,
    bar: ProgressBar,
    parse: std::thread::JoinHandle<Result<TestRun>>,
    io: IoThreads,
    watchdog: Option<TestWatchdog>,
}

type SupervisorResult = std::result::Result<
    std::result::Result<std::process::ExitStatus, TimeoutReason>,
    std::io::Error,
>;

/// Wait for all supervisor threads and collect their results.
fn join_supervisors(
    supervisors: Vec<(usize, std::thread::JoinHandle<SupervisorResult>)>,
) -> Result<HashMap<usize, std::result::Result<std::process::ExitStatus, TimeoutReason>>> {
    let mut results = HashMap::new();
    for (worker_id, supervisor) in supervisors {
        let result = supervisor
            .join()
            .map_err(|_| {
                crate::error::Error::CommandExecution(format!(
                    "Supervisor thread {} panicked",
                    worker_id
                ))
            })?
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to wait for worker {}: {}",
                    worker_id, e
                ))
            })?;
        results.insert(worker_id, result);
    }
    Ok(results)
}

/// Join parse and I/O threads for each worker, tag results, and merge into all_results.
fn collect_worker_results(
    worker_threads: Vec<WorkerThreads>,
    all_results: &mut HashMap<TestId, TestResult>,
) -> Result<HashMap<usize, Option<TestWatchdog>>> {
    let mut worker_watchdogs = HashMap::new();
    for wt in worker_threads {
        let mut worker_run = wt.parse.join().map_err(|_| {
            crate::error::Error::CommandExecution(format!("Parse thread {} panicked", wt.worker_id))
        })??;

        wt.io.join(&format!("worker-{}", wt.worker_id))?;
        wt.bar.finish_with_message("done");

        let worker_tag = format!("worker-{}", wt.worker_id);
        for (_, result) in worker_run.results.iter_mut() {
            if !result.tags.contains(&worker_tag) {
                result.tags.push(worker_tag.clone());
            }
        }

        for (test_id, result) in worker_run.results {
            all_results.insert(test_id, result);
        }

        worker_watchdogs.insert(wt.worker_id, wt.watchdog);
    }
    Ok(worker_watchdogs)
}

/// Examine supervisor results to determine which workers timed out and need restarting.
#[allow(clippy::too_many_arguments)]
fn compute_restart_partitions(
    supervisor_results: &HashMap<
        usize,
        std::result::Result<std::process::ExitStatus, TimeoutReason>,
    >,
    worker_watchdogs: &HashMap<usize, Option<TestWatchdog>>,
    pending_partitions: &[(usize, Vec<TestId>)],
    all_results: &mut HashMap<TestId, TestResult>,
    any_failed: &mut bool,
    start_time: std::time::Instant,
    no_output_timeout: Option<Duration>,
) -> Vec<(usize, Vec<TestId>)> {
    let mut restart_partitions = Vec::new();
    for (worker_id, result) in supervisor_results {
        match result {
            Err(TimeoutReason::TestTimeout(hung_test)) => {
                tracing::warn!("worker {} killed (test {} timed out)", worker_id, hung_test);
                let test_id = TestId::new(hung_test);
                all_results.insert(test_id.clone(), timeout_error_result(test_id));
                *any_failed = true;

                let completed_from_watchdog = worker_watchdogs
                    .get(worker_id)
                    .and_then(|wd| wd.as_ref())
                    .map(|wd| wd.completed_tests())
                    .unwrap_or_default();
                let completed_in_results: HashSet<&str> =
                    all_results.keys().map(|id| id.as_str()).collect();

                let original_partition: &[TestId] = &pending_partitions
                    .iter()
                    .find(|(wid, _)| wid == worker_id)
                    .expect("worker_id must exist in pending_partitions")
                    .1;

                let remaining = compute_remaining_tests(
                    original_partition,
                    &completed_from_watchdog,
                    &completed_in_results,
                    hung_test,
                );

                if !remaining.is_empty() {
                    restart_partitions.push((*worker_id, remaining));
                }
            }
            Err(TimeoutReason::Timeout) => {
                tracing::warn!(
                    "worker {} killed (max duration exceeded after {:.1}s)",
                    worker_id,
                    start_time.elapsed().as_secs_f64()
                );
                *any_failed = true;
            }
            Err(TimeoutReason::NoOutput) => {
                tracing::warn!(
                    "worker {} killed (no output for {:?})",
                    worker_id,
                    no_output_timeout.unwrap()
                );
                *any_failed = true;
            }
            Ok(status) if !status.success() => {
                *any_failed = true;
            }
            Ok(_) => {}
        }
    }
    restart_partitions
}

/// RAII guard to ensure test instances are disposed.
struct InstanceDisposeGuard<'a> {
    test_cmd: &'a TestCommand,
    instance_ids: &'a [String],
}

impl Drop for InstanceDisposeGuard<'_> {
    fn drop(&mut self) {
        let _ = self.test_cmd.dispose_instances(self.instance_ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{TestId, TestResult, TestStatus};

    #[test]
    fn test_run_output_exit_code_all_passing() {
        let mut results = HashMap::new();
        results.insert(TestId::new("test1"), TestResult::success("test1"));
        results.insert(TestId::new("test2"), TestResult::success("test2"));
        let output = RunOutput {
            run_id: "0".to_string(),
            results,
            any_command_failed: false,
            duration: Duration::from_secs(1),
            test_command: "echo".to_string(),
            concurrency: 1,
        };
        assert_eq!(output.exit_code(), 0);
    }

    #[test]
    fn test_run_output_exit_code_with_failure() {
        let mut results = HashMap::new();
        results.insert(TestId::new("test1"), TestResult::success("test1"));
        results.insert(
            TestId::new("test2"),
            TestResult::failure("test2", "assertion failed"),
        );
        let output = RunOutput {
            run_id: "0".to_string(),
            results,
            any_command_failed: false,
            duration: Duration::from_secs(1),
            test_command: "echo".to_string(),
            concurrency: 1,
        };
        assert_eq!(output.exit_code(), 1);
    }

    #[test]
    fn test_run_output_exit_code_command_failed() {
        let output = RunOutput {
            run_id: "0".to_string(),
            results: HashMap::new(),
            any_command_failed: true,
            duration: Duration::from_secs(1),
            test_command: "echo".to_string(),
            concurrency: 1,
        };
        assert_eq!(output.exit_code(), 1);
    }

    #[test]
    fn test_run_output_exit_code_with_error() {
        let mut results = HashMap::new();
        results.insert(
            TestId::new("test1"),
            TestResult::error("test1", "something broke"),
        );
        let output = RunOutput {
            run_id: "0".to_string(),
            results,
            any_command_failed: false,
            duration: Duration::from_secs(1),
            test_command: "echo".to_string(),
            concurrency: 1,
        };
        assert_eq!(output.exit_code(), 1);
    }

    #[test]
    fn test_run_output_into_test_run() {
        let mut results = HashMap::new();
        results.insert(TestId::new("test1"), TestResult::success("test1"));
        results.insert(TestId::new("test2"), TestResult::failure("test2", "failed"));
        let output = RunOutput {
            run_id: "42".to_string(),
            results,
            any_command_failed: false,
            duration: Duration::from_secs(5),
            test_command: "cargo test".to_string(),
            concurrency: 2,
        };
        let test_run = output.into_test_run();
        assert_eq!(test_run.id, "42");
        assert_eq!(test_run.total_tests(), 2);
        assert_eq!(test_run.count_successes(), 1);
        assert_eq!(test_run.count_failures(), 1);
        assert_eq!(
            test_run.results.get(&TestId::new("test2")).unwrap().status,
            TestStatus::Failure
        );
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn test_truncate_test_name_no_truncation_needed() {
        let name = "short_test";
        let result = truncate_test_name(name, 50, 0);
        assert_eq!(result, "short_test");
    }

    #[test]
    fn test_truncate_test_name_with_truncation() {
        let name = "very.long.test.module.name.TestClass.test_method_name";
        let result = truncate_test_name(name, 30, 0);
        assert_eq!(result.len(), 28);
        assert!(result.ends_with("test_method_name"));
    }

    #[test]
    fn test_truncate_test_name_with_fail_msg() {
        let name = "some.long.test.name.that.needs.truncating";
        let result = truncate_test_name(name, 30, 15);
        assert_eq!(result.len(), 13);
        assert!(result.ends_with("truncating"));
    }

    #[test]
    fn test_get_progress_bar_colors_all_passing() {
        let (filled, empty) = get_progress_bar_colors(0.0);
        assert_eq!(filled, "green");
        assert_eq!(empty, "blue");
    }

    #[test]
    fn test_get_progress_bar_colors_few_failures() {
        let (filled, empty) = get_progress_bar_colors(0.05);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "blue");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_10_percent() {
        let (filled, empty) = get_progress_bar_colors(0.09);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "blue");

        let (filled, empty) = get_progress_bar_colors(0.1);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_moderate_failures() {
        let (filled, empty) = get_progress_bar_colors(0.15);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_25_percent() {
        let (filled, empty) = get_progress_bar_colors(0.24);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");

        let (filled, empty) = get_progress_bar_colors(0.25);
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");
    }

    #[test]
    fn test_get_progress_bar_colors_many_failures() {
        let (filled, empty) = get_progress_bar_colors(0.4);
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_50_percent() {
        let (filled, empty) = get_progress_bar_colors(0.49);
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");

        let (filled, empty) = get_progress_bar_colors(0.5);
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_most_failures() {
        let (filled, empty) = get_progress_bar_colors(0.75);
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_all_failures() {
        let (filled, empty) = get_progress_bar_colors(1.0);
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_update_progress_bar_style_doesnt_panic() {
        let pb = ProgressBar::new(10);

        update_progress_bar_style(&pb, 50, 5, 0);
        update_progress_bar_style(&pb, 50, 5, 1);
        update_progress_bar_style(&pb, 50, 5, 3);
        update_progress_bar_style(&pb, 50, 5, 5);
        update_progress_bar_style(&pb, 50, 0, 0);
    }

    #[test]
    fn test_format_failure_msg_no_failures() {
        let msg = format_failure_msg(0, false);
        assert_eq!(msg, "");
    }

    #[test]
    fn test_format_failure_msg_with_failures_long() {
        let msg = format_failure_msg(5, false);
        assert!(msg.contains("[failures: 5]"));
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_format_failure_msg_with_failures_short() {
        let msg = format_failure_msg(3, true);
        assert!(msg.contains("[fail: 3]"));
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_truncate_edge_case_exact_fit() {
        let name = "exactly_twenty_chars";
        let result = truncate_test_name(name, 22, 0);
        assert_eq!(result, "exactly_twenty_chars");
    }

    #[test]
    fn test_truncate_edge_case_very_small_max() {
        let name = "some.long.test.name";
        let result = truncate_test_name(name, 5, 0);
        assert_eq!(result.len(), 3);
        assert_eq!(result, "ame");
    }

    #[test]
    fn test_format_duration_short_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn test_format_duration_short_minutes() {
        assert_eq!(format_duration_short(Duration::from_secs(90)), "1m 30s");
    }

    #[test]
    fn test_format_duration_short_hours() {
        assert_eq!(format_duration_short(Duration::from_secs(3661)), "1h 01m");
    }

    #[test]
    fn test_format_duration_short_zero() {
        assert_eq!(format_duration_short(Duration::ZERO), "0s");
    }

    #[test]
    fn test_format_eta_with_history() {
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(50),
            Duration::from_secs(60),
        );
        assert_eq!(eta, " ETA: 1m 00s");
    }

    #[test]
    fn test_format_eta_no_history() {
        let eta = format_eta(Duration::ZERO, Duration::ZERO, Duration::from_secs(10));
        assert_eq!(eta, "");
    }

    #[test]
    fn test_format_eta_no_elapsed() {
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(10),
            Duration::ZERO,
        );
        assert_eq!(eta, "");
    }

    #[test]
    fn test_format_eta_nearly_done() {
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(120),
            Duration::from_secs(90),
        );
        assert_eq!(eta, "");
    }
}
