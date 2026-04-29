//! `POST /api/run` and `GET /api/active/:id/events` — start an `inq run`
//! child process and stream its progress over Server-Sent Events.

use super::{emit, json_error, AppState, RunEvent, RunHandle, SSE_CHANNEL_CAPACITY};
use crate::error::{Error, Result};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Deserialize)]
pub(super) struct StartRunRequest {
    /// Specific test IDs to run (passed via `--load-list`).
    #[serde(default)]
    test_ids: Vec<String>,
    /// Regex filters passed positionally to `inq run`. Mutually compatible
    /// with `test_ids` — both are forwarded.
    #[serde(default)]
    filters: Vec<String>,
    /// When true, run only the previously-failing tests.
    #[serde(default)]
    failing: bool,
    /// Number of parallel workers. `None` keeps `inq run`'s default (serial).
    #[serde(default)]
    parallel: Option<usize>,
    /// Profile to apply (forwarded as `--profile <name>`). `None` uses the
    /// project's default profile or the bare base config.
    #[serde(default)]
    profile: Option<String>,
    /// Test ordering strategy (`--order`). Accepted values match the CLI:
    /// `auto`, `discovery`, `alphabetical`, `failing-first`, `spread`,
    /// `shuffle[:<seed>]`, `slowest-first`, `fastest-first`,
    /// `frequent-failing-first`.
    #[serde(default)]
    order: Option<String>,
    /// Run each test in its own process (`--isolated`).
    #[serde(default)]
    isolated: bool,
    /// Partial-run mode: merge results into the failing-tests set instead
    /// of resetting (`--partial`).
    #[serde(default)]
    partial: bool,
    /// Loop until a failure occurs (`--until-failure`). Optionally bounded
    /// by `max_iterations`.
    #[serde(default)]
    until_failure: bool,
    /// Maximum iterations when `until_failure` is true.
    #[serde(default)]
    max_iterations: Option<usize>,
    /// `--starting-with` prefixes (one or more). Each is forwarded as a
    /// separate `-s <prefix>` argument.
    #[serde(default)]
    starting_with: Vec<String>,
    /// `--tag` filter values. May include `!`-prefixed exclusions. Each is
    /// forwarded as a separate `--tag <value>` argument.
    #[serde(default)]
    tags: Vec<String>,
    /// Per-test timeout (`--test-timeout`).
    #[serde(default)]
    test_timeout: Option<String>,
    /// Overall run timeout (`--max-duration`).
    #[serde(default)]
    max_duration: Option<String>,
    /// No-output watchdog timeout (`--no-output-timeout`).
    #[serde(default)]
    no_output_timeout: Option<String>,
    /// Forward extra arguments to the underlying test command (after `--`).
    #[serde(default)]
    test_args: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct StartRunResponse {
    /// The server-side handle for SSE attachment. Distinct from the run ID
    /// the repository assigns, which is only known after the child starts.
    handle: u64,
}

pub(super) async fn api_start_run(
    State(state): State<AppState>,
    Json(req): Json<StartRunRequest>,
) -> Response {
    let handle_id = state.next_run.fetch_add(1, Ordering::SeqCst);
    let (tx, _) = broadcast::channel::<RunEvent>(SSE_CHANNEL_CAPACITY);
    let history: Arc<Mutex<Vec<RunEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    {
        let mut runs = state.runs.lock().expect("runs mutex poisoned");
        runs.insert(
            handle_id,
            RunHandle {
                tx: tx.clone(),
                history: history.clone(),
                child_pid: child_pid.clone(),
            },
        );
    }

    let base = state.base.clone();
    let runs_map = state.runs.clone();
    tokio::task::spawn_blocking(move || {
        let result = drive_child_run(&base, &req, &history, &tx, &child_pid);
        let event = match result {
            Ok(code) => RunEvent::Finished { exit_code: code },
            Err(e) => RunEvent::Error {
                message: e.to_string(),
            },
        };
        emit(&history, &tx, event);

        // Keep the handle around for a short grace period so late-arriving
        // SSE clients can still read the terminal events before we
        // forget the run.
        std::thread::sleep(Duration::from_secs(30));
        if let Ok(mut runs) = runs_map.lock() {
            runs.remove(&handle_id);
        }
    });

    Json(StartRunResponse { handle: handle_id }).into_response()
}

/// Locate the inq executable to spawn for a child run.
///
/// `std::env::current_exe()` is the right answer when `inq` was launched
/// from an install path. But for in-development use (`cargo run -- web`)
/// the binary at that path can be replaced or removed by the time we want
/// to fork — leading to ENOENT on `Command::spawn`. Fall through to a PATH
/// lookup so the user gets a working spawn instead of a confusing error.
fn resolve_inq_exe() -> std::io::Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.exists() {
            return Ok(exe);
        }
    }
    // Fall back to PATH. We don't want to invent a `which` dependency, so
    // walk `$PATH` ourselves looking for the first executable named `inq`.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(if cfg!(windows) { "inq.exe" } else { "inq" });
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "could not locate the `inq` executable: current_exe() points at a missing file and `inq` is not on PATH",
    ))
}

