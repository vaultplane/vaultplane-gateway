//! OpenAI-compatible proxy API.
//!
//! `POST /v1/chat/completions` is forwarded to the configured provider and the
//! response is streamed back to the client, so both buffered and streamed (SSE)
//! replies pass through without buffering. `/v1/embeddings` and `/v1/models` are
//! stubs that return 501 until they are implemented.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use vaultplane_core::provider::{ChatRequest, Connector};

/// Shared state for the proxy API.
#[derive(Clone)]
struct ProxyState {
    connector: Arc<dyn Connector>,
}

/// Build the proxy API router around a provider connector.
pub fn router(connector: Arc<dyn Connector>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(not_implemented))
        .route("/v1/models", get(not_implemented))
        .fallback(fallback)
        .with_state(ProxyState { connector })
}

/// The fields of a chat request the gateway reads before forwarding the rest.
#[derive(Debug, Default, Deserialize)]
struct ChatMeta {
    model: Option<String>,
    #[serde(default)]
    stream: bool,
}

async fn chat_completions(State(state): State<ProxyState>, body: Bytes) -> Response {
    let meta: ChatMeta = serde_json::from_slice(&body).unwrap_or_default();
    let request = ChatRequest {
        model: meta.model.unwrap_or_default(),
        stream: meta.stream,
        body,
    };

    match state.connector.chat(request).await {
        Ok(upstream) => {
            let mut builder = Response::builder().status(upstream.status);
            if let Some(content_type) = upstream.content_type {
                builder = builder.header(CONTENT_TYPE, content_type);
            }
            builder
                .body(Body::from_stream(upstream.body))
                .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "invalid upstream response"))
        }
        Err(err) => {
            tracing::warn!(error = %err, "upstream chat completion failed");
            error(StatusCode::BAD_GATEWAY, "upstream request failed")
        }
    }
}

async fn not_implemented() -> Response {
    error(StatusCode::NOT_IMPLEMENTED, "not yet implemented")
}

async fn fallback() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

/// Build a small JSON error response in the OpenAI error shape.
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;
    use vaultplane_core::provider::Connector;
    use vaultplane_core::provider::openai::OpenAiConnector;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn connector(base_url: &str) -> Arc<dyn Connector> {
        Arc::new(OpenAiConnector::new(base_url, "test-key").unwrap())
    }

    #[tokio::test]
    async fn chat_completions_proxies_to_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"object":"chat.completion"}"#),
            )
            .mount(&server)
            .await;

        let app = router(connector(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"gpt-4o"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("chat.completion"));
    }

    #[tokio::test]
    async fn embeddings_is_not_implemented() {
        let app = router(connector("http://127.0.0.1:1"));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/embeddings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn unknown_route_is_not_found() {
        let app = router(connector("http://127.0.0.1:1"));
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
