//! OpenAI-compatible proxy API.
//!
//! Each request is authenticated (with expiry checked), rate-limited per key,
//! gated on the key's spend limit for the period, optionally served from the
//! exact-match cache, and otherwise dispatched through the provider connector
//! (typically the model registry). Successful non-streaming responses are stored in
//! the cache before being returned, and the upstream cost is recorded against the
//! key's spend tracker. Each request is recorded as a tracing span with OpenTelemetry
//! GenAI semantic-convention attributes plus VaultPlane attributes.

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
use vaultplane_core::auth::{KeyStore, RateLimiter, SpendTracker, VirtualKey};
use vaultplane_core::cache::{CachedResponse, ResponseCache};
use vaultplane_core::config::Pricing;
use vaultplane_core::provider::{BodyStream, ChatRequest, Connector, Usage};
use vaultplane_core::stream_observer::UsageObservingStream;

const CACHE_HEADER: &str = "x-vaultplane-cache";

/// A virtual model entry surfaced from the registry by `GET /v1/models`.
#[derive(Clone, Debug)]
pub struct RegisteredModel {
    pub id: String,
    pub provider: String,
}

/// Shared state for the proxy API.
#[derive(Clone)]
struct ProxyState {
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
    cache: Option<Arc<ResponseCache>>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
    models: Arc<Vec<RegisteredModel>>,
}

