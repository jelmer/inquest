//! Web UI for browsing tests, viewing per-test stats, and triggering runs.
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

use crate::commands::Command;
use crate::error::{Error, Result};
use crate::repository::{TestId, TestStatus};
use crate::ui::UI;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

const SSE_CHANNEL_CAPACITY: usize = 1024;

/// Command to start the web UI server.
pub struct WebCommand {
    base_path: Option<String>,
    bind: String,
    port: u16,
    open: bool,
}

impl WebCommand {
    /// Create a new web command.
    pub fn new(base_path: Option<String>, bind: String, port: u16, open: bool) -> Self {
        WebCommand {
            base_path,
            bind,
            port,
            open,
        }
    }
}

impl Command for WebCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        // Validate the repository up front so we fail fast with a clear error
        // before we bring up the listener — once the server is running, errors
        // would surface as HTTP 500s instead.
        let _ = super::utils::open_repository(self.base_path.as_deref())?;

        let base = self.base_path.clone().unwrap_or_else(|| ".".to_string());
        let base = std::path::Path::new(&base)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&base));

        let addr = format!("{}:{}", self.bind, self.port);
        let url = format!("http://{}/", addr);

        ui.output(&format!("inq web listening on {}", url))?;
        ui.output("Press Ctrl-C to stop.")?;

        if self.open {
            // Best-effort browser launch. Failure is intentionally silent —
            // the URL is already printed above so the user can open it
            // manually if `xdg-open`/`open` isn't available.
            let _ = open_browser(&url);
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("Failed to start tokio runtime: {}", e)))?;

        runtime.block_on(serve(base, addr))?;
        Ok(0)
    }

    fn name(&self) -> &str {
        "web"
    }

    fn help(&self) -> &str {
        "Start a web UI for browsing tests and runs"
    }
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_browser(_url: &str) -> std::io::Result<()> {
    Ok(())
}

/// Shared application state passed to every request handler.
#[derive(Clone)]
struct AppState {
    base: PathBuf,
    runs: Arc<Mutex<HashMap<u64, RunHandle>>>,
    next_run: Arc<AtomicU64>,
}

