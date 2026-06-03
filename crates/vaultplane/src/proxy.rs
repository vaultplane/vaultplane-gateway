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
use vaultplane_core::Error as CoreError;
use vaultplane_core::auth::{KeyStore, RateLimiter, SpendTracker, VirtualKey};
use vaultplane_core::cache::{CachedResponse, ResponseCache};
use vaultplane_core::config::Pricing;
use vaultplane_core::plugin::{Decision, Plugin, PluginRequest, RejectInfo};
use vaultplane_core::provider::{BodyStream, ChatRequest, EmbeddingsRequest, Usage};
use vaultplane_core::stream_observer::UsageObservingStream;

use crate::prom::{label_values, names};
use crate::runtime::RuntimeHandle;
// The test-only `router()` convenience constructor wraps individual fields into
// a `Runtime`; production callers use `router_with_runtime` with a handle they
// own. These imports back that test-only constructor.
#[cfg(test)]
use crate::runtime::{self, RegisteredModel, Runtime};
#[cfg(test)]
use vaultplane_core::plugin::PluginChain;
#[cfg(test)]
use vaultplane_core::provider::Connector;

const CACHE_HEADER: &str = "x-vaultplane-cache";

/// Increment the rejection counter with the given reason.
fn record_rejection(reason: &'static str) {
    metrics::counter!(names::REJECTIONS_TOTAL, "reason" => reason).increment(1);
}

/// Record one completed request: bump the request count, the duration
/// histogram, and (when a cost is present) the cumulative cents counter.
fn record_completion(
    provider: &str,
    model: &str,
    status: u16,
    cache_label: &'static str,
    duration_seconds: f64,
    cost_usd: Option<f64>,
) {
    let provider = provider.to_string();
    let model = model.to_string();
    let status = status.to_string();
    metrics::counter!(
        names::REQUESTS_TOTAL,
        "provider" => provider.clone(),
        "model" => model.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        names::REQUEST_DURATION_SECONDS,
        "provider" => provider.clone(),
        "model" => model.clone(),
        "cache" => cache_label,
    )
    .record(duration_seconds);
    if let Some(cost) = cost_usd {
        // Round to nearest cent. Negative or NaN costs (which should never
        // happen but would saturate-cast to zero) are dropped.
        if cost.is_finite() && cost > 0.0 {
            let cents = (cost * 100.0).round() as u64;
            if cents > 0 {
                metrics::counter!(
                    names::COST_CENTS_TOTAL,
                    "provider" => provider,
                    "model" => model,
                )
                .increment(cents);
            }
        }
    }
}

/// Shared state for the proxy API.
///
/// `runtime` is hot-swappable: a config reload replaces it atomically, and each
/// request loads the current snapshot once at the top of the handler. The
/// keystore, rate limiter, and spend tracker are NOT swapped on reload; they
/// hold per-key state that must persist across configuration changes.
#[derive(Clone)]
struct ProxyState {
    runtime: RuntimeHandle,
    keys: Arc<KeyStore>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
}

/// Build the proxy API router around a provider connector, key store, pricing,
/// optional response cache, rate limiter, spend tracker, and the registered
/// virtual model list for `GET /v1/models`.
///
/// Convenience wrapper used by tests: bundles the runtime fields into a
/// freshly built [`Runtime`] and a private swap handle. Production callers
/// should construct the runtime separately and use [`router_with_runtime`] so
/// the same handle can be shared with the reload path.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn router(
    connector: Arc<dyn Connector>,
    keys: Arc<KeyStore>,
    pricing: Arc<Pricing>,
    cache: Option<Arc<ResponseCache>>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
    models: Arc<Vec<RegisteredModel>>,
    plugins: PluginChain,
) -> Router {
    let runtime = runtime::handle(Runtime {
        connector,
        pricing,
        cache,
        plugins,
        models,
    });
    router_with_runtime(runtime, keys, rate_limiter, spend_tracker)
}

/// Build the proxy API router around an externally owned [`RuntimeHandle`].
/// The handle is shared with the admin module so a configuration reload swaps
/// the runtime in place for the live router.
pub fn router_with_runtime(
    runtime: RuntimeHandle,
    keys: Arc<KeyStore>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
) -> Router {
    let state = ProxyState {
        runtime,
        keys,
        rate_limiter,
        spend_tracker,
    };
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(list_models))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