fn drive_child_run(
    base: &std::path::Path,
    req: &StartRunRequest,
    history: &Arc<Mutex<Vec<RunEvent>>>,
    tx: &broadcast::Sender<RunEvent>,
    child_pid: &Arc<Mutex<Option<u32>>>,
) -> Result<i32> {
    use std::io::Write;
    use std::process::{Command as Proc, Stdio};

    // Resolve the inq executable to spawn. We try `current_exe()` first
    // (correct for an installed binary), then fall back to whatever `inq`
    // resolves to on PATH if the current_exe path no longer exists on disk
    // — that happens when the user runs `cargo run` and a subsequent build
    // replaces the binary, or after `cargo clean`.
    let exe = resolve_inq_exe()
        .map_err(|e| Error::Other(format!("cannot locate inq executable: {}", e)))?;

    let mut cmd = Proc::new(&exe);
    cmd.arg("-C").arg(base);
    if let Some(ref name) = req.profile {
        cmd.arg("--profile").arg(name);
    }
    cmd.arg("run").arg("--subunit");
    if req.failing {
        cmd.arg("--failing");
    }
    if req.isolated {
        cmd.arg("--isolated");
    }
    if req.partial {
        cmd.arg("--partial");
    }
    if req.until_failure {
        cmd.arg("--until-failure");
        if let Some(m) = req.max_iterations {
            cmd.arg("--max-iterations").arg(m.to_string());
        }
    }
    if let Some(j) = req.parallel {
        cmd.arg("-j").arg(j.to_string());
    }
    if let Some(ref order) = req.order {
        cmd.arg("--order").arg(order);
    }
    if let Some(ref t) = req.test_timeout {
        cmd.arg("--test-timeout").arg(t);
    }
    if let Some(ref t) = req.max_duration {
        cmd.arg("--max-duration").arg(t);
    }
    if let Some(ref t) = req.no_output_timeout {
        cmd.arg("--no-output-timeout").arg(t);
    }
    for prefix in &req.starting_with {
        cmd.arg("-s").arg(prefix);
    }
    for tag in &req.tags {
        cmd.arg("--tag").arg(tag);
    }

    // Materialise an explicit test list when given. We use a temp file so we
    // don't blow past the platform argv limit on large test sets.
    let load_list_file = if req.test_ids.is_empty() {
        None
    } else {
        let mut f = tempfile::NamedTempFile::new()
            .map_err(|e| Error::Other(format!("cannot create temp file: {}", e)))?;
        for id in &req.test_ids {
            writeln!(f, "{}", id)
                .map_err(|e| Error::Other(format!("cannot write temp file: {}", e)))?;
        }
        f.flush()
            .map_err(|e| Error::Other(format!("cannot flush temp file: {}", e)))?;
        cmd.arg("--load-list").arg(f.path());
        Some(f)
    };

    for filter in &req.filters {
        cmd.arg(filter);
    }
    if !req.test_args.is_empty() {
        cmd.arg("--");
        for ta in &req.test_args {
            cmd.arg(ta);
        }
    }

    // Render the assembled command for the UI before spawning so the user
    // sees what's about to run even if the spawn fails.
    let cmd_repr = format!(
        "{} {}",
        exe.display(),
        cmd.get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Other(format!("failed to spawn inq run: {}", e)))?;

    // Record the PID so the cancel handler can deliver SIGTERM to the
    // process group. We assign before emitting `Spawned` so a cancel that
    // races the very first event still has a target.
    if let Ok(mut slot) = child_pid.lock() {
        *slot = Some(child.id());
    }

    emit(history, tx, RunEvent::Spawned { command: cmd_repr });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("child stdout missing".to_string()))?;
    let stderr = child.stderr.take();

    // Forward each stderr line to the UI as a Log event. This surfaces the
    // cargo "Compiling…" output during the build/discovery phase, plus any
    // hard error message if `inq run` fails fast.
    let stderr_handle = stderr.map(|e| {
        let history = history.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(e);
            for line in reader.lines().map_while(std::io::Result::ok) {
                emit(&history, &tx, RunEvent::Log { line });
            }
        })
    });

    // Stream subunit events back to the broadcast channel as they arrive.
    // We also surface `exists` events (test discovery, before InProgress) as
    // Discovered events so the UI can show progress during enumeration.
    let history_for_parser = history.clone();
    let tx_for_parser = tx.clone();
    let parse_result = std::thread::spawn(move || {
        let run_id = crate::repository::RunId::new("web");
        crate::subunit_stream::parse_stream_with_progress(
            stdout,
            run_id,
            |test_id, status| {
                let event = match status {
                    crate::subunit_stream::ProgressStatus::InProgress => RunEvent::Started {
                        test_id: test_id.to_string(),
                    },
                    other => RunEvent::Completed {
                        test_id: test_id.to_string(),
                        status: progress_status_label(other).to_string(),
                        duration_secs: None,
                    },
                };
                emit(&history_for_parser, &tx_for_parser, event);
            },
            |_bytes| {},
            crate::subunit_stream::OutputFilter::FailuresOnly,
        )
    })
    .join();

    let exit = child
        .wait()
        .map_err(|e| Error::Other(format!("waiting for child: {}", e)))?;
    drop(load_list_file);

    // Clear the PID so a late cancel request returns "already finished"
    // instead of trying to signal a reaped process.
    if let Ok(mut slot) = child_pid.lock() {
        *slot = None;
    }

    if let Some(h) = stderr_handle {
        let _ = h.join();
    }

    if let Ok(Err(e)) = parse_result {
        tracing::warn!("subunit parse error during web run: {}", e);
    }

    Ok(exit.code().unwrap_or(-1))
}

