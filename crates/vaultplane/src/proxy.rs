//! OpenAI-compatible proxy API.
//!
//! The proxy binds to the main port (default 8080) and exposes the OpenAI REST
//! surface. The endpoints are stubs today; each returns 501 Not Implemented until
//! the provider connectors and routing land.

use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::json;

/// Build the proxy API router.
pub fn router() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(not_implemented))
        .route("/v1/embeddings", post(not_implemented))
        .route("/v1/models", get(not_implemented))
        .fallback(fallback)
}

async fn not_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": { "message": "not yet implemented", "type": "not_implemented" }
        })),
    )
}

async fn fallback() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": { "message": "not found", "type": "not_found" }
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn chat_completions_is_not_implemented() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn unknown_route_is_not_found() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
