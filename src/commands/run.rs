//! Run tests and load results into the repository

use crate::commands::utils::{init_repository, open_repository};
use crate::commands::Command;
use crate::error::Result;
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;

/// Helper to truncate test name to fit in available space
fn truncate_test_name(test_id: &str, max_len: usize, fail_msg_len: usize) -> String {
    let max_name = max_len.saturating_sub(2 + fail_msg_len); // 2 for indicator + space
    if test_id.len() > max_name {
        test_id[test_id.len().saturating_sub(max_name)..].to_string()
    } else {
        test_id.to_string()
    }
}

/// Helper to format failure message with color
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

/// Helper to write non-subunit bytes to stdout
fn write_non_subunit_output(progress_bar: &ProgressBar, bytes: &[u8]) {
    use std::io::Write;
    // Suspend the progress bar while writing output
    progress_bar.suspend(|| {
        let _ = std::io::stdout().write_all(bytes);
        let _ = std::io::stdout().flush();
    });
}

/// Choose progress bar colors based on failure rate
/// Returns (filled_color, empty_color) tuple
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

/// Update progress bar style based on current failure rate
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
        // Should show the end (most specific part)
        assert_eq!(result.len(), 28); // 30 - 2 for indicator and space
        assert!(result.ends_with("test_method_name"));
    }

    #[test]
    fn test_truncate_test_name_with_fail_msg() {
        let name = "some.long.test.name.that.needs.truncating";
        let result = truncate_test_name(name, 30, 15); // Reserve 15 chars for " [failures: 99]"
        assert_eq!(result.len(), 13); // 30 - 2 - 15 = 13
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
        let (filled, empty) = get_progress_bar_colors(0.05); // 5% failure
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "blue");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_10_percent() {
        // Just under 10%
        let (filled, empty) = get_progress_bar_colors(0.09);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "blue");

        // At 10%
        let (filled, empty) = get_progress_bar_colors(0.1);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_moderate_failures() {
        let (filled, empty) = get_progress_bar_colors(0.15); // 15% failure
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_25_percent() {
        // Just under 25%
        let (filled, empty) = get_progress_bar_colors(0.24);
        assert_eq!(filled, "yellow");
        assert_eq!(empty, "red");

        // At 25%
        let (filled, empty) = get_progress_bar_colors(0.25);
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");
    }

    #[test]
    fn test_get_progress_bar_colors_many_failures() {
        let (filled, empty) = get_progress_bar_colors(0.4); // 40% failure
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");
    }

    #[test]
    fn test_get_progress_bar_colors_boundary_50_percent() {
        // Just under 50%
        let (filled, empty) = get_progress_bar_colors(0.49);
        assert_eq!(filled, "red");
        assert_eq!(empty, "yellow");

        // At 50%
        let (filled, empty) = get_progress_bar_colors(0.5);
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_most_failures() {
        let (filled, empty) = get_progress_bar_colors(0.75); // 75% failure
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_get_progress_bar_colors_all_failures() {
        let (filled, empty) = get_progress_bar_colors(1.0); // 100% failure
        assert_eq!(filled, "red");
        assert_eq!(empty, "red");
    }

    #[test]
    fn test_update_progress_bar_style_doesnt_panic() {
        // Test that the function can be called without panicking
        // We can't easily test the visual output, but we can verify it executes
        let pb = ProgressBar::new(10);

        // Test with no failures (0%)
        update_progress_bar_style(&pb, 50, 5, 0);

        // Test with some failures (20%)
        update_progress_bar_style(&pb, 50, 5, 1);

        // Test with many failures (60%)
        update_progress_bar_style(&pb, 50, 5, 3);

        // Test with all failures (100%)
        update_progress_bar_style(&pb, 50, 5, 5);

        // Test with zero completed (edge case)
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
        // In tests, console::style may or may not add colors depending on the environment
        // Just verify the message contains the expected text
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_format_failure_msg_with_failures_short() {
        let msg = format_failure_msg(3, true);
        assert!(msg.contains("[fail: 3]"));
        // In tests, console::style may or may not add colors depending on the environment
        // Just verify the message contains the expected text
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_truncate_edge_case_exact_fit() {
        let name = "exactly_twenty_chars";
        let result = truncate_test_name(name, 22, 0); // 22 - 2 = 20
        assert_eq!(result, "exactly_twenty_chars");
    }

    #[test]
    fn test_truncate_edge_case_very_small_max() {
        let name = "some.long.test.name";
        let result = truncate_test_name(name, 5, 0);
        assert_eq!(result.len(), 3); // 5 - 2 = 3
        assert_eq!(result, "ame"); // Last 3 chars
    }
}

/// Command to run tests and load results into the repository.
///
/// Executes tests using the configured test command, displays progress,
/// and stores the results in the repository.
pub struct RunCommand {
    base_path: Option<String>,
    failing_only: bool,
    force_init: bool,
    partial: bool,
    load_list: Option<String>,
    concurrency: Option<usize>,
    until_failure: bool,
    isolated: bool,
    subunit: bool,
    all_output: bool,
    test_filters: Option<Vec<String>>,
    test_args: Option<Vec<String>>,
}

impl RunCommand {
    /// Creates a new run command with default settings.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
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
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
        }
    }

    /// Creates a run command that only runs previously failing tests.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
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
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
        }
    }

    /// Creates a run command that will initialize the repository if needed.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `failing_only` - Whether to only run previously failing tests
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
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
        }
    }

    /// Creates a run command with control over partial loading mode.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `partial` - If true, add/update failing tests without clearing previous failures
    /// * `failing_only` - Whether to only run previously failing tests
    /// * `force_init` - If true, initialize the repository if it doesn't exist
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
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
        }
    }

    /// Creates a run command with full control over all options.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `partial` - If true, add/update failing tests without clearing previous failures
    /// * `failing_only` - Whether to only run previously failing tests
    /// * `force_init` - If true, initialize the repository if it doesn't exist
    /// * `load_list` - Optional path to a file containing test IDs to run
    /// * `concurrency` - Optional number of parallel test workers
    /// * `until_failure` - If true, stop running tests after the first failure
    /// * `isolated` - If true, run each test in isolation
    /// * `subunit` - If true, output in subunit format instead of showing progress
    /// * `all_output` - If true, show all test output instead of just failures
    /// * `test_filters` - Optional list of test patterns to filter
    /// * `test_args` - Optional additional arguments to pass to the test command
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
        subunit: bool,
        all_output: bool,
        test_filters: Option<Vec<String>>,
        test_args: Option<Vec<String>>,
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
            subunit,
            all_output,
            test_filters,
            test_args,
        }
    }

    /// Run tests and output raw subunit stream (no progress bars)
    fn run_subunit(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
        _run_id: String,
    ) -> Result<i32> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // Build command with test IDs if provided
        let (cmd_str, _temp_file) =
            test_cmd.build_command_full(test_ids, false, None, self.test_args.as_deref())?;

        // Begin the test run and get a writer for streaming raw bytes
        let (run_id, raw_writer) = repo.begin_test_run_raw()?;

        // Spawn test command with piped stdout
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test command: {}",
                    e
                ))
            })?;

        let mut stdout = child.stdout.take().expect("stdout was piped");

        // Create a tee writer that writes to both file and UI
        struct TeeWriter<W1: Write, W2: Write> {
            writer1: W1,
            writer2: W2,
        }

        impl<W1: Write, W2: Write> Write for TeeWriter<W1, W2> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.writer1.write_all(buf)?;
                self.writer2.write_all(buf)?;
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.writer1.flush()?;
                self.writer2.flush()?;
                Ok(())
            }
        }

        // Create a writer that outputs to UI
        struct UIWriter<'a> {
            ui: &'a mut dyn UI,
        }

        impl<'a> Write for UIWriter<'a> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.ui.output_bytes(buf).map_err(std::io::Error::other)?;
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // Stream to both repository file and UI output
        let mut tee = TeeWriter {
            writer1: raw_writer,
            writer2: UIWriter { ui },
        };

        std::io::copy(&mut stdout, &mut tee).map_err(crate::error::Error::Io)?;
        tee.flush().map_err(crate::error::Error::Io)?;

        // Wait for process to complete
        let status = child.wait().map_err(|e| {
            crate::error::Error::CommandExecution(format!("Failed to wait for test command: {}", e))
        })?;

        // Parse the stored stream to update failing tests
        let test_run = repo.get_test_run(&run_id)?;

        crate::commands::utils::update_repository_failing_tests(repo, &test_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &test_run)?;

        // Return exit code based on test command exit code
        if status.success() {
            Ok(0)
        } else {
            Ok(1)
        }
    }

    /// Run tests serially (single process)
    fn run_serial(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
    ) -> Result<i32> {
        use std::process::{Command, Stdio};

        // Get test count for progress bar
        let test_count = if let Some(ids) = test_ids {
            ids.len()
        } else {
            test_cmd.list_tests()?.len()
        };

        // Build command with test IDs if provided
        let (cmd_str, _temp_file) =
            test_cmd.build_command_full(test_ids, false, None, self.test_args.as_deref())?;

        // Begin the test run and get a writer for streaming raw bytes
        let (run_id, raw_writer) = repo.begin_test_run_raw()?;

        // Create progress bar with dynamic width
        let term_width = console::Term::stdout().size().1 as usize;
        // Template: "[HH:MM:SS] [bar] pos/len msg"
        // Fixed elements: "[HH:MM:SS] " (11) + " " (1) + " " (1) + "9999/9999" (9) + " " (1) = ~23 chars
        let fixed_width = 25; // Add a bit of margin
        let bar_width = term_width.saturating_sub(fixed_width + 30).clamp(20, 60); // Bar between 20-60 chars
                                                                                   // Calculate max message length
        let max_msg_len = term_width.saturating_sub(bar_width + fixed_width).max(30);

        let progress_bar = ProgressBar::new(test_count as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(&format!(
                    "[{{elapsed_precise}}] {{bar:{}.cyan/blue}} {{pos}}/{{len}} {{msg}}",
                    bar_width
                ))
                .unwrap()
                .progress_chars("█▓▒░  "),
        );

        // Spawn test command with both stdout and stderr piped
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(Path::new(self.base_path.as_deref().unwrap_or(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test command: {}",
                    e
                ))
            })?;

        // Take stdout and stderr for streaming
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Tee the stream: capture raw bytes for storage AND parse for progress display
        let (tx, rx) = std::sync::mpsc::sync_channel(100);

        // Thread for stdout
        let tee_thread = crate::test_runner::spawn_stdout_tee(stdout, raw_writer, tx);

        // Thread for stderr - write directly to stderr (not to parser or storage)
        let stderr_thread =
            crate::test_runner::spawn_stderr_forwarder(stderr, progress_bar.clone());

        // Parse stdout stream in a thread for real-time progress
        let progress_bar_clone = progress_bar.clone();
        let run_id_clone = run_id.clone();

        // Create a reader from the channel
        let channel_reader = crate::test_runner::ChannelReader::new(rx);

        let output_filter = if self.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        let parse_thread = std::thread::spawn(move || {
            let mut failures = 0;
            let progress_bar_for_bytes = progress_bar_clone.clone();
            let progress_bar_for_style = progress_bar_clone.clone();

            let result = subunit_stream::parse_stream_with_progress(
                channel_reader,
                run_id_clone,
                |test_id, status| {
                    let indicator = status.indicator();
                    if !indicator.is_empty() {
                        progress_bar_clone.inc(1);

                        // Track failures
                        if matches!(
                            status,
                            subunit_stream::ProgressStatus::Failed
                                | subunit_stream::ProgressStatus::UnexpectedSuccess
                        ) {
                            failures += 1;
                        }

                        // Update progress bar color based on failure rate
                        let completed = progress_bar_clone.position();
                        update_progress_bar_style(
                            &progress_bar_for_style,
                            bar_width,
                            completed,
                            failures,
                        );

                        let fail_msg = format_failure_msg(failures, false);
                        let fail_len = if failures > 0 {
                            12 + failures.to_string().len()
                        } else {
                            0
                        };
                        let short_name = truncate_test_name(test_id, max_msg_len, fail_len);

                        progress_bar_clone
                            .set_message(format!("{} {}{}", indicator, short_name, fail_msg));
                    }
                },
                |bytes| {
                    write_non_subunit_output(&progress_bar_for_bytes, bytes);
                },
                output_filter,
            );
            result
        });

        // Wait for process to complete
        let status = child.wait().map_err(|e| {
            progress_bar.finish_and_clear();
            crate::error::Error::CommandExecution(format!("Failed to wait for test command: {}", e))
        })?;

        let command_failed = !status.success();

        // Get results from parse thread
        let test_run = parse_thread.join().map_err(|_| {
            progress_bar.finish_and_clear();
            crate::error::Error::CommandExecution("Parse thread panicked".to_string())
        })??;

        // Wait for tee threads to finish writing raw bytes
        tee_thread
            .join()
            .map_err(|_| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution("Tee thread panicked".to_string())
            })?
            .map_err(|e| {
                progress_bar.finish_and_clear();
                crate::error::Error::Io(e)
            })?;

        stderr_thread
            .join()
            .map_err(|_| {
                progress_bar.finish_and_clear();
                crate::error::Error::CommandExecution("Stderr thread panicked".to_string())
            })?
            .map_err(|e| {
                progress_bar.finish_and_clear();
                crate::error::Error::Io(e)
            })?;

        progress_bar.finish_and_clear();

        // Update failing tests and test times
        crate::commands::utils::update_repository_failing_tests(repo, &test_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &test_run)?;

        // Display summary
        crate::commands::utils::display_test_summary(ui, &run_id, &test_run)?;

        // Return exit code based on results
        if test_run.count_failures() > 0 || command_failed {
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
        concurrency: usize,
    ) -> Result<i32> {
        use std::collections::HashMap;
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let output_filter = if self.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        // Get the base run ID - each worker will write to run_id-{worker_id}
        let base_run_id = repo.get_next_run_id()?;

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
        let term_width = console::Term::stdout().size().1 as usize;
        let fixed_width = 25; // For "[HH:MM:SS] " + " pos/len "
        let overall_bar_width = term_width.saturating_sub(fixed_width + 30).clamp(20, 60);

        let multi_progress = indicatif::MultiProgress::new();
        let overall_bar = multi_progress.add(ProgressBar::new(all_tests.len() as u64));
        overall_bar.set_style(
            ProgressStyle::default_bar()
                .template(&format!(
                    "[{{elapsed_precise}}] {{bar:{}.cyan/blue}} {{pos}}/{{len}} {{msg}}",
                    overall_bar_width
                ))
                .unwrap()
                .progress_chars("█▓▒░  "),
        );

        // Shared failure counter across all workers
        let total_failures = Arc::new(AtomicUsize::new(0));

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
            // Template: "Worker N: [bar] pos/len msg"
            // Fixed: "Worker N: " (10-12) + " " + "999/999" (7) + " " = ~20 chars
            let worker_fixed = 22;
            let worker_bar_width =
                ((term_width.saturating_sub(worker_fixed + 30)) / concurrency.min(4)).clamp(15, 40);
            // Calculate max message for worker
            let worker_max_msg = term_width
                .saturating_sub(worker_bar_width + worker_fixed)
                .max(20);

            let worker_bar = multi_progress.add(ProgressBar::new(partition.len() as u64));
            worker_bar.set_style(
                ProgressStyle::default_bar()
                    .template(&format!(
                        "Worker {}: [{{bar:{}.green/blue}}] {{pos}}/{{len}} {{msg}}",
                        worker_id, worker_bar_width
                    ))
                    .unwrap()
                    .progress_chars("█▓▒░  "),
            );

            // Build command for this partition with instance ID
            let instance_id = instance_ids.get(worker_id).map(|s| s.as_str());
            let (cmd_str, _temp_file) = test_cmd.build_command_full(
                Some(partition),
                false,
                instance_id,
                self.test_args.as_deref(),
            )?;

            // Spawn the worker process with both stdout and stderr piped
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

            // Take stdout and stderr for streaming
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");

            // Get a writer for this worker's raw output
            let worker_run_id = format!("{}-{}", base_run_id, worker_id);
            let (_, raw_writer) = repo.begin_test_run_raw()?;

            // Tee the stream: capture raw bytes for storage AND parse for progress display
            let (tx, rx) = std::sync::mpsc::sync_channel(100);

            // Thread for stdout
            let tee_thread = crate::test_runner::spawn_stdout_tee(stdout, raw_writer, tx);

            // Thread for stderr - write directly to stderr (not to parser or storage)
            let stderr_thread =
                crate::test_runner::spawn_stderr_forwarder(stderr, worker_bar.clone());

            // Create a reader from the channel
            let channel_reader = crate::test_runner::ChannelReader::new(rx);

            // Spawn thread to parse output in real-time
            let worker_bar_clone = worker_bar.clone();
            let overall_bar_clone = overall_bar.clone();
            let worker_run_id_clone = worker_run_id.clone();
            let total_failures_clone = Arc::clone(&total_failures);

            let output_filter_clone = output_filter;
            let parse_thread = std::thread::spawn(move || {
                let mut failures = 0;
                let worker_bar_for_bytes = worker_bar_clone.clone();
                // Parse stdout stream for real-time progress
                subunit_stream::parse_stream_with_progress(
                    channel_reader,
                    worker_run_id_clone,
                    |test_id, status| {
                        let indicator = status.indicator();
                        if !indicator.is_empty() {
                            worker_bar_clone.inc(1);
                            overall_bar_clone.inc(1);

                            // Track failures
                            if matches!(
                                status,
                                subunit_stream::ProgressStatus::Failed
                                    | subunit_stream::ProgressStatus::UnexpectedSuccess
                            ) {
                                failures += 1;
                                let total =
                                    total_failures_clone.fetch_add(1, Ordering::Relaxed) + 1;

                                // Update progress bar color based on failure rate
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

                            let fail_msg = format_failure_msg(failures, true);
                            let fail_len = if failures > 0 {
                                9 + failures.to_string().len()
                            } else {
                                0
                            };
                            let short_name = truncate_test_name(test_id, worker_max_msg, fail_len);

                            worker_bar_clone
                                .set_message(format!("{} {}{}", indicator, short_name, fail_msg));
                        }
                    },
                    |bytes| {
                        write_non_subunit_output(&worker_bar_for_bytes, bytes);
                    },
                    output_filter_clone,
                )
            });

            parse_threads.push((
                worker_id,
                worker_bar,
                parse_thread,
                tee_thread,
                stderr_thread,
            ));
            workers.push((worker_id, child, _temp_file));
        }

        // Collect results from parse threads and wait for workers to complete
        // IMPORTANT: We must collect from parse threads FIRST (while workers are still running)
        // to avoid deadlock. If we wait for workers first, the pipe buffer can fill up and
        // the worker process will block trying to write, while we're blocked waiting for it to finish.
        let mut all_results = HashMap::new();
        let mut any_failed = false;

        // First, collect results from ALL parse threads (this will also consume stdout, preventing deadlock)
        for (worker_id, worker_bar, parse_thread, tee_thread, stderr_thread) in parse_threads {
            let worker_run = parse_thread.join().map_err(|_| {
                crate::error::Error::CommandExecution(format!(
                    "Parse thread {} panicked",
                    worker_id
                ))
            })??;

            // Wait for tee threads to finish writing raw bytes
            tee_thread
                .join()
                .map_err(|_| {
                    crate::error::Error::CommandExecution(format!(
                        "Tee thread {} panicked",
                        worker_id
                    ))
                })?
                .map_err(crate::error::Error::Io)?;

            stderr_thread
                .join()
                .map_err(|_| {
                    crate::error::Error::CommandExecution(format!(
                        "Stderr thread {} panicked",
                        worker_id
                    ))
                })?
                .map_err(crate::error::Error::Io)?;

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

        // Now wait for all worker processes to complete
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

        // Finish progress bars
        overall_bar.finish_and_clear();

        // Create combined test run
        let run_id_for_display = base_run_id.to_string();
        let mut combined_run = crate::repository::TestRun::new(run_id_for_display.clone());
        combined_run.timestamp = chrono::Utc::now();

        for (_, result) in all_results {
            combined_run.add_result(result);
        }

        // Update failing tests and test times
        crate::commands::utils::update_repository_failing_tests(repo, &combined_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &combined_run)?;

        // Dispose instances (done explicitly before drop to handle errors)
        drop(dispose_guard);
        test_cmd.dispose_instances(&instance_ids)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output("Disposed instances")?;
        }

        // Display summary
        crate::commands::utils::display_test_summary(ui, &run_id_for_display, &combined_run)?;

        // Return exit code based on results
        if combined_run.count_failures() > 0 || any_failed {
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
    ) -> Result<i32> {
        use std::collections::HashMap;
        use std::process::{Command, Stdio};

        // Get the base run ID - each isolated test will write to its own file
        let base_run_id = repo.get_next_run_id()?;

        ui.output(&format!(
            "Running {} tests in isolated mode (one test per process)",
            test_ids.len()
        ))?;

        let mut all_results = HashMap::new();
        let mut any_failed = false;

        for (idx, test_id) in test_ids.iter().enumerate() {
            ui.output(&format!("  [{}/{}] {}", idx + 1, test_ids.len(), test_id))?;

            // Build command for this single test
            let (cmd_str, _temp_file) = test_cmd.build_command_full(
                Some(std::slice::from_ref(test_id)),
                false,
                None,
                self.test_args.as_deref(),
            )?;

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
            let test_run_id = format!("{}-{}", base_run_id, idx);
            let test_run = subunit_stream::parse_stream(output.stdout.as_slice(), test_run_id)?;

            // Collect results
            for (test_id, result) in test_run.results {
                all_results.insert(test_id, result);
            }
        }

        // Create combined test run
        let run_id_for_display = base_run_id.to_string();
        let mut combined_run = crate::repository::TestRun::new(run_id_for_display.clone());
        combined_run.timestamp = chrono::Utc::now();

        for (_, result) in all_results {
            combined_run.add_result(result);
        }

        // Update failing tests and test times
        crate::commands::utils::update_repository_failing_tests(repo, &combined_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &combined_run)?;

        // Display summary
        crate::commands::utils::display_test_summary(ui, &run_id_for_display, &combined_run)?;

        // Return exit code based on results
        if combined_run.count_failures() > 0 || any_failed {
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

        // Apply test_filters if provided
        if let Some(ref filters) = self.test_filters {
            use regex::Regex;

            // Compile all filter patterns
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

            // If we don't have test_ids yet, we need to list all tests first
            let all_test_ids = if let Some(ids) = test_ids {
                ids
            } else {
                test_cmd.list_tests()?
            };

            // Filter test IDs using the patterns (union of all matches)
            let filtered_ids: Vec<_> = all_test_ids
                .into_iter()
                .filter(|test_id| {
                    // Include test if ANY filter matches (using search, not match)
                    compiled_filters
                        .iter()
                        .any(|re| re.is_match(test_id.as_str()))
                })
                .collect();

            test_ids = Some(filtered_ids);
        }

        // If subunit mode is requested, run and output raw subunit stream
        if self.subunit {
            let run_id = repo.get_next_run_id()?.to_string();
            return self.run_subunit(ui, &mut repo, &test_cmd, test_ids.as_deref(), run_id);
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
                    let exit_code = self.run_isolated(ui, &mut repo, &test_cmd, &all_tests)?;

                    if exit_code != 0 {
                        ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                        return Ok(exit_code);
                    }

                    iteration += 1;
                }
            } else {
                self.run_isolated(ui, &mut repo, &test_cmd, &all_tests)
            }
        } else if self.until_failure {
            // Run tests in a loop until failure (non-isolated)
            let mut iteration = 1;
            loop {
                ui.output(&format!("\n=== Iteration {} ===", iteration))?;

                let exit_code = if concurrency > 1 {
                    self.run_parallel(ui, &mut repo, &test_cmd, test_ids.as_deref(), concurrency)?
                } else {
                    self.run_serial(ui, &mut repo, &test_cmd, test_ids.as_deref())?
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
            if concurrency > 1 {
                // Parallel execution
                self.run_parallel(ui, &mut repo, &test_cmd, test_ids.as_deref(), concurrency)
            } else {
                // Serial execution
                self.run_serial(ui, &mut repo, &test_cmd, test_ids.as_deref())
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