async fn list_models(State(state): State<ProxyState>) -> Response {
    let runtime = state.runtime.load();
    let data: Vec<serde_json::Value> = runtime
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
            .and_then(|token| state.keys.authenticate(token))
        {
            Some(key) => key,
            None => {
                record_rejection(label_values::REASON_AUTH);
                return error(StatusCode::UNAUTHORIZED, "missing or invalid virtual key");
            }
        }
    };

    if key.is_expired() {
        record_rejection(label_values::REASON_EXPIRED);
        return error(StatusCode::UNAUTHORIZED, "virtual key has expired");
    }

    if let Some(rps) = key.rate_limit_rps
        && !state.rate_limiter.check(&key.identifier(), rps)
    {
        record_rejection(label_values::REASON_RATE_LIMIT);
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

/// Run the inline plugin chain on the request body. Each plugin sees the body
/// produced by the previous one; on Reject the chain shortcircuits.
fn run_plugins(plugins: &[Box<dyn Plugin>], body: Bytes) -> Result<Bytes, RejectInfo> {
    let mut current = body;
    for plugin in plugins {
        let request = PluginRequest {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: Vec::new(),
            body: current.clone(),
        };
        match plugin.inspect_request(&request) {
            Decision::Pass => {}
            Decision::Modify(updated) => current = updated.body,
            Decision::Reject(info) => return Err(info),
        }
    }
    Ok(current)
}

async fn chat_completions(
    State(state): State<ProxyState>,
    Extension(key): Extension<VirtualKey>,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let meta: ChatMeta = serde_json::from_slice(&body).unwrap_or_default();
    let virtual_model = meta.model.unwrap_or_default();

    // Snapshot the current runtime once; a config reload swaps the handle but
    // this request keeps the bundle it loaded here for the rest of its life.
    let runtime = state.runtime.load();

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
        record_rejection(label_values::REASON_FORBIDDEN_MODEL);
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
        record_rejection(label_values::REASON_SPEND_LIMIT);
        span.record("vaultplane.cache.hit", false);
        span.record("http.response.status_code", 402_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::PAYMENT_REQUIRED,
            "spend limit exceeded for this period",
        );
    }

    // Run inline plugins (e.g. PII redaction) before the cache and the upstream
    // dispatch. A Modify decision replaces the body for the rest of the request;
    // a Reject decision shortcircuits with the plugin's status and reason.
    let body = match run_plugins(&runtime.plugins, body) {
        Ok(body) => body,
        Err(info) => {
            record_rejection(label_values::REASON_PLUGIN);
            tracing::warn!(
                reason = %info.reason,
                status = info.status_code,
                "request rejected by inline plugin",
            );
            span.record("vaultplane.cache.hit", false);
            span.record("http.response.status_code", u64::from(info.status_code));
            span.record("duration_ms", start.elapsed().as_millis() as u64);
            return error(
                StatusCode::from_u16(info.status_code).unwrap_or(StatusCode::FORBIDDEN),
                &info.reason,
            );
        }
    };

    // Compute the cache key for cacheable (non-streaming) requests.
    let cache_key =
        (!meta.stream).then(|| ResponseCache::key(&key.identifier(), &virtual_model, &body));

    // Cache lookup. Cache hits do not count against the spend budget (no upstream
    // spend incurred); their cost on the span is informational.
    if let (Some(cache), Some(key_str)) = (runtime.cache.as_ref(), cache_key.as_ref())
        && let Some(cached) = cache.get(key_str).await
    {
        return serve_from_cache(&runtime.pricing, &span, &cached, start);
    }

    let request = ChatRequest {
        model: virtual_model,
        stream: meta.stream,
        body,
    };

    let result = runtime.connector.chat(request).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(upstream) => {
            span.record("gen_ai.system", upstream.provider.as_str());
            span.record("gen_ai.response.model", upstream.model.as_str());
            span.record("vaultplane.provider.attempts", upstream.attempts as u64);
            span.record("http.response.status_code", upstream.status as u64);
            span.record("vaultplane.cache.hit", false);
            let mut cost_for_metrics: Option<f64> = None;
            if let Some(usage) = &upstream.usage {
                span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as u64);
                span.record("gen_ai.usage.output_tokens", usage.completion_tokens as u64);
                if let Some(cost) =
                    compute_cost(&runtime.pricing, &upstream.provider, &upstream.model, usage)
                {
                    span.record("vaultplane.cost_usd", cost);
                    cost_for_metrics = Some(cost);
                    if let Some(limit) = &key.spend_limit {
                        state
                            .spend_tracker
                            .record(&key.identifier(), limit.period, cost);
                    }
                }
            }
            span.record("duration_ms", duration_ms);

            record_completion(
                &upstream.provider,
                &upstream.model,
                upstream.status,
                label_values::CACHE_MISS,
                start.elapsed().as_secs_f64(),
                cost_for_metrics,
            );

            let should_cache =
                cache_key.is_some() && runtime.cache.is_some() && upstream.status == 200;

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
                        if let (Some(cache), Some(key_str)) = (runtime.cache.as_ref(), cache_key) {
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
                        runtime.pricing.clone(),
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
            let response = upstream_error_response(&err);
            span.record("vaultplane.cache.hit", false);
            span.record(
                "http.response.status_code",
                response.status().as_u16() as u64,
            );
            span.record("duration_ms", duration_ms);
            record_rejection(label_values::REASON_UPSTREAM_ERROR);
            response
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

    // Cache hits never incurred upstream spend, so cost is intentionally not
    // forwarded to the metrics counter; the request count and duration are.
    record_completion(
        &cached.provider,
        &cached.model,
        cached.status,
        label_values::CACHE_HIT,
        start.elapsed().as_secs_f64(),
        None,
    );

    build_response(
        cached.status,
        cached.content_type.as_deref(),
        Body::from(cached.body.clone()),
        "HIT",
    )
}

/// The fields of an embeddings request the gateway reads before forwarding the rest.
#[derive(Debug, Default, Deserialize)]
struct EmbeddingsMeta {
    model: Option<String>,
}

async fn embeddings(
    State(state): State<ProxyState>,
    Extension(key): Extension<VirtualKey>,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let meta: EmbeddingsMeta = serde_json::from_slice(&body).unwrap_or_default();
    let virtual_model = meta.model.unwrap_or_default();

    let runtime = state.runtime.load();

    let span = tracing::info_span!(
        "embeddings",
        "gen_ai.system" = tracing::field::Empty,
        "gen_ai.request.model" = %virtual_model,
        "gen_ai.response.model" = tracing::field::Empty,
        "gen_ai.usage.input_tokens" = tracing::field::Empty,
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
        record_rejection(label_values::REASON_FORBIDDEN_MODEL);
        span.record("vaultplane.cache.hit", false);
        span.record("http.response.status_code", 403_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::FORBIDDEN,
            "virtual key is not allowed to use this model",
        );
    }

    if let Some(limit) = &key.spend_limit
        && !state.spend_tracker.pre_check(&key.identifier(), limit)
    {
        record_rejection(label_values::REASON_SPEND_LIMIT);
        span.record("vaultplane.cache.hit", false);
        span.record("http.response.status_code", 402_u64);
        span.record("duration_ms", start.elapsed().as_millis() as u64);
        return error(
            StatusCode::PAYMENT_REQUIRED,
            "spend limit exceeded for this period",
        );
    }

    // Plugins run on the embeddings body the same way they run on chat.
    let body = match run_plugins(&runtime.plugins, body) {
        Ok(body) => body,
        Err(info) => {
            record_rejection(label_values::REASON_PLUGIN);
            tracing::warn!(
                reason = %info.reason,
                status = info.status_code,
                "request rejected by inline plugin",
            );
            span.record("vaultplane.cache.hit", false);
            span.record("http.response.status_code", u64::from(info.status_code));
            span.record("duration_ms", start.elapsed().as_millis() as u64);
            return error(
                StatusCode::from_u16(info.status_code).unwrap_or(StatusCode::FORBIDDEN),
                &info.reason,
            );
        }
    };

    // Embeddings are deterministic for the same input, so always cacheable.
    let cache_key = Some(ResponseCache::key(&key.identifier(), &virtual_model, &body));

    if let (Some(cache), Some(key_str)) = (runtime.cache.as_ref(), cache_key.as_ref())
        && let Some(cached) = cache.get(key_str).await
    {
        return serve_from_cache(&runtime.pricing, &span, &cached, start);
    }

    let request = EmbeddingsRequest {
        model: virtual_model,
        body,
    };

    let result = runtime.connector.embeddings(request).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(upstream) => {
            span.record("gen_ai.system", upstream.provider.as_str());
            span.record("gen_ai.response.model", upstream.model.as_str());
            span.record("vaultplane.provider.attempts", upstream.attempts as u64);
            span.record("http.response.status_code", upstream.status as u64);
            span.record("vaultplane.cache.hit", false);
            let mut cost_for_metrics: Option<f64> = None;
            if let Some(usage) = &upstream.usage {
                span.record("gen_ai.usage.input_tokens", usage.prompt_tokens as u64);
                if let Some(cost) =
                    compute_cost(&runtime.pricing, &upstream.provider, &upstream.model, usage)
                {
                    span.record("vaultplane.cost_usd", cost);
                    cost_for_metrics = Some(cost);
                    if let Some(limit) = &key.spend_limit {
                        state
                            .spend_tracker
                            .record(&key.identifier(), limit.period, cost);
                    }
                }
            }
            span.record("duration_ms", duration_ms);

            record_completion(
                &upstream.provider,
                &upstream.model,
                upstream.status,
                label_values::CACHE_MISS,
                start.elapsed().as_secs_f64(),
                cost_for_metrics,
            );

            let cacheable = runtime.cache.is_some() && upstream.status == 200;
            if cacheable {
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
                        if let (Some(cache), Some(key_str)) = (runtime.cache.as_ref(), cache_key) {
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
                        tracing::warn!(error = %err, "failed to drain upstream embeddings body");
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
            tracing::warn!(error = %err, "upstream embeddings request failed");
            let response = upstream_error_response(&err);
            span.record("vaultplane.cache.hit", false);
            span.record(
                "http.response.status_code",
                response.status().as_u16() as u64,
            );
            span.record("duration_ms", duration_ms);
            record_rejection(label_values::REASON_UPSTREAM_ERROR);
            response
        }
    }
}

async fn fallback() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

/// Build a small JSON error response in the OpenAI error shape.
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

/// Build an OpenAI-shaped error response for a connector failure. Timeouts
/// become 504 Gateway Timeout; everything else becomes 502 Bad Gateway. The
/// returned body includes a stable `type` field clients can match on.
fn upstream_error_response(err: &CoreError) -> Response {
    let (status, kind, message) = match err {
        CoreError::UpstreamTimeout(detail) => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            format!("upstream request timed out: {detail}"),
        ),
        other => (StatusCode::BAD_GATEWAY, "upstream_error", other.to_string()),
    };
    (
        status,
        Json(json!({ "error": { "message": message, "type": kind } })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{compute_cost, router};
    use async_trait::async_trait;
    use axum::body::{self, Body};
    use axum::http::{Method, Request, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;
    use vaultplane_core::Error as CoreError;
    use vaultplane_core::Result as CoreResult;
    use vaultplane_core::auth::{
        KeyStore, Period, RateLimiter, SpendLimit, SpendTracker, VirtualKey, hash_token,
    };
    use vaultplane_core::cache::ResponseCache;
    use vaultplane_core::config::{ModelPricing, Pricing};
    use vaultplane_core::plugin::{PiiRedactionPlugin, Plugin, PluginChain};
    use vaultplane_core::provider::Connector;
    use vaultplane_core::provider::Usage;
    use vaultplane_core::provider::openai::OpenAiConnector;
    use vaultplane_core::provider::{ChatRequest, ChatResponse, EmbeddingsRequest};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn connector(base_url: &str) -> Arc<dyn Connector> {
        Arc::new(OpenAiConnector::new(base_url, "test-key").unwrap())
    }

    /// A connector that always returns the same `Err`, for testing the
    /// proxy's error-mapping path.
    struct FailingConnector {
        make_error: Box<dyn Fn() -> CoreError + Send + Sync>,
    }

    impl FailingConnector {
        /// Build an erased `Arc<dyn Connector>` wrapping a closure that
        /// produces the error to return on every call.
        fn arc<F>(make_error: F) -> Arc<dyn Connector>
        where
            F: Fn() -> CoreError + Send + Sync + 'static,
        {
            Arc::new(Self {
                make_error: Box::new(make_error),
            })
        }
    }

    #[async_trait]
    impl Connector for FailingConnector {
        fn name(&self) -> &str {
            "failing"
        }
        async fn chat(&self, _request: ChatRequest) -> CoreResult<ChatResponse> {
            Err((self.make_error)())
        }
        async fn embeddings(&self, _request: EmbeddingsRequest) -> CoreResult<ChatResponse> {
            Err((self.make_error)())
        }
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

    fn no_plugins() -> PluginChain {
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
        );
        let response = app
            .oneshot(chat_request(Some("vp_test"), "gpt-4o"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn embeddings_request(token: Option<&str>, model: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/v1/embeddings")
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::from(format!(
                r#"{{"model":"{model}","input":"hello world"}}"#
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn upstream_timeout_returns_504_with_openai_error_shape() {
        let app = router(
            FailingConnector::arc(|| {
                CoreError::UpstreamTimeout("test upstream timeout".to_string())
            }),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
            no_plugins(),
        );
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let bytes = body::to_bytes(response.into_body(), 8 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["type"], "upstream_timeout");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("timed out"),
            "expected timeout in message, got {message}"
        );
    }

    #[tokio::test]
    async fn upstream_provider_error_returns_502_with_message_in_body() {
        let app = router(
            FailingConnector::arc(|| {
                CoreError::Provider("connection refused at 127.0.0.1:1".to_string())
            }),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
            no_plugins(),
        );
        let response = app.oneshot(chat_request(None, "gpt-4o")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = body::to_bytes(response.into_body(), 8 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["type"], "upstream_error");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("connection refused"),
            "expected the connector's message in the body, got {message}"
        );
    }

    #[tokio::test]
    async fn embeddings_proxies_to_upstream_when_open() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"list","data":[{"object":"embedding","embedding":[0.1,0.2]}],"usage":{"prompt_tokens":2,"total_tokens":2}}"#,
                    ),
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
            no_plugins(),
        );
        let response = app
            .oneshot(embeddings_request(None, "text-embedding-3-small"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("\"embedding\""), "body: {text}");
    }

    #[tokio::test]
    async fn embeddings_rejects_a_disallowed_model() {
        let app = router(
            connector("http://127.0.0.1:1"),
            store_with_key(&["text-embedding-3-small"]),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
            no_plugins(),
        );
        let response = app
            .oneshot(embeddings_request(
                Some("vp_test"),
                "text-embedding-3-large",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn embeddings_repeated_request_is_served_from_cache() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"list","data":[],"usage":{"prompt_tokens":2,"total_tokens":2}}"#,
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
            no_plugins(),
        );

        let first = app
            .clone()
            .oneshot(embeddings_request(None, "text-embedding-3-small"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers().get(super::CACHE_HEADER).unwrap(), "MISS");

        let second = app
            .oneshot(embeddings_request(None, "text-embedding-3-small"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(second.headers().get(super::CACHE_HEADER).unwrap(), "HIT");
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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
            no_plugins(),
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

    #[tokio::test]
    async fn pii_plugin_redacts_request_body_before_dispatch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({
                "messages": [{ "content": "My SSN is [REDACTED] please respond" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let plugins: PluginChain = Arc::new(vec![
            Box::new(PiiRedactionPlugin::default()) as Box<dyn Plugin>
        ]);
        let app = router(
            connector(&server.uri()),
            open_keys(),
            open_pricing(),
            no_cache(),
            no_rate_limit(),
            no_spend_tracker(),
            no_models(),
            plugins,
        );

        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"My SSN is 123-45-6789 please respond"}]}"#,
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
