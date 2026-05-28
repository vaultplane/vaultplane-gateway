//! OpenAI-compatible proxy API.
//!
//! Each request is authenticated against the virtual key store, dispatched through
//! the provider connector (typically the model registry), and recorded as a tracing
//! span with OpenTelemetry GenAI semantic-convention attributes plus VaultPlane
//! attributes (virtual key id, team, app, env, attempts, cost, status, duration).

use std::sync::Arc;
use std::time::Instant;

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
use vaultplane_core::config::Pricing;
use vaultplane_core::provider::{ChatRequest, Connector, Usage};

/// Shared state for the proxy API.
#[derive(Clone)]
struct ProxyState {
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
}

/// Build the proxy API router around a provider connector, key store, and pricing.
pub fn router(connector: Arc<dyn Connector>, keys: Arc<KeyStore>, pricing: Arc<Pricing>) -> Router {
    let state = ProxyState {
        connector,
        keys,
        pricing,
    };
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

/// Compute the request cost in USD from upstream usage and the pricing table.
fn compute_cost(pricing: &Pricing, provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    let model_pricing = pricing.providers.get(provider)?.get(model)?;
    let input = (usage.prompt_tokens as f64) / 1000.0 * model_pricing.input_per_1k_tokens_usd;
    let output = (usage.completion_tokens as f64) / 1000.0 * model_pricing.output_per_1k_tokens_usd;
    Some(input + output)
}

async fn chat_completions(
    State(state): State<ProxyState>,
    Extension(key): Extension<VirtualKey>,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let meta: ChatMeta = serde_json::from_slice(&body).unwrap_or_default();
    let virtual_model = meta.model.unwrap_or_default();

    let span = tracing::info_span!(
        "chat",
        "gen_ai.system" = tracing::field::Empty,
        "gen_ai.request.model" = %virtual_model,
        "gen_ai.response.model" = tracing::field::Empty,
        "gen_ai.usage.input_tokens" = tracing::field::Empty,
        "gen_ai.usage.output_tokens" = tracing::field::Empty,
        "vaultplane.virtual_key.id" = %key.id(),
        "vaultplane.team" = %key.team,
        "vaultplane.app" = %key.app,
        "vaultplane.env" = %key.env,
        "vaultplane.provider.attempts" = tracing::field::Empty,
        "vaultplane.cost_usd" = tracing::field::Empty,
        "http.response.status_code" = tracing::field::Empty,
        "duration_ms" = tracing::field::Empty,
    );

    if !key.allows_model(&virtual_model) {
        span.record("http.response.status_code", 403_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::FORBIDDEN,
            "virtual key is not allowed to use this model",
        );
    }

    let request = ChatRequest {
        model: virtual_model,
        stream: meta.stream,
        body,
    };

    let result = state.connector.chat(request).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(upstream) => {
            span.record("gen_ai.system", upstream.provider.as_str());
            span.record("gen_ai.response.model", upstream.model.as_str());
            span.record("vaultplane.provider.attempts", upstream.attempts as u64);
            span.record("http.response.status_code", upstream.status as u64);
            if let Some(usage) = &upstream.usage {
                span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as u64);
                span.record("gen_ai.usage.output_tokens", usage.completion_tokens as u64);
                if let Some(cost) =
                    compute_cost(&state.pricing, &upstream.provider, &upstream.model, usage)
                {
                    span.record("vaultplane.cost_usd", cost);
                }
            }
            span.record("duration_ms", duration_ms);

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
            span.record("http.response.status_code", 502_u64);
            span.record("duration_ms", duration_ms);
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
    use super::{compute_cost, router};
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;
    use vaultplane_core::auth::{KeyStore, VirtualKey};
    use vaultplane_core::config::{ModelPricing, Pricing};
    use vaultplane_core::provider::Connector;
    use vaultplane_core::provider::Usage;
    use vaultplane_core::provider::openai::OpenAiConnector;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn connector(base_url: &str) -> Arc<dyn Connector> {
        Arc::new(OpenAiConnector::new(base_url, "test-key").unwrap())
    }

    fn open_keys() -> Arc<KeyStore> {
        Arc::new(KeyStore::default())
    }

    fn open_pricing() -> Arc<Pricing> {
        Arc::new(Pricing::default())
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

    #[test]
    fn cost_is_computed_from_usage_and_pricing() {
        let mut pricing = Pricing::default();
        let mut openai = std::collections::HashMap::new();
        openai.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 2.5,
                output_per_1k_tokens_usd: 10.0,
            },
        );
        pricing.providers.insert("openai".to_string(), openai);

        let usage = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
        };
        let cost = compute_cost(&pricing, "openai", "gpt-4o", &usage).unwrap();
        // 1000/1000 * 2.5 + 500/1000 * 10.0 = 2.5 + 5.0 = 7.5
        assert!((cost - 7.5).abs() < 1e-9);

        // Unknown provider or model returns None.
        assert!(compute_cost(&pricing, "openai", "unknown", &usage).is_none());
        assert!(compute_cost(&pricing, "anthropic", "gpt-4o", &usage).is_none());
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

        let app = router(connector(&server.uri()), open_keys(), open_pricing());
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("chat.completion"));
    }

    #[tokio::test]
    async fn chat_completions_requires_a_valid_key_when_configured() {
        let app = router(
            connector("http://127.0.0.1:1"),
            store_with_key(&["gpt-4o"]),
            open_pricing(),
        );
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_completions_rejects_a_disallowed_model() {
        let app = router(
            connector("http://127.0.0.1:1"),
            store_with_key(&["gpt-4o"]),
            open_pricing(),
        );
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

        let app = router(
            connector(&server.uri()),
            store_with_key(&["gpt-4o"]),
            open_pricing(),
        );
        let response = app
            .oneshot(chat_request(Some("vp_test"), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn embeddings_is_not_implemented() {
        let app = router(connector("http://127.0.0.1:1"), open_keys(), open_pricing());
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
        let app = router(connector("http://127.0.0.1:1"), open_keys(), open_pricing());
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