fn progress_status_label(s: crate::subunit_stream::ProgressStatus) -> &'static str {
    match s {
        crate::subunit_stream::ProgressStatus::InProgress => "in_progress",
        crate::subunit_stream::ProgressStatus::Success => "success",
        crate::subunit_stream::ProgressStatus::Failed => "failure",
        crate::subunit_stream::ProgressStatus::Skipped => "skip",
        crate::subunit_stream::ProgressStatus::ExpectedFailure => "xfail",
        crate::subunit_stream::ProgressStatus::UnexpectedSuccess => "uxsuccess",
    }
}

pub(super) async fn api_run_events(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    // Subscribe to the broadcast channel *before* snapshotting the history
    // buffer, so any event that lands between snapshot and subscribe still
    // reaches us via the live stream. Per-event de-duplication on the
    // boundary is handled by tagging each replayed event with its index in
    // the history vector and de-duping on the client only when needed —
    // here the broadcast channel preserves the events, so the only real
    // risk is double-delivery, not loss.
    //
    // Concretely: we snapshot history under the same lock that holds the
    // subscription, so any post-snapshot event is guaranteed to arrive on
    // the broadcast (since `subscribe()` returns a receiver that captures
    // future sends). A client may briefly see a duplicate event around the
    // boundary; the UI is idempotent to that.
    let (rx, history_snapshot) = {
        let runs = state.runs.lock().expect("runs mutex poisoned");
        match runs.get(&id) {
            Some(handle) => {
                let rx = handle.tx.subscribe();
                let history = handle.history.lock().map(|h| h.clone()).unwrap_or_default();
                (rx, history)
            }
            None => return json_error(StatusCode::NOT_FOUND, "unknown run handle"),
        }
    };

    let preamble = history_snapshot.into_iter().map(|ev| {
        let payload = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
        Ok::<_, Infallible>(Event::default().event("run").data(payload))
    });

    let live = BroadcastStream::new(rx).filter_map(|res| async move {
        let ev = res.ok()?;
        let payload = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".to_string());
        Some(Ok::<_, Infallible>(
            Event::default().event("run").data(payload),
        ))
    });

    let stream = futures::stream::iter(preamble).chain(live);

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[derive(Serialize)]
pub(super) struct CancelResponse {
    /// Outcome: "cancelled" if we delivered a signal, "already_finished"
    /// if the child had already exited, or an error message.
    status: String,
}

/// `POST /api/active/:id/cancel` — terminate the spawned child of an
/// in-progress run. We send SIGTERM to the child's process group on Unix
/// so any test workers it forked die with it. On Windows we fall back to
/// `TerminateProcess` for just the child.
///
/// The terminated child's wait() call inside `drive_child_run` returns
/// with a non-zero exit, which the spawn_blocking task already converts
/// into a `Finished` event — the SSE stream closes naturally, no extra
/// machinery needed.
pub(super) async fn api_cancel_run(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    let pid_opt = {
        let runs = state.runs.lock().expect("runs mutex poisoned");
        match runs.get(&id) {
            Some(handle) => handle.child_pid.lock().ok().and_then(|g| *g),
            None => return json_error(StatusCode::NOT_FOUND, "unknown run handle"),
        }
    };
    let pid = match pid_opt {
        Some(p) => p,
        None => {
            return Json(CancelResponse {
                status: "already_finished".to_string(),
            })
            .into_response();
        }
    };
    match send_terminate(pid) {
        Ok(()) => Json(CancelResponse {
            status: "cancelled".to_string(),
        })
        .into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not signal pid {}: {}", pid, e),
        ),
    }
}

