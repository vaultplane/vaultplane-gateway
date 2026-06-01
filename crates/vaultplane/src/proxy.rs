//! OpenAI-compatible proxy API.
//!
//! Each request is authenticated, rate-limited per key, optionally served from the
//! exact-match cache, and otherwise dispatched through the provider connector
//! (typically the model registry). Successful non-streaming responses are stored in
//! the cache before being returned. Each request is recorded as a tracing span with
//! OpenTelemetry GenAI semantic-convention attributes plus VaultPlane attributes
//! (virtual key id, team, app, env, attempts, cost, status, duration, cache hit).

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
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use vaultplane_core::auth::{KeyStore, RateLimiter, VirtualKey};
use vaultplane_core::cache::{CachedResponse, ResponseCache};
use vaultplane_core::config::Pricing;
use vaultplane_core::provider::{BodyStream, ChatRequest, Connector, Usage};

const CACHE_HEADER: &str = "x-vaultplane-cache";

/// Shared state for the proxy API.
#[derive(Clone)]
struct ProxyState {
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
    cache: Option<Arc<ResponseCache>>,
    rate_limiter: Arc<RateLimiter>,
}

/// Build the proxy API router around a provider connector, key store, pricing,
/// optional response cache, and a rate limiter.
pub fn router(
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
    cache: Option<Arc<ResponseCache>>,
    rate_limiter: Arc<RateLimiter>,
) -> Router {
    let state = ProxyState {
        connector,
        keys,
        pricing,
        cache,
        rate_limiter,
    };
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(not_implemented))
        .route("/v1/models", get(not_implemented))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

/// Authenticate the request against the virtual key store, then enforce the key's
/// rate limit (when configured). Anonymous traffic (no keys configured) is allowed
/// through unrestricted.
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

    if let Some(rps) = key.rate_limit_rps
        && !state.rate_limiter.check(&key.identifier(), rps)
    {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }

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

/// Drain a streaming body into a single contiguous buffer.
async fn collect_body(mut body: BodyStream) -> std::io::Result<Bytes> {
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(Bytes::from(buf))
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
        "vaultplane.virtual_key.id" = %key.identifier(),
        "vaultplane.team" = %key.team,
        "vaultplane.app" = %key.app,
        "vaultplane.env" = %key.env,
        "vaultplane.provider.attempts" = tracing::field::Empty,
        "vaultplane.cost_usd" = tracing::field::Empty,
        "vaultplane.cache.hit" = tracing::field::Empty,
        "http.response.status_code" = tracing::field::Empty,
        "duration_ms" = tracing::field::Empty,
    );

    if !key.allows_model(&virtual_model) {
        span.record("vaultplane.cache.hit", false);
        span.record("http.response.status_code", 403_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::FORBIDDEN,
            "virtual key is not allowed to use this model",
        );
    }

    // Compute the cache key for cacheable (non-streaming) requests.
    let cache_key =
        (!meta.stream).then(|| ResponseCache::key(&key.identifier(), &virtual_model, &body));

    // Cache lookup.
    if let (Some(cache), Some(key_str)) = (state.cache.as_ref(), cache_key.as_ref())
        && let Some(cached) = cache.get(key_str).await
    {
        return serve_from_cache(&state.pricing, &span, &cached, start);
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
            span.record("vaultplane.cache.hit", false);
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

            let should_cache =
                cache_key.is_some() && state.cache.is_some() && upstream.status == 200;

            if should_cache {
                // Drain the body so we can both cache it and return it.
                match collect_body(upstream.body).await {
                    Ok(bytes) => {
                        let cached = Arc::new(CachedResponse {
                            status: upstream.status,
                            content_type: upstream.content_type.clone(),
                            body: bytes.clone(),
                            provider: upstream.provider.clone(),
                            model: upstream.model.clone(),
                            usage: upstream.usage,
                        });
                        if let (Some(cache), Some(key_str)) = (state.cache.as_ref(), cache_key) {
                            cache.insert(key_str, cached.clone()).await;
                        }
                        build_response(
                            cached.status,
                            cached.content_type.as_deref(),
                            Body::from(bytes),
                            "MISS",
                        )
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to drain upstream body for caching");
                        error(StatusCode::BAD_GATEWAY, "failed to read upstream response")
                    }
                }
            } else {
                build_response(
                    upstream.status,
                    upstream.content_type.as_deref(),
                    Body::from_stream(upstream.body),
                    "MISS",
                )
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "upstream chat completion failed");
            span.record("vaultplane.cache.hit", false);
            span.record("http.response.status_code", 502_u64);
            span.record("duration_ms", duration_ms);
            error(StatusCode::BAD_GATEWAY, "upstream request failed")
        }
    }
}

