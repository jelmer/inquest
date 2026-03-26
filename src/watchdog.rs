//! Test timeout watchdog for detecting and killing hung tests.
//!
//! Provides [`TestWatchdog`] for tracking per-test deadlines in streaming mode,
//! and [`wait_with_timeout`] for waiting on child processes with multiple
//! timeout conditions (overall, no-output, and per-test).

use crate::config::TIMEOUT_POLL_INTERVAL;
use crate::test_runner::ActivityTracker;
use std::collections::{HashMap, HashSet};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Kill a child process and all its descendants by sending SIGKILL to its
/// process group. Falls back to killing just the child if the group kill fails.
#[cfg(unix)]
fn kill_process_tree(child: &mut Child) -> std::io::Result<()> {
    let pid = child.id() as libc::pid_t;
    // Try to kill the process group (negative PID)
    let ret = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if ret != 0 {
        // Process group kill failed — fall back to killing just the child
        child.kill()?;
    }
    Ok(())
}

#[cfg(windows)]
fn kill_process_tree(child: &mut Child) -> std::io::Result<()> {
    let pid = child.id();
    // taskkill /T kills the entire process tree; /F forces termination.
    let status = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => child.kill(),
    }
}

#[cfg(not(any(unix, windows)))]
fn kill_process_tree(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

/// Why a process was killed by the timeout logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutReason {
    /// The overall timeout expired.
    Timeout,
    /// No output was received for too long.
    NoOutput,
    /// A specific test exceeded its per-test timeout.
    TestTimeout(String),
}

/// Tracks per-test deadlines for tests running inside a single process.
///
/// The parse thread calls [`on_test_start`](TestWatchdog::on_test_start) and
/// [`on_test_complete`](TestWatchdog::on_test_complete) as subunit events arrive.
/// The wait loop polls [`check_timeout`](TestWatchdog::check_timeout) to detect
/// hung tests.
#[derive(Clone, Default)]
pub struct TestWatchdog {
    inner: Arc<Mutex<WatchdogState>>,
}

#[derive(Default)]
struct WatchdogState {
    /// Currently in-progress tests with their deadlines.
    in_progress: HashMap<String, Instant>,
    /// Tests that reached a terminal status.
    completed: HashSet<String>,
}

impl TestWatchdog {
    /// Create a new watchdog with no tests tracked.
    pub fn new() -> Self {
        TestWatchdog {
            inner: Arc::new(Mutex::new(WatchdogState::default())),
        }
    }

    /// Record that a test has started. If `timeout` is `Some`, a deadline is set.
    pub fn on_test_start(&self, test_id: &str, timeout: Option<Duration>) {
        let mut state = self.inner.lock().unwrap();
        if let Some(t) = timeout {
            state
                .in_progress
                .insert(test_id.to_string(), Instant::now() + t);
        }
    }

    /// Record that a test has completed (any terminal status).
    pub fn on_test_complete(&self, test_id: &str) {
        let mut state = self.inner.lock().unwrap();
        state.in_progress.remove(test_id);
        state.completed.insert(test_id.to_string());
    }

    /// Check whether any in-progress test has exceeded its deadline.
    ///
    /// Returns the test ID of the first overdue test, or `None`.
    pub fn check_timeout(&self) -> Option<String> {
        let state = self.inner.lock().unwrap();
        let now = Instant::now();
        // Return the test with the earliest expired deadline
        state
            .in_progress
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .min_by_key(|(_, deadline)| *deadline)
            .map(|(id, _)| id.clone())
    }

    /// Snapshot of all test IDs that have completed so far.
    pub fn completed_tests(&self) -> HashSet<String> {
        self.inner.lock().unwrap().completed.clone()
    }
}