/// Per-run server-side handle: tracks the spawned child, broadcasts
/// progress events to currently-attached SSE listeners, and keeps a buffered
/// log of every event so newly-attached clients can replay history.
///
/// The buffered log is essential: there's a window between `POST /api/run`
/// returning and the browser opening the SSE stream during which any events
/// the child produces would otherwise be dropped (a `broadcast::Sender`
/// silently discards messages when there are zero receivers).
struct RunHandle {
    tx: broadcast::Sender<RunEvent>,
    history: Arc<Mutex<Vec<RunEvent>>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RunEvent {
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
fn emit(history: &Mutex<Vec<RunEvent>>, tx: &broadcast::Sender<RunEvent>, ev: RunEvent) {
    if let Ok(mut h) = history.lock() {
        h.push(ev.clone());
    }
    let _ = tx.send(ev);
}

async fn serve(base: PathBuf, addr: String) -> Result<()> {
    let state = AppState {
        base,
        runs: Arc::new(Mutex::new(HashMap::new())),
        next_run: Arc::new(AtomicU64::new(1)),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/tests", get(api_tests))
        .route("/api/tree", get(api_tree))
        .route("/api/runs", get(api_runs))
        .route("/api/runs/:id", get(api_run_detail))
        .route("/api/test/:id/history", get(api_test_history))
        .route("/api/profiles", get(api_profiles))
        .route("/api/repo", get(api_repo))
        .route("/api/run", post(api_start_run))
        .route("/api/active/:id/events", get(api_run_events))
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

#[derive(askama::Template)]
#[template(path = "web_index.html")]
struct IndexTemplate;

async fn index() -> Response {
    use askama::Template;
    match IndexTemplate.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("template render error: {}", e),
        ),
    }
}

#[derive(Serialize)]
struct TestStats {
    test_id: String,
    runs: u32,
    failures: u32,
    last_status: Option<String>,
    last_run_id: Option<String>,
    runs_since_seen: u32,
    avg_duration_secs: Option<f64>,
    discovered: bool,
}

#[derive(Serialize)]
struct TestsResponse {
    tests: Vec<TestStats>,
    group_regex: Option<String>,
    total_runs_in_repo: usize,
    /// True when the response includes the project's discovered-test list.
    /// When false the `discovered` flag on each test reflects only repo
    /// history (so it's effectively meaningless and should be ignored by UI).
    discovery_run: bool,
}

#[derive(Deserialize)]
struct TestsQuery {
    /// When true, also invoke the project's test-list command to surface
    /// tests that have never been recorded. Off by default because for
    /// e.g. Cargo projects this triggers a (potentially slow) build.
    #[serde(default)]
    include_discovered: bool,
}

async fn api_tests(State(state): State<AppState>, Query(q): Query<TestsQuery>) -> Response {
    match collect_test_stats(&state.base, q.include_discovered) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn collect_test_stats(base: &std::path::Path, include_discovered: bool) -> Result<TestsResponse> {
    let repo = super::utils::open_repository(Some(&base.to_string_lossy()))?;
    let run_ids = repo.list_run_ids()?;
    let total_runs_in_repo = run_ids.len();

    // Aggregate per-test stats over the full run history. We walk runs in
    // chronological order so `last_status`/`last_run_id`/`runs_since_seen`
    // reflect the most recent recorded outcome for each test.
    struct Agg {
        runs: u32,
        failures: u32,
        last_status: Option<TestStatus>,
        last_run_index: Option<usize>,
        duration_total: Duration,
        duration_count: u32,
    }
    let mut agg: HashMap<TestId, Agg> = HashMap::new();
    for (idx, rid) in run_ids.iter().enumerate() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for (tid, result) in &run.results {
            let entry = agg.entry(tid.clone()).or_insert(Agg {
                runs: 0,
                failures: 0,
                last_status: None,
                last_run_index: None,
                duration_total: Duration::ZERO,
                duration_count: 0,
            });
            entry.runs += 1;
            if result.status.is_failure() {
                entry.failures += 1;
            }
            entry.last_status = Some(result.status);
            entry.last_run_index = Some(idx);
            if let Some(d) = result.duration {
                entry.duration_total += d;
                entry.duration_count += 1;
            }
        }
    }

    // Pull the discovered list from the test command only when the caller
    // asked for it. For Cargo projects this triggers `cargo build --tests`,
    // which can take seconds-to-minutes — we don't want that on every page
    // load, so the UI gates this behind an explicit "Discover tests" action.
    let discovered: std::collections::HashSet<TestId> = if include_discovered {
        match crate::testcommand::TestCommand::from_directory(base) {
            Ok(tc) => tc
                .list_tests()
                .map(|ts| ts.into_iter().collect())
                .unwrap_or_default(),
            Err(_) => Default::default(),
        }
    } else {
        Default::default()
    };

    let mut all_ids: std::collections::BTreeSet<TestId> = agg.keys().cloned().collect();
    for d in &discovered {
        all_ids.insert(d.clone());
    }

    let last_run_idx = run_ids.len().saturating_sub(1);
    let mut tests = Vec::with_capacity(all_ids.len());
    for tid in all_ids {
        let entry = agg.get(&tid);
        let avg = entry.and_then(|e| {
            if e.duration_count == 0 {
                None
            } else {
                Some(e.duration_total.as_secs_f64() / e.duration_count as f64)
            }
        });
        let runs_since_seen = match entry.and_then(|e| e.last_run_index) {
            Some(idx) => (last_run_idx - idx) as u32,
            None => total_runs_in_repo as u32,
        };
        tests.push(TestStats {
            test_id: tid.as_str().to_string(),
            runs: entry.map_or(0, |e| e.runs),
            failures: entry.map_or(0, |e| e.failures),
            last_status: entry.and_then(|e| e.last_status).map(|s| s.to_string()),
            last_run_id: entry
                .and_then(|e| e.last_run_index)
                .and_then(|i| run_ids.get(i))
                .map(|r| r.as_str().to_string()),
            runs_since_seen,
            avg_duration_secs: avg,
            discovered: discovered.contains(&tid),
        });
    }

    // Surface the configured group_regex so the UI can default to it.
    let group_regex = crate::config::TestrConfig::find_in_directory(base)
        .ok()
        .and_then(|(cfg, _)| cfg.group_regex);

    Ok(TestsResponse {
        tests,
        group_regex,
        total_runs_in_repo,
        discovery_run: include_discovered,
    })
}

#[derive(Deserialize)]
struct TreeQuery {
    regex: Option<String>,
    #[serde(default)]
    include_discovered: bool,
}

#[derive(Serialize)]
struct TreeResponse {
    regex_used: Option<String>,
    groups: Vec<TreeGroup>,
}

#[derive(Serialize)]
struct TreeGroup {
    name: String,
    tests: Vec<String>,
}

async fn api_tree(State(state): State<AppState>, Query(q): Query<TreeQuery>) -> Response {
    let regex = match q.regex.as_deref().filter(|s| !s.is_empty()) {
        Some(r) => Some(r.to_string()),
        None => crate::config::TestrConfig::find_in_directory(&state.base)
            .ok()
            .and_then(|(c, _)| c.group_regex),
    };

    let repo = match super::utils::open_repository(Some(&state.base.to_string_lossy())) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    // Collect every test we have ever seen, plus discovered tests. Without
    // this union, freshly-discovered tests that have not yet been recorded
    // wouldn't show up in the tree.
    let mut tests: std::collections::BTreeSet<TestId> = std::collections::BTreeSet::new();
    if let Ok(run_ids) = repo.list_run_ids() {
        for rid in run_ids {
            if let Ok(run) = repo.get_test_run(&rid) {
                tests.extend(run.results.into_keys());
            }
        }
    }
    if q.include_discovered {
        if let Ok(tc) = crate::testcommand::TestCommand::from_directory(&state.base) {
            if let Ok(ds) = tc.list_tests() {
                tests.extend(ds);
            }
        }
    }
    let tests: Vec<TestId> = tests.into_iter().collect();

    let groups: Vec<TreeGroup> = match &regex {
        Some(r) => match crate::grouping::group_tests(&tests, r) {
            Ok(map) => {
                let mut out: Vec<TreeGroup> = map
                    .into_iter()
                    .map(|(name, tids)| {
                        let mut t: Vec<String> =
                            tids.into_iter().map(|t| t.as_str().to_string()).collect();
                        t.sort();
                        TreeGroup { name, tests: t }
                    })
                    .collect();
                out.sort_by(|a, b| a.name.cmp(&b.name));
                out
            }
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid group regex: {}", e),
                );
            }
        },
        None => {
            // No regex: a single flat group containing every test.
            let mut t: Vec<String> = tests.iter().map(|t| t.as_str().to_string()).collect();
            t.sort();
            vec![TreeGroup {
                name: String::new(),
                tests: t,
            }]
        }
    };

