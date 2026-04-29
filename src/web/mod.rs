//! HTTP server backing the `inq web` command.
//!
//! `inq web` starts a small HTTP server bound to the loopback interface that
//! serves a single-page UI. The UI displays the full set of tests known to
//! the repository, grouped into a hierarchy via the configured `group_regex`
//! (or a user-supplied one), with per-test history (run count, failure count,
//! flakiness, last status, average duration). It supports searching, filtering,
//! and triggering runs of individual tests or pattern-matched subsets.
//!
//! Live test progress is streamed to the browser over Server-Sent Events
//! (SSE). The server spawns `inq run --subunit ...` as a child process and
//! re-emits each test's start/finish event as the run progresses, so the UI
//! can highlight currently-running tests and recolor them as they complete.

use crate::error::{Error, Result};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

mod index;
mod profiles;
mod repo;
mod run;
mod runs;
mod test_history;
mod test_list;

pub(crate) const SSE_CHANNEL_CAPACITY: usize = 1024;

/// Shared application state passed to every request handler.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) base: PathBuf,
    pub(crate) runs: Arc<Mutex<HashMap<u64, RunHandle>>>,
    pub(crate) next_run: Arc<AtomicU64>,
}

/// Per-run server-side handle: tracks the spawned child, broadcasts
/// progress events to currently-attached SSE listeners, and keeps a buffered
/// log of every event so newly-attached clients can replay history.
///
/// The buffered log is essential: there's a window between `POST /api/run`
/// returning and the browser opening the SSE stream during which any events
/// the child produces would otherwise be dropped (a `broadcast::Sender`
/// silently discards messages when there are zero receivers).
pub(crate) struct RunHandle {
    pub(crate) tx: broadcast::Sender<RunEvent>,
    pub(crate) history: Arc<Mutex<Vec<RunEvent>>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    /// Child process has been spawned — emitted before any test events so
    /// the UI can stop showing "starting…".
    Spawned {
        command: String,
    },
    Started {
        test_id: String,
    },
    Completed {
        test_id: String,
        status: String,
        duration_secs: Option<f64>,
    },
    /// A line of textual output from the child process (typically stderr —
    /// build progress, error messages). Surfaced so the UI can show live
    /// log output during the discovery/build phase.
    Log {
        line: String,
    },
    Finished {
        exit_code: i32,
    },
    Error {
        message: String,
    },
}

/// Push an event into both the per-run history buffer and the broadcast
/// channel. The history buffer ensures newly-attached SSE clients can
/// replay everything from the start of the run.
pub(crate) fn emit(history: &Mutex<Vec<RunEvent>>, tx: &broadcast::Sender<RunEvent>, ev: RunEvent) {
    if let Ok(mut h) = history.lock() {
        h.push(ev.clone());
    }
    let _ = tx.send(ev);
}

/// Build the JSON-error response shape used by every handler. The frontend
/// expects `{ "error": "<message>" }` with the appropriate HTTP status.
pub(crate) fn json_error(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({ "error": msg });
    (status, Json(body)).into_response()
}

/// Run the HTTP server. Blocks until shutdown (Ctrl-C).
pub async fn serve(base: PathBuf, addr: String) -> Result<()> {
    let state = AppState {
        base,
        runs: Arc::new(Mutex::new(HashMap::new())),
        next_run: Arc::new(AtomicU64::new(1)),
    };

    let app = Router::new()
        .route("/", get(index::index))
        .route("/api/tests", get(test_list::api_tests))
        .route("/api/tree", get(test_list::api_tree))
        .route("/api/runs", get(runs::api_runs))
        .route("/api/runs/:id", get(runs::api_run_detail))
        .route("/api/test/:id/history", get(test_history::api_test_history))
        .route("/api/profiles", get(profiles::api_profiles))
        .route("/api/repo", get(repo::api_repo))
        .route("/api/run", post(run::api_start_run))
        .route("/api/active/:id/events", get(run::api_run_events))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| Error::Other(format!("Failed to bind to {}: {}", addr, e)))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| Error::Other(format!("Web server error: {}", e)))?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