/// Wait for a child process with optional timeout, no-output detection,
/// and per-test watchdog.
///
/// Returns:
/// - `Ok(Ok(status))` — process exited normally
/// - `Ok(Err(reason))` — process was killed due to a timeout
/// - `Err(io_error)` — system error waiting/killing
pub fn wait_with_timeout(
    child: &mut Child,
    timeout: Option<Duration>,
    no_output_timeout: Option<Duration>,
    activity: Option<&ActivityTracker>,
    watchdog: Option<&TestWatchdog>,
) -> std::io::Result<Result<ExitStatus, TimeoutReason>> {
    let needs_polling = timeout.is_some() || no_output_timeout.is_some() || watchdog.is_some();

    if !needs_polling {
        return child.wait().map(Ok);
    }

    let kill_and_reap = |child: &mut Child| -> std::io::Result<()> {
        kill_process_tree(child)?;
        let _ = child.wait();
        Ok(())
    };

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Ok(status));
        }
        if let Some(t) = timeout {
            if start.elapsed() >= t {
                kill_and_reap(child)?;
                return Ok(Err(TimeoutReason::Timeout));
            }
        }
        if let (Some(no_out), Some(tracker)) = (no_output_timeout, activity) {
            if tracker.elapsed_since_last() >= no_out {
                kill_and_reap(child)?;
                return Ok(Err(TimeoutReason::NoOutput));
            }
        }
        if let Some(wd) = watchdog {
            if let Some(hung_test) = wd.check_timeout() {
                kill_and_reap(child)?;
                return Ok(Err(TimeoutReason::TestTimeout(hung_test)));
            }
        }
        std::thread::sleep(TIMEOUT_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watchdog_no_timeout_returns_none() {
        let wd = TestWatchdog::new();
        wd.on_test_start("test1", None);
        assert_eq!(wd.check_timeout(), None);
    }

    #[test]
    fn test_watchdog_not_expired() {
        let wd = TestWatchdog::new();
        wd.on_test_start("test1", Some(Duration::from_secs(60)));
        assert_eq!(wd.check_timeout(), None);
    }

    #[test]
    fn test_watchdog_expired() {
        let wd = TestWatchdog::new();
        wd.on_test_start("test1", Some(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(wd.check_timeout(), Some("test1".to_string()));
    }

    #[test]
    fn test_watchdog_complete_clears() {
        let wd = TestWatchdog::new();
        wd.on_test_start("test1", Some(Duration::ZERO));
        wd.on_test_complete("test1");
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(wd.check_timeout(), None);
        assert!(wd.completed_tests().contains("test1"));
    }

    #[test]
    fn test_watchdog_multiple_tests() {
        let wd = TestWatchdog::new();
        wd.on_test_start("fast", Some(Duration::from_secs(60)));
        wd.on_test_start("hung", Some(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(wd.check_timeout(), Some("hung".to_string()));
    }

    #[test]
    fn test_watchdog_completed_tests_snapshot() {
        let wd = TestWatchdog::new();
        wd.on_test_start("a", Some(Duration::from_secs(60)));
        wd.on_test_start("b", Some(Duration::from_secs(60)));
        wd.on_test_complete("a");
        let completed = wd.completed_tests();
        assert_eq!(completed.len(), 1);
        assert!(completed.contains("a"));
    }

    #[test]
    fn test_wait_with_timeout_normal_exit() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let result = wait_with_timeout(&mut child, None, None, None, None).unwrap();
        assert!(result.is_ok());
        assert!(result.unwrap().success());
    }

    #[test]
    fn test_wait_with_timeout_overall_timeout() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let result = wait_with_timeout(
            &mut child,
            Some(Duration::from_millis(100)),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(result, Err(TimeoutReason::Timeout));
    }

    #[test]
    fn test_wait_with_timeout_watchdog_kills_process() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();

        let wd = TestWatchdog::new();
        // Set a test with an already-expired deadline
        wd.on_test_start("hung_test", Some(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(1));

        let result = wait_with_timeout(&mut child, None, None, None, Some(&wd)).unwrap();
        assert_eq!(
            result,
            Err(TimeoutReason::TestTimeout("hung_test".to_string()))
        );
    }

    #[test]
    fn test_wait_with_timeout_watchdog_no_timeout_if_test_completes() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();

        let wd = TestWatchdog::new();
        wd.on_test_start("fast_test", Some(Duration::from_secs(60)));

        let result = wait_with_timeout(&mut child, None, None, None, Some(&wd)).unwrap();
        // Process exits before timeout
        assert!(result.is_ok());
    }

    #[test]
    fn test_wait_with_timeout_no_output_timeout() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();

        let tracker = crate::test_runner::ActivityTracker::new();
        // Don't touch the tracker — no output simulated

        let result = wait_with_timeout(
            &mut child,
            None,
            Some(Duration::from_millis(100)),
            Some(&tracker),
            None,
        )
        .unwrap();
        assert_eq!(result, Err(TimeoutReason::NoOutput));
    }

    #[test]
    fn test_wait_with_timeout_no_output_averted_by_activity() {
        use std::process::{Command, Stdio};
        // Process that exits quickly
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();

        let tracker = crate::test_runner::ActivityTracker::new();
        tracker.touch(); // Simulate recent output

        let result = wait_with_timeout(
            &mut child,
            None,
            Some(Duration::from_secs(60)),
            Some(&tracker),
            None,
        )
        .unwrap();
        // Process exits before no-output timeout
        assert!(result.is_ok());
    }
}
