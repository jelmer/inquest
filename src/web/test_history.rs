//! `GET /api/test/:id/history` — return every recorded result for a single
//! test across the repository's run history.

use super::{json_error, AppState};
use crate::repository::TestId;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct TestHistoryEntry {
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
pub(super) struct TestHistoryResponse {
    test_id: String,
    entries: Vec<TestHistoryEntry>,
}

pub(super) async fn api_test_history(
    State(state): State<AppState>,
    AxumPath(test_id): AxumPath<String>,
) -> Response {
    let repo = match crate::commands::utils::open_repository(Some(&state.base.to_string_lossy())) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_history_endpoint_returns_per_test_results() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let temp = TempDir::new().unwrap();
        let mut repo =
            crate::commands::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

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
}