/// Build the proxy API router around a provider connector, key store, pricing,
/// optional response cache, rate limiter, spend tracker, and the registered
/// virtual model list for `GET /v1/models`.
#[allow(clippy::too_many_arguments)]
pub fn router(
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
    cache: Option<Arc<ResponseCache>>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
    models: Arc<Vec<RegisteredModel>>,
) -> Router {
    let state = ProxyState {
        connector,
        keys,
        pricing,
        cache,
        rate_limiter,
        spend_tracker,
        models,
    };
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(not_implemented))
        .route("/v1/models", get(list_models))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn list_models(State(state): State<ProxyState>) -> Response {
    let data: Vec<serde_json::Value> = state
        .models
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "owned_by": m.provider,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// Authenticate the request against the virtual key store, reject expired keys,
/// and enforce the key's rate limit. Anonymous traffic (no keys configured) is
/// allowed through unrestricted.
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

    if key.is_expired() {
        return error(StatusCode::UNAUTHORIZED, "virtual key has expired");
    }

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
///
/// Thin wrapper over [`vaultplane_core::cost::compute`] so the streaming observer
/// in core and the request handler here apply the same formula.
fn compute_cost(pricing: &Pricing, provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    vaultplane_core::cost::compute(pricing, provider, model, usage)
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

    // Spend-limit pre-check: if the key has already spent its budget for the
    // current period, reject before contacting any upstream.
    if let Some(limit) = &key.spend_limit
        && !state.spend_tracker.pre_check(&key.identifier(), limit)
    {
        span.record("vaultplane.cache.hit", false);
        span.record("http.response.status_code", 402_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::PAYMENT_REQUIRED,
            "spend limit exceeded for this period",
        );
    }

    // Compute the cache key for cacheable (non-streaming) requests.
    let cache_key =
        (!meta.stream).then(|| ResponseCache::key(&key.identifier(), &virtual_model, &body));

    // Cache lookup. Cache hits do not count against the spend budget (no upstream
    // spend incurred); their cost on the span is informational.
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
                    if let Some(limit) = &key.spend_limit {
                        state
                            .spend_tracker
                            .record(&key.identifier(), limit.period, cost);
                    }
                }
            }
            span.record("duration_ms", duration_ms);

            let should_cache =
                cache_key.is_some() && state.cache.is_some() && upstream.status == 200;

            if should_cache {
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
                // For streaming responses, observe SSE chunks for OpenAI-style
                // `usage` and feed it back into the span and spend tracker. Cache
                // hits and non-streaming responses have usage recorded above.
                let body = if meta.stream {
                    UsageObservingStream::new(
                        upstream.body,
                        span.clone(),
                        state.pricing.clone(),
                        state.spend_tracker.clone(),
                        key.identifier(),
                        key.spend_limit,
                        upstream.provider.clone(),
                        upstream.model.clone(),
                    )
                    .boxed()
                } else {
                    upstream.body
                };
                build_response(
                    upstream.status,
                    upstream.content_type.as_deref(),
                    Body::from_stream(body),
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
    use vaultplane_core::auth::{
        KeyStore, Period, RateLimiter, SpendLimit, SpendTracker, VirtualKey, hash_token,
    };
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

    fn no_spend_tracker() -> Arc<SpendTracker> {
        Arc::new(SpendTracker::default())
    }

    fn no_models() -> Arc<Vec<super::RegisteredModel>> {
        Arc::new(Vec::new())
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
            no_spend_tracker(),
            no_models(),
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
        key.id = "vp_test_rl_id".to_string();
        key.hash = hash_token(token);
        key.rate_limit_rps = Some(1);
        let keys = Arc::new(KeyStore::new(vec![key]));

        let app = router(
            connector(&server.uri()),
            keys,
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
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

    #[tokio::test]
    async fn expired_key_is_rejected() {
        let token = "vp_test";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_test_exp_id".to_string();
        key.hash = hash_token(token);
        key.expires_at = Some("1970-01-01T00:00:00Z".to_string());
        let keys = Arc::new(KeyStore::new(vec![key]));

        let app = router(
            connector("http://127.0.0.1:1"),
            keys,
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
        );

        let response = app
            .oneshot(chat_request(Some(token), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn streaming_usage_is_observed_and_charged_against_spend() {
        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":0,\"total_tokens\":1000}}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let token = "vp_test";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_stream_spend_id".to_string();
        key.hash = hash_token(token);
        key.spend_limit = Some(SpendLimit {
            amount_usd: 0.5,
            period: Period::Day,
        });
        let keys = Arc::new(KeyStore::new(vec![key]));

        let mut pricing = Pricing::default();
        let mut openai = std::collections::HashMap::new();
        openai.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 1.0,
                output_per_1k_tokens_usd: 0.0,
            },
        );
        pricing.providers.insert("openai".to_string(), openai);
        let pricing = Arc::new(pricing);
        let spend_tracker = Arc::new(SpendTracker::default());

        let app = router(
            connector(&server.uri()),
            keys,
            pricing,
            no_cache(),
            no_rate_limit(),
            spend_tracker,
            no_models(),
        );

        // Streaming request: drain the body so the SSE flows through the observer
        // and the usage chunk is recorded against the spend tracker.
        let streaming = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(r#"{"model":"gpt-4o","stream":true}"#))
            .unwrap();
        let first = app.clone().oneshot(streaming).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();

        // The streaming request reported $1 of usage, exceeding the $0.5/day budget.
        // The next non-streaming request must be blocked with 402.
        let second = app
            .oneshot(chat_request(Some(token), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn spend_limit_returns_402_after_exceeding_budget() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"chat.completion","usage":{"prompt_tokens":1000,"completion_tokens":0,"total_tokens":1000}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let token = "vp_test";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_test_spend_id".to_string();
        key.hash = hash_token(token);
        key.spend_limit = Some(SpendLimit {
            amount_usd: 0.5,
            period: Period::Day,
        });
        let keys = Arc::new(KeyStore::new(vec![key]));

        // Pricing: openai gpt-4o input = $1/1k tokens → one request costs $1.
        let mut pricing = Pricing::default();
        let mut openai = std::collections::HashMap::new();
        openai.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 1.0,
                output_per_1k_tokens_usd: 0.0,
            },
        );
        pricing.providers.insert("openai".to_string(), openai);
        let pricing = Arc::new(pricing);

        let app = router(
            connector(&server.uri()),
            keys,
            pricing,
            no_cache(),
            no_rate_limit(),
            Arc::new(SpendTracker::default()),
            no_models(),
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
        assert_eq!(second.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn list_models_returns_the_configured_registry() {
        let models = Arc::new(vec![
            super::RegisteredModel {
                id: "smart".to_string(),
                provider: "openai".to_string(),
            },
            super::RegisteredModel {
                id: "claude-default".to_string(),
                provider: "anthropic".to_string(),
            },
        ]);
        let app = router(
            connector("http://127.0.0.1:1"),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            models,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["object"], "list");
        let data = value["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "smart");
        assert_eq!(data[0]["object"], "model");
        assert_eq!(data[0]["owned_by"], "openai");
        assert_eq!(data[1]["id"], "claude-default");
        assert_eq!(data[1]["owned_by"], "anthropic");
    }

    #[tokio::test]
    async fn list_models_is_empty_when_no_registry_is_configured() {
        let app = router(
            connector("http://127.0.0.1:1"),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["data"].as_array().unwrap().len(), 0);
    }
}