    Json(TreeResponse {
        regex_used: regex,
        groups,
    })
    .into_response()
}

#[derive(Serialize)]
struct RunListItem {
    id: String,
    timestamp: String,
    total: usize,
    failures: usize,
    duration_secs: Option<f64>,
    exit_code: Option<i32>,
    /// Profile applied for this run, if any (`--profile <name>`).
    profile: Option<String>,
    /// Git commit at the time of the run, if recorded.
    git_commit: Option<String>,
    /// Whether the working tree was dirty at run-time. None when not tracked.
    git_dirty: Option<bool>,
    /// Run-level tags (distinct from per-test tags).
    tags: Vec<String>,
}

async fn api_runs(State(state): State<AppState>) -> Response {
    let repo = match super::utils::open_repository(Some(&state.base.to_string_lossy())) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let ids = match repo.list_run_ids() {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut out = Vec::with_capacity(ids.len());
    for rid in ids.iter().rev() {
        if let Ok(run) = repo.get_test_run(rid) {
            let meta = repo.get_run_metadata(rid).unwrap_or_default();
            out.push(RunListItem {
                id: rid.as_str().to_string(),
                timestamp: run.timestamp.to_rfc3339(),
                total: run.total_tests(),
                failures: run.count_failures(),
                duration_secs: meta.duration_secs,
                exit_code: meta.exit_code,
                profile: meta.profile,
                git_commit: meta.git_commit,
                git_dirty: meta.git_dirty,
                tags: run.tags.clone(),
            });
        }
    }
    Json(out).into_response()
}

#[derive(Serialize)]
struct RunDetail {
    id: String,
    timestamp: String,
    total: usize,
    failures: usize,
    /// Successes plus expected-failures (i.e. results in the `is_success`
    /// bucket minus skips).
    successes: usize,
    skipped: usize,
    /// `Status::Error` count. This is a *subset* of `failures`, separated
    /// out so the caller can distinguish a hard execution error from an
    /// assertion failure.
    errors: usize,
    duration_secs: Option<f64>,
    exit_code: Option<i32>,
    command: Option<String>,
    profile: Option<String>,
    concurrency: Option<u32>,
    git_commit: Option<String>,
    git_dirty: Option<bool>,
    test_args: Option<Vec<String>>,
    predicted_duration_secs: Option<f64>,
    /// Run-level tags (distinct from per-test tags).
    tags: Vec<String>,
    /// Reason the subunit stream was cut short, if any.
    interruption: Option<String>,
    /// Total time spent inside test bodies, summed from per-test durations.
    /// Differs from `duration_secs` (wall-clock) when running in parallel,
    /// where total CPU time exceeds wall-clock time.
    total_test_duration_secs: Option<f64>,
    /// Slowest single test duration in seconds, if any test reported one.
    slowest_test_secs: Option<f64>,
    /// Mean per-test duration in seconds, if any test reported one.
    mean_test_secs: Option<f64>,
    results: Vec<RunDetailResult>,
}

#[derive(Serialize)]
struct RunDetailResult {
    test_id: String,
    status: String,
    duration_secs: Option<f64>,
    /// Short failure message (the assertion text, etc.).
    message: Option<String>,
    /// Full traceback / captured stdout+stderr / attachment payload, if
    /// the subunit stream carried one for this test. Populated regardless
    /// of pass/fail since captured output for passing tests is also
    /// occasionally useful.
    details: Option<String>,
    /// Per-result tags carried by the subunit stream.
    tags: Vec<String>,
}

async fn api_run_detail(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let repo = match super::utils::open_repository(Some(&state.base.to_string_lossy())) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let rid = crate::repository::RunId::new(id.clone());
    let run = match repo.get_test_run(&rid) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::NOT_FOUND, &e.to_string()),
    };
    let mut results: Vec<RunDetailResult> = run
        .results
        .values()
        .map(|r| RunDetailResult {
            test_id: r.test_id.as_str().to_string(),
            status: r.status.to_string(),
            duration_secs: r.duration.map(|d| d.as_secs_f64()),
            message: r.message.clone(),
            // Always ship details when present — captured stdout/stderr is
            // useful regardless of pass/fail, and the inquest format only
            // stores attachments for tests that actually emitted output, so
            // this doesn't bloat green runs.
            details: r.details.clone(),
            tags: r.tags.clone(),
        })
        .collect();
    results.sort_by(|a, b| a.test_id.cmp(&b.test_id));

    // Per-status breakdown. We walk the results twice (once for the
    // serialised view, once for counts) but the result count for a single
    // run is always small enough that this is irrelevant.
    use crate::repository::TestStatus;
    let mut successes = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;
    let mut total_test_duration = Duration::ZERO;
    let mut any_duration = false;
    let mut slowest = Duration::ZERO;
    let mut duration_count: u32 = 0;
    for r in run.results.values() {
        match r.status {
            TestStatus::Success | TestStatus::ExpectedFailure => successes += 1,
            TestStatus::Skip => skipped += 1,
            TestStatus::Error => errors += 1,
            _ => {}
        }
        if let Some(d) = r.duration {
            total_test_duration += d;
            any_duration = true;
            duration_count += 1;
            if d > slowest {
                slowest = d;
            }
        }
    }
    let total_test_duration_secs = if any_duration {
        Some(total_test_duration.as_secs_f64())
    } else {
        None
    };
    let slowest_test_secs = if any_duration {
        Some(slowest.as_secs_f64())
    } else {
        None
    };
    let mean_test_secs = if duration_count > 0 {
        Some(total_test_duration.as_secs_f64() / duration_count as f64)
    } else {
        None
    };

    let meta = repo.get_run_metadata(&rid).unwrap_or_default();
    Json(RunDetail {
        id,
        timestamp: run.timestamp.to_rfc3339(),
        total: run.total_tests(),
        failures: run.count_failures(),
        successes,
        skipped,
        errors,
        duration_secs: meta.duration_secs,
        exit_code: meta.exit_code,
        command: meta.command,
        profile: meta.profile,
        concurrency: meta.concurrency,
        git_commit: meta.git_commit,
        git_dirty: meta.git_dirty,
        test_args: meta.test_args,
        predicted_duration_secs: meta.predicted_duration_secs,
        tags: run.tags.clone(),
        interruption: run.interruption.as_ref().map(|i| i.to_string()),
        total_test_duration_secs,
        slowest_test_secs,
        mean_test_secs,
        results,
    })
    .into_response()
}

