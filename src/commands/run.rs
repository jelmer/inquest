//! Run tests and load results into the repository

use crate::commands::utils::open_or_init_repository;
use crate::commands::Command;
use crate::config::{
    TimeoutSetting, AUTO_MAX_DURATION_MINIMUM, AUTO_TIMEOUT_MULTIPLIER, MAX_TEST_TIMEOUT_RESTARTS,
};
use crate::error::Result;
use crate::repository::TestId;
use crate::subunit_stream;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use crate::watchdog::{wait_with_timeout, TestWatchdog, TimeoutReason};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Type alias for the per-test timeout lookup function.
type TestTimeoutFn = Arc<dyn Fn(&str) -> Option<Duration> + Send + Sync>;

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

/// Format a duration as a human-readable string (e.g., "1m 23s", "45s", "2h 05m")
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
///
/// Uses elapsed wall-clock time and the fraction of estimated total duration
/// that has been completed to project the remaining time.
fn format_eta(
    estimated_total: Duration,
    completed_duration: Duration,
    elapsed: Duration,
) -> String {
    if estimated_total.is_zero() || elapsed.is_zero() {
        return String::new();
    }

    // What fraction of the estimated work is done?
    let fraction_done = completed_duration.as_secs_f64() / estimated_total.as_secs_f64();
    if fraction_done <= 0.0 || fraction_done > 1.0 {
        return String::new();
    }

    // Project total wall-clock time from fraction done and elapsed time
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
        // Estimated total: 100s, completed 50s worth, elapsed 60s wall-clock
        // fraction_done = 0.5, projected_total = 120s, remaining = 60s
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(50),
            Duration::from_secs(60),
        );
        assert_eq!(eta, " ETA: 1m 00s");
    }

    #[test]
    fn test_format_eta_no_history() {
        // Zero estimated total means no ETA
        let eta = format_eta(Duration::ZERO, Duration::ZERO, Duration::from_secs(10));
        assert_eq!(eta, "");
    }

    #[test]
    fn test_format_eta_no_elapsed() {
        // Zero elapsed means no ETA
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(10),
            Duration::ZERO,
        );
        assert_eq!(eta, "");
    }

    #[test]
    fn test_format_eta_nearly_done() {
        // Completed more than estimated (tests ran faster than historical)
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(120),
            Duration::from_secs(90),
        );
        // fraction_done > 1.0, should return empty
        assert_eq!(eta, "");
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
    auto: bool,
    load_list: Option<String>,
    concurrency: Option<usize>,
    until_failure: bool,
    isolated: bool,
    subunit: bool,
    all_output: bool,
    test_filters: Option<Vec<String>>,
    test_args: Option<Vec<String>>,
    test_timeout: TimeoutSetting,
    max_duration: TimeoutSetting,
    no_output_timeout: Option<Duration>,
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
            auto: false,
            load_list: None,
            concurrency: None,
            until_failure: false,
            isolated: false,
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
            test_timeout: TimeoutSetting::Disabled,
            max_duration: TimeoutSetting::Disabled,
            no_output_timeout: None,
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
            auto: false,
            load_list: None,
            concurrency: None,
            until_failure: false,
            isolated: false,
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
            test_timeout: TimeoutSetting::Disabled,
            max_duration: TimeoutSetting::Disabled,
            no_output_timeout: None,
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
            auto: false,
            load_list: None,
            concurrency: None,
            until_failure: false,
            isolated: false,
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
            test_timeout: TimeoutSetting::Disabled,
            max_duration: TimeoutSetting::Disabled,
            no_output_timeout: None,
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
            auto: false,
            load_list: None,
            concurrency: None,
            until_failure: false,
            isolated: false,
            subunit: false,
            all_output: false,
            test_filters: None,
            test_args: None,
            test_timeout: TimeoutSetting::Disabled,
            max_duration: TimeoutSetting::Disabled,
            no_output_timeout: None,
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
    /// * `test_timeout` - Per-test timeout setting
    /// * `max_duration` - Overall run timeout setting
    /// * `no_output_timeout` - Kill test if no output for this duration
    #[allow(clippy::too_many_arguments)]
    pub fn with_all_options(
        base_path: Option<String>,
        partial: bool,
        failing_only: bool,
        force_init: bool,
        auto: bool,
        load_list: Option<String>,
        concurrency: Option<usize>,
        until_failure: bool,
        isolated: bool,
        subunit: bool,
        all_output: bool,
        test_filters: Option<Vec<String>>,
        test_args: Option<Vec<String>>,
        test_timeout: TimeoutSetting,
        max_duration: TimeoutSetting,
        no_output_timeout: Option<Duration>,
    ) -> Self {
        RunCommand {
            base_path,
            failing_only,
            force_init,
            partial,
            auto,
            load_list,
            concurrency,
            until_failure,
            isolated,
            subunit,
            all_output,
            test_filters,
            test_args,
            test_timeout,
            max_duration,
            no_output_timeout,
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

        // Build command with test IDs if provided
        // IMPORTANT: Keep _temp_file alive until after child process completes
        let (cmd_str, _temp_file) =
            test_cmd.build_command_full(test_ids, false, None, self.test_args.as_deref())?;

        // Begin the test run and get a writer for streaming raw bytes
        let (run_id, raw_writer) = repo.begin_test_run_raw()?;

        // Spawn test command with piped stdout
        let mut child = spawn_in_process_group(
            &cmd_str,
            Path::new(self.base_path.as_deref().unwrap_or(".")),
        )
        .map_err(|e| {
            crate::error::Error::CommandExecution(format!("Failed to execute test command: {}", e))
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

        let start_time = std::time::Instant::now();

        std::io::copy(&mut stdout, &mut tee).map_err(crate::error::Error::Io)?;
        tee.flush().map_err(crate::error::Error::Io)?;

        // Wait for process to complete
        let status = child.wait().map_err(|e| {
            crate::error::Error::CommandExecution(format!("Failed to wait for test command: {}", e))
        })?;

        let duration = start_time.elapsed();
        let exit_code = if status.success() { 0 } else { 1 };

        // Explicitly drop temp file now that child process has completed
        drop(_temp_file);

        // Parse the stored stream to update failing tests
        let test_run = repo.get_test_run(&run_id)?;

        crate::commands::utils::update_repository_failing_tests(repo, &test_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &test_run)?;
        crate::commands::utils::store_run_metadata(
            repo,
            &run_id,
            Some(&test_cmd.config().test_command),
            None,
            Some(duration),
            Some(exit_code),
        )?;

        // Return exit code based on test command exit code
        Ok(exit_code)
    }

    /// Run tests serially (single process), with per-test timeout and restart.
    #[allow(clippy::too_many_arguments)]
    fn run_serial(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
        max_duration: Option<Duration>,
        no_output_timeout: Option<Duration>,
        test_timeout_fn: Option<&TestTimeoutFn>,
    ) -> Result<i32> {
        let historical_times: HashMap<TestId, Duration> = if let Some(ids) = test_ids {
            repo.get_test_times_for_ids(ids).unwrap_or_default()
        } else {
            repo.get_test_times().unwrap_or_default()
        };
        let estimated_total: Duration = historical_times.values().sum();

        let mut remaining_tests: Option<Vec<TestId>> = test_ids.map(|ids| ids.to_vec());
        let mut all_results: HashMap<TestId, crate::repository::TestResult> = HashMap::new();
        let mut restarts = 0;
        let mut any_command_failed = false;

        let total_test_count = if let Some(ids) = test_ids {
            ids.len()
        } else {
            test_cmd.list_tests()?.len()
        };

        let start_time = std::time::Instant::now();
        let (run_id, _) = repo.begin_test_run_raw()?;

        let term_width = console::Term::stdout().size().1 as usize;
        let fixed_width = 25;
        let bar_width = term_width.saturating_sub(fixed_width + 30).clamp(20, 60);
        let max_msg_len = term_width.saturating_sub(bar_width + fixed_width).max(30);

        let progress_bar = ProgressBar::new(total_test_count as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(&format!(
                    "[{{elapsed_precise}}] {{bar:{}.cyan/blue}} {{pos}}/{{len}} {{msg}}",
                    bar_width
                ))
                .unwrap()
                .progress_chars("█▓▒░  "),
        );
        progress_bar.set_position(all_results.len() as u64);

        let output_filter = if self.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        loop {
            let current_ids = remaining_tests.as_deref();
            let (cmd_str, _temp_file) =
                test_cmd.build_command_full(current_ids, false, None, self.test_args.as_deref())?;
            let (_, raw_writer) = repo.begin_test_run_raw()?;

            let mut child = spawn_in_process_group(
                &cmd_str,
                Path::new(self.base_path.as_deref().unwrap_or(".")),
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

            let (tx, rx) = std::sync::mpsc::sync_channel(100);
            let activity_tracker =
                no_output_timeout.map(|_| crate::test_runner::ActivityTracker::new());
            let tee_thread = if let Some(ref tracker) = activity_tracker {
                crate::test_runner::spawn_stdout_tee_tracked(
                    stdout,
                    raw_writer,
                    tx,
                    tracker.clone(),
                )
            } else {
                crate::test_runner::spawn_stdout_tee(stdout, raw_writer, tx)
            };
            let stderr_thread =
                crate::test_runner::spawn_stderr_forwarder(stderr, progress_bar.clone());

            let watchdog = test_timeout_fn.as_ref().map(|_| TestWatchdog::new());
            let watchdog_for_thread = watchdog.clone();
            let test_timeout_fn_clone = test_timeout_fn.cloned();

            let progress_bar_clone = progress_bar.clone();
            let run_id_clone = run_id.clone();
            let channel_reader = crate::test_runner::ChannelReader::new(rx);
            let historical_times_for_thread = historical_times.clone();

            let parse_thread = std::thread::spawn(move || {
                let historical_times = historical_times_for_thread;
                let mut failures = 0;
                let mut completed_duration = Duration::ZERO;
                let progress_bar_for_bytes = progress_bar_clone.clone();
                let progress_bar_for_style = progress_bar_clone.clone();

                subunit_stream::parse_stream_with_progress(
                    channel_reader,
                    run_id_clone,
                    |test_id, status| {
                        if let Some(ref wd) = watchdog_for_thread {
                            if matches!(status, subunit_stream::ProgressStatus::InProgress) {
                                let timeout =
                                    test_timeout_fn_clone.as_ref().and_then(|f| f(test_id));
                                wd.on_test_start(test_id, timeout);
                            } else if !status.indicator().is_empty() {
                                wd.on_test_complete(test_id);
                            }
                        }

                        let indicator = status.indicator();
                        if !indicator.is_empty() {
                            progress_bar_clone.inc(1);

                            if let Some(&dur) = historical_times.get(&TestId::new(test_id)) {
                                completed_duration += dur;
                            }
                            if matches!(
                                status,
                                subunit_stream::ProgressStatus::Failed
                                    | subunit_stream::ProgressStatus::UnexpectedSuccess
                            ) {
                                failures += 1;
                            }

                            let completed = progress_bar_clone.position();
                            update_progress_bar_style(
                                &progress_bar_for_style,
                                bar_width,
                                completed,
                                failures,
                            );

                            let fail_msg = format_failure_msg(failures, false);
                            let eta_msg = format_eta(
                                estimated_total,
                                completed_duration,
                                start_time.elapsed(),
                            );
                            let extra_len = if failures > 0 {
                                12 + failures.to_string().len()
                            } else {
                                0
                            } + eta_msg.len();
                            let short_name = truncate_test_name(test_id, max_msg_len, extra_len);

                            progress_bar_clone.set_message(format!(
                                "{} {}{}{}",
                                indicator, short_name, fail_msg, eta_msg
                            ));
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
                    all_results.insert(
                        test_id.clone(),
                        crate::repository::TestResult::error(
                            test_id,
                            "test timed out (killed after per-test timeout)",
                        ),
                    );
                    any_command_failed = true;

                    let completed = watchdog
                        .as_ref()
                        .map(|wd| wd.completed_tests())
                        .unwrap_or_default();
                    let completed_in_results: std::collections::HashSet<&str> =
                        all_results.keys().map(|id| id.as_str()).collect();
                    let all_test_ids = if let Some(ref ids) = remaining_tests {
                        ids.clone()
                    } else {
                        // No explicit test list — discover tests now for restart
                        test_cmd.list_tests()?
                    };
                    let next_remaining: Vec<TestId> = all_test_ids
                        .iter()
                        .filter(|id| {
                            !completed.contains(id.as_str())
                                && !completed_in_results.contains(id.as_str())
                                && id.as_str() != hung_test
                        })
                        .cloned()
                        .collect();

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

                    tracing::info!("restarting with {} remaining tests", next_remaining.len());
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

        let mut combined_run = crate::repository::TestRun::new(run_id.clone());
        combined_run.timestamp = chrono::Utc::now();
        for (_, result) in all_results {
            combined_run.add_result(result);
        }

        let duration = start_time.elapsed();
        let exit_code = if combined_run.count_failures() > 0 || any_command_failed {
            1
        } else {
            0
        };

        crate::commands::utils::update_repository_failing_tests(repo, &combined_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &combined_run)?;
        crate::commands::utils::store_run_metadata(
            repo,
            &run_id,
            Some(&test_cmd.config().test_command),
            Some(1),
            Some(duration),
            Some(exit_code),
        )?;

        crate::commands::utils::display_test_summary(ui, &run_id, &combined_run)?;
        crate::commands::utils::warn_slow_tests(ui, &combined_run, &historical_times)?;

        Ok(exit_code)
    }

    /// Run tests in parallel across multiple workers, with per-test timeout and restart.
    #[allow(clippy::too_many_arguments)]
    fn run_parallel(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: Option<&[crate::repository::TestId]>,
        concurrency: usize,
        max_duration: Option<Duration>,
        no_output_timeout: Option<Duration>,
        test_timeout_fn: Option<&TestTimeoutFn>,
    ) -> Result<i32> {
        use std::collections::HashMap;

        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let output_filter = if self.all_output {
            subunit_stream::OutputFilter::All
        } else {
            subunit_stream::OutputFilter::FailuresOnly
        };

        let start_time = std::time::Instant::now();

        // Get the base run ID - each worker will write to run_id-{worker_id}
        let base_run_id = repo.get_next_run_id()?;

        // Get the list of tests to run
        let all_tests = if let Some(ids) = test_ids {
            ids.to_vec()
        } else {
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

        // Initial partition of tests across workers
        let initial_partitions = crate::partition::partition_tests_with_grouping(
            &all_tests,
            &durations,
            concurrency,
            group_regex,
        )
        .map_err(|e| crate::error::Error::Config(format!("Invalid group_regex pattern: {}", e)))?;

        // Compute estimated duration per partition from historical times for ETA
        let partition_estimated_totals: Vec<Duration> = initial_partitions
            .iter()
            .map(|partition| partition.iter().filter_map(|id| durations.get(id)).sum())
            .collect();

        // Create multi-progress for tracking all workers
        let term_width = console::Term::stdout().size().1 as usize;
        let fixed_width = 25;
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

        let mut all_results: HashMap<TestId, crate::repository::TestResult> = HashMap::new();
        let mut any_failed = false;
        let mut restarts = 0;

        // pending_partitions: list of (worker_id, test_ids) to run.
        // On restart, only the timed-out workers' remaining tests are re-partitioned.
        let mut pending_partitions: Vec<(usize, Vec<TestId>)> = initial_partitions
            .into_iter()
            .enumerate()
            .filter(|(_, p)| !p.is_empty())
            .collect();

        loop {
            // Spawn workers, supervisor threads, and parse threads for pending partitions
            type SupervisorResult = std::result::Result<
                std::result::Result<std::process::ExitStatus, TimeoutReason>,
                std::io::Error,
            >;
            let mut supervisors: Vec<(usize, std::thread::JoinHandle<SupervisorResult>)> =
                Vec::new();
            #[allow(clippy::type_complexity)]
            let mut parse_threads: Vec<(
                usize,
                ProgressBar,
                std::thread::JoinHandle<Result<crate::repository::TestRun>>,
                std::thread::JoinHandle<std::result::Result<(), std::io::Error>>,
                std::thread::JoinHandle<std::result::Result<(), std::io::Error>>,
                Option<TestWatchdog>,
            )> = Vec::new();
            // Keep temp files alive for the duration of this iteration
            let mut _temp_files = Vec::new();

            for (worker_id, partition) in &pending_partitions {
                let worker_id = *worker_id;

                let worker_fixed = 22;
                let worker_bar_width = ((term_width.saturating_sub(worker_fixed + 30))
                    / concurrency.min(4))
                .clamp(15, 40);
                let worker_max_msg = term_width
                    .saturating_sub(worker_bar_width + worker_fixed)
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
                    self.test_args.as_deref(),
                )?;
                _temp_files.push(temp_file);

                let mut child = spawn_in_process_group(
                    &cmd_str,
                    Path::new(self.base_path.as_deref().unwrap_or(".")),
                )
                .map_err(|e| {
                    crate::error::Error::CommandExecution(format!(
                        "Failed to spawn worker {}: {}",
                        worker_id, e
                    ))
                })?;

                let stdout = child.stdout.take().expect("stdout was piped");
                let stderr = child.stderr.take().expect("stderr was piped");

                let worker_run_id = format!("{}-{}", base_run_id, worker_id);
                let (_, raw_writer) = repo.begin_test_run_raw()?;

                let (tx, rx) = std::sync::mpsc::sync_channel(100);
                let worker_activity =
                    no_output_timeout.map(|_| crate::test_runner::ActivityTracker::new());

                let tee_thread = if let Some(ref tracker) = worker_activity {
                    crate::test_runner::spawn_stdout_tee_tracked(
                        stdout,
                        raw_writer,
                        tx,
                        tracker.clone(),
                    )
                } else {
                    crate::test_runner::spawn_stdout_tee(stdout, raw_writer, tx)
                };

                let stderr_thread =
                    crate::test_runner::spawn_stderr_forwarder(stderr, worker_bar.clone());

                let channel_reader = crate::test_runner::ChannelReader::new(rx);

                let worker_watchdog = test_timeout_fn.as_ref().map(|_| TestWatchdog::new());
                let watchdog_for_thread = worker_watchdog.clone();
                let watchdog_for_supervisor = worker_watchdog.clone();
                let test_timeout_fn_clone = test_timeout_fn.cloned();

                // Supervisor thread: calls wait_with_timeout concurrently so a hung
                // worker is killed immediately, unblocking the parse/tee threads.
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
                    let mut failures = 0;
                    let mut completed_duration = Duration::ZERO;
                    let worker_bar_for_bytes = worker_bar_clone.clone();
                    subunit_stream::parse_stream_with_progress(
                        channel_reader,
                        worker_run_id_clone,
                        |test_id, status| {
                            if let Some(ref wd) = watchdog_for_thread {
                                if matches!(status, subunit_stream::ProgressStatus::InProgress) {
                                    let timeout =
                                        test_timeout_fn_clone.as_ref().and_then(|f| f(test_id));
                                    wd.on_test_start(test_id, timeout);
                                } else if !status.indicator().is_empty() {
                                    wd.on_test_complete(test_id);
                                }
                            }

                            let indicator = status.indicator();
                            if !indicator.is_empty() {
                                worker_bar_clone.inc(1);
                                overall_bar_clone.inc(1);

                                if let Some(&dur) = worker_durations.get(&TestId::new(test_id)) {
                                    completed_duration += dur;
                                }

                                if matches!(
                                    status,
                                    subunit_stream::ProgressStatus::Failed
                                        | subunit_stream::ProgressStatus::UnexpectedSuccess
                                ) {
                                    failures += 1;
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

                                let fail_msg = format_failure_msg(failures, true);
                                let eta_msg = format_eta(
                                    worker_estimated_total,
                                    completed_duration,
                                    worker_start_time.elapsed(),
                                );
                                let extra_len = if failures > 0 {
                                    9 + failures.to_string().len()
                                } else {
                                    0
                                } + eta_msg.len();
                                let short_name =
                                    truncate_test_name(test_id, worker_max_msg, extra_len);

                                worker_bar_clone.set_message(format!(
                                    "{} {}{}{}",
                                    indicator, short_name, fail_msg, eta_msg
                                ));
                            }
                        },
                        |bytes| {
                            write_non_subunit_output(&worker_bar_for_bytes, bytes);
                        },
                        output_filter_clone,
                    )
                });

                supervisors.push((worker_id, supervisor));
                parse_threads.push((
                    worker_id,
                    worker_bar,
                    parse_thread,
                    tee_thread,
                    stderr_thread,
                    worker_watchdog,
                ));
            }

            // Wait for all supervisors first. When a supervisor kills a hung worker,
            // the stdout pipe closes, unblocking the tee/parse threads for that worker.
            let mut supervisor_results: HashMap<
                usize,
                std::result::Result<std::process::ExitStatus, TimeoutReason>,
            > = HashMap::new();
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
                supervisor_results.insert(worker_id, result);
            }

            // Now collect from parse threads (safe: all workers have exited or been killed)
            let mut worker_watchdogs: HashMap<usize, Option<TestWatchdog>> = HashMap::new();
            for (worker_id, worker_bar, parse_thread, tee_thread, stderr_thread, watchdog) in
                parse_threads
            {
                let worker_run = parse_thread.join().map_err(|_| {
                    crate::error::Error::CommandExecution(format!(
                        "Parse thread {} panicked",
                        worker_id
                    ))
                })??;

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

                let worker_tag = format!("worker-{}", worker_id);
                let mut worker_run = worker_run;
                for (_, result) in worker_run.results.iter_mut() {
                    if !result.tags.contains(&worker_tag) {
                        result.tags.push(worker_tag.clone());
                    }
                }

                for (test_id, result) in worker_run.results {
                    all_results.insert(test_id, result);
                }

                worker_watchdogs.insert(worker_id, watchdog);
            }

            // Compute restart partitions from timed-out workers
            let mut restart_partitions: Vec<(usize, Vec<TestId>)> = Vec::new();
            for (worker_id, result) in &supervisor_results {
                match result {
                    Err(TimeoutReason::TestTimeout(hung_test)) => {
                        tracing::warn!(
                            "worker {} killed (test {} timed out)",
                            worker_id,
                            hung_test
                        );
                        let test_id = TestId::new(hung_test);
                        all_results.insert(
                            test_id.clone(),
                            crate::repository::TestResult::error(
                                test_id,
                                "test timed out (killed after per-test timeout)",
                            ),
                        );
                        any_failed = true;

                        let completed = worker_watchdogs
                            .get(worker_id)
                            .and_then(|wd| wd.as_ref())
                            .map(|wd| wd.completed_tests())
                            .unwrap_or_default();
                        let completed_in_results: std::collections::HashSet<&str> =
                            all_results.keys().map(|id| id.as_str()).collect();

                        // Find this worker's original partition
                        let original_partition: &Vec<TestId> = &pending_partitions
                            .iter()
                            .find(|(wid, _)| wid == worker_id)
                            .expect("worker_id must exist in pending_partitions")
                            .1;

                        let remaining: Vec<TestId> = original_partition
                            .iter()
                            .filter(|id| {
                                !completed.contains(id.as_str())
                                    && !completed_in_results.contains(id.as_str())
                                    && id.as_str() != hung_test
                            })
                            .cloned()
                            .collect();

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
                        any_failed = true;
                    }
                    Err(TimeoutReason::NoOutput) => {
                        tracing::warn!(
                            "worker {} killed (no output for {:?})",
                            worker_id,
                            no_output_timeout.unwrap()
                        );
                        any_failed = true;
                    }
                    Ok(status) if !status.success() => {
                        any_failed = true;
                    }
                    Ok(_) => {}
                }
            }

            restarts += 1;
            if restart_partitions.is_empty() || restarts > MAX_TEST_TIMEOUT_RESTARTS {
                if restarts > MAX_TEST_TIMEOUT_RESTARTS && !restart_partitions.is_empty() {
                    tracing::error!(
                        "exceeded maximum restart limit ({}), stopping",
                        MAX_TEST_TIMEOUT_RESTARTS
                    );
                }
                break;
            }

            tracing::info!(
                "restarting {} workers with remaining tests",
                restart_partitions.len()
            );
            pending_partitions = restart_partitions;
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

        let duration = start_time.elapsed();
        let exit_code = if combined_run.count_failures() > 0 || any_failed {
            1
        } else {
            0
        };

        // Update failing tests and test times
        crate::commands::utils::update_repository_failing_tests(repo, &combined_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &combined_run)?;
        crate::commands::utils::store_run_metadata(
            repo,
            &run_id_for_display,
            Some(&test_cmd.config().test_command),
            Some(concurrency as u32),
            Some(duration),
            Some(exit_code),
        )?;

        // Dispose instances (done explicitly before drop to handle errors)
        drop(dispose_guard);
        test_cmd.dispose_instances(&instance_ids)?;
        if test_cmd.config().instance_provision.is_some() {
            ui.output("Disposed instances")?;
        }

        // Display summary and slow test warnings
        crate::commands::utils::display_test_summary(ui, &run_id_for_display, &combined_run)?;
        crate::commands::utils::warn_slow_tests(ui, &combined_run, &durations)?;

        Ok(exit_code)
    }

    /// Run each test in complete isolation (one test per process)
    #[allow(clippy::too_many_arguments)]
    fn run_isolated(
        &self,
        ui: &mut dyn UI,
        repo: &mut Box<dyn crate::repository::Repository>,
        test_cmd: &TestCommand,
        test_ids: &[crate::repository::TestId],
        test_timeout: &TimeoutSetting,
        historical_times: &HashMap<TestId, Duration>,
        max_duration: Option<Duration>,
    ) -> Result<i32> {
        use std::collections::HashMap;

        let start_time = std::time::Instant::now();

        // Get the base run ID - each isolated test will write to its own file
        let base_run_id = repo.get_next_run_id()?;

        ui.output(&format!(
            "Running {} tests in isolated mode (one test per process)",
            test_ids.len()
        ))?;

        let mut all_results = HashMap::new();
        let mut any_failed = false;

        for (idx, test_id) in test_ids.iter().enumerate() {
            // Check max_duration before starting each test
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

            let per_test_timeout =
                test_timeout.effective_timeout(historical_times.get(test_id).copied());

            ui.output(&format!("  [{}/{}] {}", idx + 1, test_ids.len(), test_id))?;

            // Build command for this single test
            // IMPORTANT: Keep _temp_file alive until after process completes
            let (cmd_str, _temp_file) = test_cmd.build_command_full(
                Some(std::slice::from_ref(test_id)),
                false,
                None,
                self.test_args.as_deref(),
            )?;

            // Spawn process for this test in its own process group
            let test_start = std::time::Instant::now();
            let mut child = spawn_in_process_group(
                &cmd_str,
                Path::new(self.base_path.as_deref().unwrap_or(".")),
            )
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!(
                    "Failed to execute test {}: {}",
                    test_id, e
                ))
            })?;

            // Drain stdout in a background thread to prevent pipe buffer deadlock
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

            // Explicitly drop temp file now that process has completed
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
                    crate::repository::TestResult::error(test_id.clone(), msg)
                        .with_duration(elapsed),
                );
                any_failed = true;
                continue;
            }

            if wait_result.is_ok_and(|s| !s.success()) {
                any_failed = true;
            }

            // Parse test results
            let test_run_id = format!("{}-{}", base_run_id, idx);
            let test_run = subunit_stream::parse_stream(stdout_bytes.as_slice(), test_run_id)?;

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

        let duration = start_time.elapsed();
        let exit_code = if combined_run.count_failures() > 0 || any_failed {
            1
        } else {
            0
        };

        // Update failing tests and test times
        crate::commands::utils::update_repository_failing_tests(repo, &combined_run, self.partial)?;
        crate::commands::utils::update_test_times_from_run(repo, &combined_run)?;
        crate::commands::utils::store_run_metadata(
            repo,
            &run_id_for_display,
            Some(&test_cmd.config().test_command),
            Some(1),
            Some(duration),
            Some(exit_code),
        )?;

        // Display summary and slow test warnings
        crate::commands::utils::display_test_summary(ui, &run_id_for_display, &combined_run)?;
        crate::commands::utils::warn_slow_tests(ui, &combined_run, historical_times)?;

        Ok(exit_code)
    }
}

impl Command for RunCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        // Auto-detect and generate config if --auto and no config exists yet
        if self.auto && crate::config::TestrConfig::find_in_directory(base).is_err() {
            let auto_cmd = crate::commands::auto::AutoCommand::new(self.base_path.clone());
            let exit_code = auto_cmd.execute(ui)?;
            if exit_code != 0 {
                return Ok(exit_code);
            }
        }

        // Open repository (auto-init if config file exists or --force-init)
        let mut repo =
            open_or_init_repository(self.base_path.as_deref(), self.force_init || self.auto, ui)?;

        // Load test command configuration
        let test_cmd = TestCommand::from_directory(base)?;

        // Resolve timeout settings: CLI flags take precedence over config file values
        let test_timeout = if self.test_timeout != TimeoutSetting::Disabled {
            self.test_timeout.clone()
        } else {
            test_cmd.config().parsed_test_timeout()?
        };
        let max_duration = if self.max_duration != TimeoutSetting::Disabled {
            self.max_duration.clone()
        } else {
            test_cmd.config().parsed_max_duration()?
        };
        let no_output_timeout = self
            .no_output_timeout
            .or(test_cmd.config().parsed_no_output_timeout()?);

        if test_timeout != TimeoutSetting::Disabled {
            tracing::info!("per-test timeout: {:?}", test_timeout);
        }
        if max_duration != TimeoutSetting::Disabled {
            tracing::info!("max run duration: {:?}", max_duration);
        }
        if let Some(t) = no_output_timeout {
            tracing::info!("no-output timeout: {:?}", t);
        }

        // Determine which tests to run
        let mut test_ids = if self.failing_only {
            let failing = repo.get_failing_tests()?;
            if failing.is_empty() {
                ui.output("No failing tests to run")?;
                return Ok(0);
            }
            Some(failing)
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

        // Compute effective max_duration and historical times for timeouts/ETA
        let historical_times: HashMap<TestId, Duration> = repo.get_test_times().unwrap_or_default();
        let max_duration_value = match &max_duration {
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
        };

        // Build per-test timeout lookup closure
        let test_timeout_fn: Option<TestTimeoutFn> = if test_timeout != TimeoutSetting::Disabled {
            let tt = test_timeout.clone();
            let ht = historical_times.clone();
            Some(Arc::new(move |test_id: &str| {
                tt.effective_timeout(ht.get(&TestId::new(test_id)).copied())
            }))
        } else {
            None
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
                    let exit_code = self.run_isolated(
                        ui,
                        &mut repo,
                        &test_cmd,
                        &all_tests,
                        &test_timeout,
                        &historical_times,
                        max_duration_value,
                    )?;

                    if exit_code != 0 {
                        ui.output(&format!("\nTests failed on iteration {}", iteration))?;
                        return Ok(exit_code);
                    }

                    iteration += 1;
                }
            } else {
                self.run_isolated(
                    ui,
                    &mut repo,
                    &test_cmd,
                    &all_tests,
                    &test_timeout,
                    &historical_times,
                    max_duration_value,
                )
            }
        } else if self.until_failure {
            // Run tests in a loop until failure (non-isolated)
            let mut iteration = 1;
            loop {
                ui.output(&format!("\n=== Iteration {} ===", iteration))?;

                let exit_code = if concurrency > 1 {
                    self.run_parallel(
                        ui,
                        &mut repo,
                        &test_cmd,
                        test_ids.as_deref(),
                        concurrency,
                        max_duration_value,
                        no_output_timeout,
                        test_timeout_fn.as_ref(),
                    )?
                } else {
                    self.run_serial(
                        ui,
                        &mut repo,
                        &test_cmd,
                        test_ids.as_deref(),
                        max_duration_value,
                        no_output_timeout,
                        test_timeout_fn.as_ref(),
                    )?
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
                self.run_parallel(
                    ui,
                    &mut repo,
                    &test_cmd,
                    test_ids.as_deref(),
                    concurrency,
                    max_duration_value,
                    no_output_timeout,
                    test_timeout_fn.as_ref(),
                )
            } else {
                // Serial execution
                self.run_serial(
                    ui,
                    &mut repo,
                    &test_cmd,
                    test_ids.as_deref(),
                    max_duration_value,
                    no_output_timeout,
                    test_timeout_fn.as_ref(),
                )
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
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::RepositoryFactory;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_run_command_no_config() {
        let temp = TempDir::new().unwrap();

        // Initialize repo but no .testr.conf
        let factory = InquestRepositoryFactory;
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
        let factory = InquestRepositoryFactory;
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