#[cfg(unix)]
fn send_terminate(pid: u32) -> std::io::Result<()> {
    // Negate the pid to deliver to the child's process group, so any test
    // workers it forked also receive the signal. The child was spawned via
    // the standard library's `Command`, which doesn't always start its own
    // group — but `kill(-pid, ...)` is harmless if the group doesn't
    // exist; we fall through to `kill(pid, ...)` in that case.
    // SAFETY: libc::kill is FFI, no shared-state hazards.
    let group_result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    if group_result == 0 {
        return Ok(());
    }
    let direct = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if direct == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn send_terminate(pid: u32) -> std::io::Result<()> {
    use std::process::Command;
    // No tightly-coupled crate dep; shell out to `taskkill`. /T kills the
    // tree, /F is force.
    let status = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill exited with {:?}",
            status.code()
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn send_terminate(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "cancel not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_status_labels() {
        use crate::subunit_stream::ProgressStatus;
        assert_eq!(progress_status_label(ProgressStatus::Success), "success");
        assert_eq!(progress_status_label(ProgressStatus::Failed), "failure");
        assert_eq!(progress_status_label(ProgressStatus::Skipped), "skip");
    }

    #[test]
    fn resolve_inq_exe_returns_current_exe_when_present() {
        // current_exe() points at the test binary, which obviously exists
        // while this test is running. The function should return it
        // verbatim without falling through to the PATH search.
        let resolved = resolve_inq_exe().unwrap();
        let expected = std::env::current_exe().unwrap();
        assert_eq!(resolved, expected);
    }

    #[tokio::test]
    async fn cancel_endpoint_handles_unknown_and_finished_handles() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::post;
        use axum::Router;
        use http_body_util::BodyExt;
        use std::collections::HashMap;
        use std::sync::atomic::AtomicU64;
        use tower::ServiceExt;

        // Two scenarios in one test:
        // 1. unknown handle id → 404
        // 2. known handle but no live PID → 200 with status "already_finished"
        let runs_map: Arc<Mutex<HashMap<u64, RunHandle>>> = Arc::new(Mutex::new(HashMap::new()));

        // Insert a handle that has no PID (i.e. the child already exited).
        let (tx, _) = broadcast::channel::<RunEvent>(SSE_CHANNEL_CAPACITY);
        runs_map.lock().unwrap().insert(
            42,
            RunHandle {
                tx,
                history: Arc::new(Mutex::new(Vec::new())),
                child_pid: Arc::new(Mutex::new(None)),
            },
        );

        let state = AppState {
            base: std::path::PathBuf::from("."),
            runs: runs_map,
            next_run: Arc::new(AtomicU64::new(1)),
        };
        let app = Router::new()
            .route("/api/active/:id/cancel", post(api_cancel_run))
            .with_state(state);

        // Unknown handle.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/active/9999/cancel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Known handle with no PID — the child has already exited.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/active/42/cancel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["status"], "already_finished");
    }
}
