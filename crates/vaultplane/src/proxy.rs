//! OpenAI-compatible proxy API.
//!
//! Requests are authenticated against the virtual key store (when keys are
//! configured) before routing. `POST /v1/chat/completions` enforces the key's model
//! scope, then forwards to the provider and streams the response back without
//! buffering. `/v1/embeddings` and `/v1/models` are stubs that return 501.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use vaultplane_core::auth::{KeyStore, VirtualKey};
use vaultplane_core::provider::{ChatRequest, Connector};

/// Shared state for the proxy API.
#[derive(Clone)]
struct ProxyState {
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
}

/// Build the proxy API router around a provider connector and key store.
pub fn router(connector: Arc<dyn Connector>, keys: Arc<KeyStore>) -> Router {
    let state = ProxyState { connector, keys };
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(not_implemented))
        .route("/v1/models", get(not_implemented))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

/// Authenticate a request against the virtual key store and attach the resolved key.
///
/// When no keys are configured the proxy is open and an anonymous key (which allows
/// any model) is attached so downstream handlers have a uniform key to read.
async fn authenticate(
    State(state): State<ProxyState>,
    mut request: Request,
    next: Next,
) -> Response {
    let key = if state.keys.is_empty() {
        VirtualKey::anonymous()
    } else {
        match crate::bearer_token(request.headers())
            .and_then(|token| state.keys.authenticate(token).cloned())
        {
            Some(key) => key,
            None => return error(StatusCode::UNAUTHORIZED, "missing or invalid virtual key"),
        }
    };
    request.extensions_mut().insert(key);
    next.run(request).await
}

/// The fields of a chat request the gateway reads before forwarding the rest.
#[derive(Debug, Default, Deserialize)]
struct ChatMeta {
    model: Option<String>,
    #[serde(default)]
    stream: bool,
}

async fn chat_completions(
    State(state): State<ProxyState>,
    Extension(key): Extension<VirtualKey>,
    body: Bytes,
) -> Response {
    let meta: ChatMeta = serde_json::from_slice(&body).unwrap_or_default();
    let model = meta.model.unwrap_or_default();

    if !key.allows_model(&model) {
        return error(
            StatusCode::FORBIDDEN,
            "virtual key is not allowed to use this model",
        );
    }

    let request = ChatRequest {
        model,
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
    use vaultplane_core::auth::{KeyStore, VirtualKey};
    use vaultplane_core::provider::Connector;
    use vaultplane_core::provider::openai::OpenAiConnector;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn connector(base_url: &str) -> Arc<dyn Connector> {
        Arc::new(OpenAiConnector::new(base_url, "test-key").unwrap())
    }

    fn open() -> Arc<KeyStore> {
        Arc::new(KeyStore::default())
    }

    fn store_with_key(models: &[&str]) -> Arc<KeyStore> {
        let mut key = VirtualKey::anonymous();
        key.token = "vp_test".to_string();
        key.models = models.iter().map(|m| m.to_string()).collect();
        Arc::new(KeyStore::new(vec![key]))
    }

    fn chat_request(token: Option<&str>, model: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::from(format!(r#"{{"model":"{model}"}}"#)))
            .unwrap()
    }

    #[tokio::test]
    async fn chat_completions_proxies_to_upstream_when_open() {
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

        let app = router(connector(&server.uri()), open());
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("chat.completion"));
    }

    #[tokio::test]
    async fn chat_completions_requires_a_valid_key_when_configured() {
        let app = router(connector("http://127.0.0.1:1"), store_with_key(&["gpt-4o"]));
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_completions_rejects_a_disallowed_model() {
        let app = router(connector("http://127.0.0.1:1"), store_with_key(&["gpt-4o"]));
        let response = app
            .oneshot(chat_request(Some("vp_test"), "gpt-3.5"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn chat_completions_allows_an_allowed_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let app = router(connector(&server.uri()), store_with_key(&["gpt-4o"]));
        let response = app
            .oneshot(chat_request(Some("vp_test"), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn embeddings_is_not_implemented() {
        let app = router(connector("http://127.0.0.1:1"), open());
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
        let app = router(connector("http://127.0.0.1:1"), open());
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