/// Build an HTTP response with optional content-type and a cache header.
fn build_response(status: u16, content_type: Option<&str>, body: Body, cache: &str) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(CACHE_HEADER, cache);
    if let Some(ct) = content_type {
        builder = builder.header(CONTENT_TYPE, ct);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}

/// Serve a response from the cache and record cache-hit attributes on the span.
fn serve_from_cache(
    pricing: &Pricing,
    span: &tracing::Span,
    cached: &Arc<CachedResponse>,
    start: Instant,
) -> Response {
    span.record("gen_ai.system", cached.provider.as_str());
    span.record("gen_ai.response.model", cached.model.as_str());
    span.record("vaultplane.provider.attempts", 0_u64);
    span.record("http.response.status_code", cached.status as u64);
    span.record("vaultplane.cache.hit", true);
    if let Some(usage) = &cached.usage {
        span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as u64);
        span.record("gen_ai.usage.output_tokens", usage.completion_tokens as u64);
        if let Some(cost) = compute_cost(pricing, &cached.provider, &cached.model, usage) {
            span.record("vaultplane.cost_usd", cost);
        }
    }
    span.record("duration_ms", start.elapsed().as_millis() as u64);

    build_response(
        cached.status,
        cached.content_type.as_deref(),
        Body::from(cached.body.clone()),
        "HIT",
    )
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
    use std::time::Duration;
    use tower::ServiceExt;
    use vaultplane_core::auth::{KeyStore, RateLimiter, VirtualKey, hash_token};
    use vaultplane_core::cache::ResponseCache;
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

    fn no_cache() -> Option<Arc<ResponseCache>> {
        None
    }

    fn no_rate_limit() -> Arc<RateLimiter> {
        Arc::new(RateLimiter::default())
    }

    fn store_with_key(models: &[&str]) -> Arc<KeyStore> {
        let token = "vp_test";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_test_id".to_string();
        key.hash = hash_token(token);
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
        assert!((cost - 7.5).abs() < 1e-9);

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

        let app = router(
            connector(&server.uri()),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
        );
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
            no_cache(),
            no_rate_limit(),
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
            no_cache(),
            no_rate_limit(),
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
            no_cache(),
            no_rate_limit(),
        );
        let response = app
            .oneshot(chat_request(Some("vp_test"), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn embeddings_is_not_implemented() {
        let app = router(
            connector("http://127.0.0.1:1"),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
        );
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
        let app = router(
            connector("http://127.0.0.1:1"),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
        );
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

    #[tokio::test]
    async fn repeated_request_is_served_from_cache() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"chat.completion","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let cache = Arc::new(ResponseCache::new(1024 * 1024, Duration::from_secs(60)));
        let app = router(
            connector(&server.uri()),
            open_keys(),
            open_pricing(),
            Some(cache),
            no_rate_limit(),
        );

        let first = app
            .clone()
            .oneshot(chat_request(None, "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers().get(super::CACHE_HEADER).unwrap(), "MISS");

        let second = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(second.headers().get(super::CACHE_HEADER).unwrap(), "HIT");

        let received = server.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            1,
            "second request must be served from cache"
        );
    }

    #[tokio::test]
    async fn rate_limit_returns_429_when_exceeded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let token = "vp_test";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_test_id".to_string();
        key.hash = hash_token(token);
        key.rate_limit_rps = Some(1);
        let keys = Arc::new(KeyStore::new(vec![key]));

        let app = router(
            connector(&server.uri()),
            keys,
            open_pricing(),
            no_cache(),
            no_rate_limit(),
        );

        let first = app
            .clone()
            .oneshot(chat_request(Some(token), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .oneshot(chat_request(Some(token), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
