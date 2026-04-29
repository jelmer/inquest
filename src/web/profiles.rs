//! `GET /api/profiles` — surface the project's profile list and the
//! configured default profile, when an `inquest.toml` is present.

use super::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct ProfilesResponse {
    default_profile: Option<String>,
    profiles: Vec<String>,
}

pub(super) async fn api_profiles(State(state): State<AppState>) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

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
