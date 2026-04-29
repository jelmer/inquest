//! `GET /` — render the single-page UI.

use super::json_error;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

#[derive(askama::Template)]
#[template(path = "web_index.html")]
struct IndexTemplate;

pub(super) async fn index() -> Response {
    use askama::Template;
    match IndexTemplate.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("template render error: {}", e),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::AppState;
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

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
}
