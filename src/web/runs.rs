//! `GET /api/runs` and `GET /api/runs/:id` — list runs and surface
//! per-run metadata + per-test results.

use super::{json_error, AppState};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::time::Duration;

#[derive(Serialize)]
pub(super) struct RunListItem {
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

pub(super) async fn api_runs(State(state): State<AppState>) -> Response {
    let repo = match crate::commands::utils::open_repository(Some(&state.base.to_string_lossy())) {
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
pub(super) struct RunDetail {
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
pub(super) struct RunDetailResult {
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

pub(super) async fn api_run_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let repo = match crate::commands::utils::open_repository(Some(&state.base.to_string_lossy())) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[tokio::test]
    async fn run_detail_surfaces_failure_attachments_and_breakdown() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let temp = TempDir::new().unwrap();
        let mut repo =
            crate::commands::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

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
}