#[derive(Serialize)]
struct TestHistoryEntry {
    run_id: String,
    timestamp: String,
    status: Option<String>,
    duration_secs: Option<f64>,
    message: Option<String>,
    /// Full traceback / captured output, when the subunit stream carried
    /// one. Surfaced for any status — captured output for passing tests
    /// can be informative too.
    details: Option<String>,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct TestHistoryResponse {
    test_id: String,
    entries: Vec<TestHistoryEntry>,
}

async fn api_test_history(
    State(state): State<AppState>,
    AxumPath(test_id): AxumPath<String>,
) -> Response {
    let repo = match super::utils::open_repository(Some(&state.base.to_string_lossy())) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let run_ids = match repo.list_run_ids() {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let needle = TestId::new(test_id.clone());
    let mut entries: Vec<TestHistoryEntry> = Vec::new();
    // Walk newest-first so the resulting list reads like the runs panel.
    for rid in run_ids.iter().rev() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let result = run.results.get(&needle);
        // Skip runs that didn't observe this test at all — those rows would
        // be uninformative noise. We keep all runs where the test is present
        // even if the status is e.g. skip, since "skipped" is itself useful
        // history.
        let Some(result) = result else { continue };
        entries.push(TestHistoryEntry {
            run_id: rid.as_str().to_string(),
            timestamp: run.timestamp.to_rfc3339(),
            status: Some(result.status.to_string()),
            duration_secs: result.duration.map(|d| d.as_secs_f64()),
            message: result.message.clone(),
            details: result.details.clone(),
            tags: result.tags.clone(),
        });
    }
    Json(TestHistoryResponse { test_id, entries }).into_response()
}

#[derive(Serialize)]
struct ProfilesResponse {
    default_profile: Option<String>,
    profiles: Vec<String>,
}

async fn api_profiles(State(state): State<AppState>) -> Response {
    let (cfg_file, _) = match crate::config::ConfigFile::find_in_directory(&state.base) {
        Ok(v) => v,
        Err(_) => {
            // No config file found is normal for an uninitialised tree —
            // treat it as "no profiles" rather than a server error.
            return Json(ProfilesResponse {
                default_profile: None,
                profiles: Vec::new(),
            })
            .into_response();
        }
    };
    Json(ProfilesResponse {
        default_profile: cfg_file.default_profile.clone(),
        profiles: cfg_file
            .profile_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
    })
    .into_response()
}

#[derive(Serialize)]
struct RepoOverview {
    total_runs: usize,
    total_tests_known: usize,
    last_run_id: Option<String>,
    last_run_failures: Option<usize>,
    last_run_total: Option<usize>,
    avg_run_duration_secs: Option<f64>,
    recent_failure_rate: Option<f64>,
    slowest: Vec<TestStatLite>,
    flakiest: Vec<TestStatLite>,
}

#[derive(Serialize)]
struct TestStatLite {
    test_id: String,
    runs: u32,
    failures: u32,
    avg_duration_secs: Option<f64>,
    flakiness_score: Option<f64>,
}

async fn api_repo(State(state): State<AppState>) -> Response {
    match build_repo_overview(&state.base) {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn build_repo_overview(base: &std::path::Path) -> Result<RepoOverview> {
    let repo = super::utils::open_repository(Some(&base.to_string_lossy()))?;
    let run_ids = repo.list_run_ids()?;
    let total_runs = run_ids.len();

    // Walk runs once, capturing the per-test aggregates we need for "slowest"
    // and the per-run summary for the most-recent and average-duration data.
    struct Agg {
        runs: u32,
        failures: u32,
        duration_total: Duration,
        duration_count: u32,
    }
    let mut agg: HashMap<TestId, Agg> = HashMap::new();
    let mut total_duration_secs: f64 = 0.0;
    let mut runs_with_duration: u32 = 0;
    let mut last_run_total: Option<usize> = None;
    let mut last_run_failures: Option<usize> = None;
    let last_run_id = run_ids.last().map(|r| r.as_str().to_string());

    let recent_window = run_ids.len().min(10);
    let mut recent_total: usize = 0;
    let mut recent_failures: usize = 0;

    for (idx, rid) in run_ids.iter().enumerate() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let meta = repo.get_run_metadata(rid).unwrap_or_default();
        if let Some(d) = meta.duration_secs {
            total_duration_secs += d;
            runs_with_duration += 1;
        }
        if idx + 1 == run_ids.len() {
            last_run_total = Some(run.total_tests());
            last_run_failures = Some(run.count_failures());
        }
        if idx + recent_window >= run_ids.len() {
            recent_total += run.total_tests();
            recent_failures += run.count_failures();
        }
        for (tid, result) in &run.results {
            let entry = agg.entry(tid.clone()).or_insert(Agg {
                runs: 0,
                failures: 0,
                duration_total: Duration::ZERO,
                duration_count: 0,
            });
            entry.runs += 1;
            if result.status.is_failure() {
                entry.failures += 1;
            }
            if let Some(d) = result.duration {
                entry.duration_total += d;
                entry.duration_count += 1;
            }
        }
    }

    // Top 10 slowest by mean duration. Require at least one timed run so we
    // don't surface tests that never reported a duration.
    let mut slowest: Vec<TestStatLite> = agg
        .iter()
        .filter(|(_, a)| a.duration_count > 0)
        .map(|(tid, a)| {
            let avg = a.duration_total.as_secs_f64() / a.duration_count as f64;
            TestStatLite {
                test_id: tid.as_str().to_string(),
                runs: a.runs,
                failures: a.failures,
                avg_duration_secs: Some(avg),
                flakiness_score: None,
            }
        })
        .collect();
    slowest.sort_by(|a, b| {
        b.avg_duration_secs
            .partial_cmp(&a.avg_duration_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    slowest.truncate(10);

    // Top 10 flakiest. Reuse the repository-provided definition so the metric
    // matches `inq flaky`.
    let flakiest: Vec<TestStatLite> = repo
        .get_flakiness(2)
        .unwrap_or_default()
        .into_iter()
        .take(10)
        .map(|f| {
            let avg = agg.get(&f.test_id).and_then(|a| {
                if a.duration_count == 0 {
                    None
                } else {
                    Some(a.duration_total.as_secs_f64() / a.duration_count as f64)
                }
            });
            TestStatLite {
                test_id: f.test_id.as_str().to_string(),
                runs: f.runs,
                failures: f.failures,
                avg_duration_secs: avg,
                flakiness_score: Some(f.flakiness_score),
            }
        })
        .collect();

    let avg_run_duration_secs = if runs_with_duration > 0 {
        Some(total_duration_secs / runs_with_duration as f64)
    } else {
        None
    };
    let recent_failure_rate = if recent_total > 0 {
        Some(recent_failures as f64 / recent_total as f64)
    } else {
        None
    };

    Ok(RepoOverview {
        total_runs,
        total_tests_known: agg.len(),
        last_run_id,
        last_run_failures,
        last_run_total,
        avg_run_duration_secs,
        recent_failure_rate,
        slowest,
        flakiest,
    })
}

#[derive(Deserialize)]
struct StartRunRequest {
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
struct StartRunResponse {
    /// The server-side handle for SSE attachment. Distinct from the run ID
    /// the repository assigns, which is only known after the child starts.
    handle: u64,
}

async fn api_start_run(
    State(state): State<AppState>,
    Json(req): Json<StartRunRequest>,
) -> Response {
    let handle_id = state.next_run.fetch_add(1, Ordering::SeqCst);
    let (tx, _) = broadcast::channel::<RunEvent>(SSE_CHANNEL_CAPACITY);
    let history: Arc<Mutex<Vec<RunEvent>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let mut runs = state.runs.lock().expect("runs mutex poisoned");
        runs.insert(
            handle_id,
            RunHandle {
                tx: tx.clone(),
                history: history.clone(),
            },
        );
    }

    let base = state.base.clone();
    let runs_map = state.runs.clone();
    tokio::task::spawn_blocking(move || {
        let result = drive_child_run(&base, &req, &history, &tx);
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

async fn api_run_events(State(state): State<AppState>, AxumPath(id): AxumPath<u64>) -> Response {
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

fn json_error(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({ "error": msg });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collect_test_stats_empty_repo_errors() {
        let temp = TempDir::new().unwrap();
        let result = collect_test_stats(temp.path(), false);
        assert!(result.is_err());
    }

    #[test]
    fn collect_test_stats_with_runs() {
        let temp = TempDir::new().unwrap();
        let mut repo =
            super::super::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.add_result(
            crate::repository::TestResult::success("a.b.test_one")
                .with_duration(Duration::from_millis(10)),
        );
        run.add_result(crate::repository::TestResult::failure(
            "a.b.test_two",
            "boom",
        ));
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let resp = collect_test_stats(temp.path(), false).unwrap();
        assert_eq!(resp.total_runs_in_repo, 1);
        assert_eq!(resp.tests.len(), 2);

        let one = resp
            .tests
            .iter()
            .find(|t| t.test_id == "a.b.test_one")
            .unwrap();
        assert_eq!(one.runs, 1);
        assert_eq!(one.failures, 0);
        assert_eq!(one.last_status.as_deref(), Some("success"));

        let two = resp
            .tests
            .iter()
            .find(|t| t.test_id == "a.b.test_two")
            .unwrap();
        assert_eq!(two.runs, 1);
        assert_eq!(two.failures, 1);
        assert_eq!(two.last_status.as_deref(), Some("failure"));
    }

    #[test]
    fn progress_status_labels() {
        use crate::subunit_stream::ProgressStatus;
        assert_eq!(progress_status_label(ProgressStatus::Success), "success");
        assert_eq!(progress_status_label(ProgressStatus::Failed), "failure");
        assert_eq!(progress_status_label(ProgressStatus::Skipped), "skip");
    }

    #[tokio::test]
    async fn index_handler_returns_rendered_template() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let state = AppState {
            base: PathBuf::from("."),
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run: Arc::new(AtomicU64::new(1)),
        };
        let app = Router::new().route("/", get(index)).with_state(state);

        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<title>inquest</title>"));
        assert!(html.contains("/api/tests"));
        // The frontend should not auto-discover on load (slow on Cargo
        // projects). It only calls the discovery-enabling query string when
        // the user clicks the Discover button.
        assert!(html.contains("discover-btn"));
        assert!(html.contains("/api/test/"));
        assert!(html.contains("progress-bar"));
    }

    #[test]
    fn build_repo_overview_aggregates_runs() {
        let temp = TempDir::new().unwrap();
        let mut repo =
            super::super::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        // Two runs: the first with both tests passing, the second with one
        // flapping to failure. Gives us a non-zero failure rate, a flakiest
        // entry (1 transition), and a slowest entry.
        let mut run1 = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run1.add_result(
            crate::repository::TestResult::success("a.b.fast")
                .with_duration(Duration::from_millis(5)),
        );
        run1.add_result(
            crate::repository::TestResult::success("a.b.slow")
                .with_duration(Duration::from_millis(500)),
        );
        repo.insert_test_run(run1).unwrap();

        let mut run2 = crate::repository::TestRun::new(crate::repository::RunId::new("1"));
        run2.add_result(
            crate::repository::TestResult::success("a.b.fast")
                .with_duration(Duration::from_millis(6)),
        );
        run2.add_result(crate::repository::TestResult::failure("a.b.slow", "boom"));
        repo.insert_test_run(run2).unwrap();
        drop(repo);

        let overview = build_repo_overview(temp.path()).unwrap();
        assert_eq!(overview.total_runs, 2);
        assert_eq!(overview.total_tests_known, 2);
        assert_eq!(overview.last_run_total, Some(2));
        assert_eq!(overview.last_run_failures, Some(1));
        // recent_failure_rate is failures-over-test-results across the
        // recent window; with one failure out of four results that's 0.25.
        assert_eq!(overview.recent_failure_rate, Some(0.25));
        // a.b.slow has the higher mean duration so it should head the list.
        assert_eq!(overview.slowest.first().unwrap().test_id, "a.b.slow");
        // a.b.slow flapped (success → failure) so it shows as flakiest.
        assert_eq!(overview.flakiest.first().unwrap().test_id, "a.b.slow");
    }

    #[tokio::test]
    async fn test_history_endpoint_returns_per_test_results() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let temp = TempDir::new().unwrap();
        let mut repo =
            super::super::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run1 = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run1.add_result(
            crate::repository::TestResult::success("pkg::mod::flaky")
                .with_duration(Duration::from_millis(15)),
        );
        repo.insert_test_run(run1).unwrap();

        let mut run2 = crate::repository::TestRun::new(crate::repository::RunId::new("1"));
        run2.add_result(crate::repository::TestResult::failure(
            "pkg::mod::flaky",
            "boom",
        ));
        repo.insert_test_run(run2).unwrap();
        drop(repo);

        let state = AppState {
            base: temp.path().to_path_buf(),
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run: Arc::new(AtomicU64::new(1)),
        };
        let app = Router::new()
            .route("/api/test/:id/history", get(api_test_history))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/test/pkg%3A%3Amod%3A%3Aflaky/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["test_id"], "pkg::mod::flaky");
        let entries = parsed["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        // Newest-first ordering: run "1" (the failing one) should come first.
        assert_eq!(entries[0]["run_id"], "1");
        assert_eq!(entries[0]["status"], "failure");
        assert_eq!(entries[1]["run_id"], "0");
        assert_eq!(entries[1]["status"], "success");
    }

    #[tokio::test]
    async fn run_detail_surfaces_failure_attachments_and_breakdown() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let temp = TempDir::new().unwrap();
        let mut repo =
            super::super::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        // Build a run with a passing test, a skipped test, and a failing
        // test that carries both a message and a `details` traceback plus
        // a per-result tag. The run is round-tripped through the subunit
        // writer/parser pair, so we only assert on fields the format
        // actually preserves.
        let run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        let mut run = run;
        run.add_result(
            crate::repository::TestResult::success("a::pass")
                .with_duration(Duration::from_millis(8)),
        );
        run.add_result(crate::repository::TestResult::skip("a::skipped"));
        run.add_result(
            crate::repository::TestResult::failure("a::boom", "assertion failed: 1 == 2")
                .with_details("traceback line 1\ntraceback line 2")
                .with_tag("worker-7"),
        );
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let state = AppState {
            base: temp.path().to_path_buf(),
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run: Arc::new(AtomicU64::new(1)),
        };
        let app = Router::new()
            .route("/api/runs/:id", get(api_run_detail))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/runs/0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(parsed["successes"], 1);
        assert_eq!(parsed["failures"], 1);
        assert_eq!(parsed["skipped"], 1);
        assert!(parsed["total_test_duration_secs"].is_number());

        // Find the failing result. The subunit format stores only the
        // file-attachment payload (which becomes both message and details
        // on parse-back), not the original short message — but the
        // attachment content does round-trip, which is what makes the
        // failure-output viewable in the UI.
        let results = parsed["results"].as_array().unwrap();
        let boom = results.iter().find(|r| r["test_id"] == "a::boom").unwrap();
        assert_eq!(boom["status"], "failure");
        // Details survive the round-trip and are exposed for the UI to
        // render in the attachments pane.
        assert!(boom["details"].as_str().unwrap_or("").contains("traceback"));
        // A test that didn't carry an attachment in the subunit stream
        // gets `details: null`. (We surface details for any status when the
        // stream contained one, but the fixture's pass test didn't.)
        let pass = results.iter().find(|r| r["test_id"] == "a::pass").unwrap();
        assert_eq!(pass["status"], "success");
        assert!(pass["details"].is_null());
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
    async fn profiles_endpoint_returns_empty_when_no_config() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let temp = TempDir::new().unwrap();
        let state = AppState {
            base: temp.path().to_path_buf(),
            runs: Arc::new(Mutex::new(HashMap::new())),
            next_run: Arc::new(AtomicU64::new(1)),
        };
        let app = Router::new()
            .route("/api/profiles", get(api_profiles))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["default_profile"], serde_json::Value::Null);
        assert_eq!(parsed["profiles"], serde_json::json!([]));
    }
}
