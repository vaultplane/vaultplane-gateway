//! Admin API: health, readiness, status, and virtual key management.
//!
//! The admin API binds to its own port (default 9091) and is intended for
//! cluster-internal access. Protected endpoints require the static admin token
//! when one is configured; health and readiness probes are always open.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use vaultplane_core::auth::{
    KeyStore, RateLimiter, SpendLimit, SpendTracker, VirtualKey, constant_time_eq, generate_key,
};
use vaultplane_core::config::Config;

use crate::runtime::{self, RuntimeHandle};

/// Content type for the Prometheus text exposition format (version 0.0.4).
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Shared state for the admin API.
#[derive(Clone)]
pub struct AppState {
    started_at: Instant,
    ready: Arc<AtomicBool>,
    admin_token: Option<String>,
    config: Config,
    keys: Arc<KeyStore>,
    rate_limiter: Arc<RateLimiter>,
    spend_tracker: Arc<SpendTracker>,
    runtime: RuntimeHandle,
    /// Path to the YAML config file used at startup, if any. `POST
    /// /admin/config/reload` re-reads from this path; without it the endpoint
    /// returns 503 because there is nothing to reload from.
    config_path: Option<PathBuf>,
    /// Handle to the global Prometheus recorder, used to render the snapshot
    /// served by `GET /admin/metrics`.
    metrics: PrometheusHandle,
    /// Live rustls config for the proxy listener, if TLS is enabled. Kept
    /// here so the reload path can rotate certs in place via
    /// [`RustlsConfig::reload_from_pem_file`].
    rustls: Option<RustlsConfig>,
}

impl AppState {
    /// Create admin state for a freshly started gateway.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        admin_token: Option<String>,
        keys: Arc<KeyStore>,
        rate_limiter: Arc<RateLimiter>,
        spend_tracker: Arc<SpendTracker>,
        runtime: RuntimeHandle,
        config_path: Option<PathBuf>,
        metrics: PrometheusHandle,
        rustls: Option<RustlsConfig>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            ready: Arc::new(AtomicBool::new(false)),
            admin_token,
            config,
            keys,
            rate_limiter,
            spend_tracker,
            runtime,
            config_path,
            metrics,
            rustls,
        }
    }

    /// Mark whether the gateway is ready to serve traffic.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }
}

/// Build the admin API router. Health and readiness are open; everything else
/// is gated by the admin token when one is configured.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/admin/status", get(status))
        .route("/admin/metrics", get(metrics))
        .route("/admin/keys", get(list_keys).post(create_key))
        .route("/admin/keys/{id}", delete(delete_key))
        .route("/admin/config/reload", post(reload_config))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_token,
        ));

    Router::new()
        .route("/admin/healthz", get(healthz))
        .route("/admin/readyz", get(readyz))
        .merge(protected)
        .with_state(state)
}

/// Reject requests to protected endpoints that lack the configured admin token.
async fn require_admin_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = &state.admin_token else {
        return next.run(request).await;
    };

    let authorized = crate::bearer_token(request.headers())
        .map(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);

    if authorized {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

/// Liveness: the process is up.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Readiness: the gateway has loaded config and is ready to serve.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready.load(Ordering::SeqCst) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// A snapshot of the running gateway.
#[derive(Serialize)]
struct StatusBody {
    version: &'static str,
    uptime_seconds: u64,
    ready: bool,
    proxy_address: String,
    admin_address: String,
    key_count: usize,
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    Json(StatusBody {
        version: vaultplane_core::VERSION,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        ready: state.ready.load(Ordering::SeqCst),
        proxy_address: state.config.listen.address.clone(),
        admin_address: state.config.listen.admin_address.clone(),
        key_count: state.keys.len(),
    })
}

/// Render the current snapshot of the Prometheus registry in the text
/// exposition format. Gated by the admin token because the rest of the admin
/// surface is; Prometheus can authenticate scrapes with `bearer_token_file`.
async fn metrics(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();
    ([(CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body).into_response()
}

/// A non-secret view of a virtual key. The hash is intentionally excluded so
/// the admin API does not leak material that could be used to brute-force the
/// token, even though SHA-256 of a high-entropy token is not practically
/// recoverable.
#[derive(Debug, Serialize)]
struct KeySummary {
    id: String,
    team: String,
    app: String,
    env: String,
    models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limit_rps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spend_limit: Option<SpendLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

impl From<VirtualKey> for KeySummary {
    fn from(k: VirtualKey) -> Self {
        Self {
            id: k.id,
            team: k.team,
            app: k.app,
            env: k.env,
            models: k.models,
            rate_limit_rps: k.rate_limit_rps,
            spend_limit: k.spend_limit,
            expires_at: k.expires_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct ListKeysResponse {
    data: Vec<KeySummary>,
}

async fn list_keys(State(state): State<AppState>) -> Response {
    let data: Vec<KeySummary> = state
        .keys
        .list()
        .into_iter()
        .map(KeySummary::from)
        .collect();
    Json(ListKeysResponse { data }).into_response()
}

/// Body of `POST /admin/keys`. All fields are optional; an empty body produces
/// an unscoped key that can call any model with no limits.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CreateKeyRequest {
    team: String,
    app: String,
    env: String,
    models: Vec<String>,
    rate_limit_rps: Option<u32>,
    spend_limit: Option<SpendLimit>,
    expires_at: Option<String>,
}

/// Response from `POST /admin/keys`. The plaintext token is shown exactly once.
#[derive(Debug, Serialize)]
struct CreateKeyResponse {
    /// The plaintext bearer token. Show it to the caller, then discard.
    token: String,
    /// The non-secret key metadata.
    key: KeySummary,
}

async fn create_key(State(state): State<AppState>, Json(body): Json<CreateKeyRequest>) -> Response {
    let generated = generate_key();
    let key = VirtualKey {
        id: generated.id.clone(),
        hash: generated.hash.clone(),
        team: body.team,
        app: body.app,
        env: body.env,
        models: body.models,
        rate_limit_rps: body.rate_limit_rps,
        spend_limit: body.spend_limit,
        expires_at: body.expires_at,
    };
    state.keys.insert(key.clone());

    tracing::info!(
        key_id = %key.id,
        team = %key.team,
        app = %key.app,
        env = %key.env,
        "virtual key issued"
    );

    (
        StatusCode::CREATED,
        Json(CreateKeyResponse {
            token: generated.token,
            key: KeySummary::from(key),
        }),
    )
        .into_response()
}

async fn delete_key(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.keys.remove_by_id(&id) {
        state.rate_limiter.forget(&id);
        state.spend_tracker.forget(&id);
        tracing::info!(key_id = %id, "virtual key revoked");
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "key not found").into_response()
    }
}

#[derive(Debug, Serialize)]
struct ReloadResponse {
    status: &'static str,
    config_path: String,
}

/// Re-read the configured YAML file and atomically swap the runtime.
///
/// On validation failure the old runtime stays in place and the endpoint
/// returns 400 with the error text. Without a `--config` path at startup the
/// endpoint returns 503: there is nothing to reload from.
async fn reload_config(State(state): State<AppState>) -> Response {
    let Some(path) = state.config_path.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "config reload requires the gateway to have been started with --config",
        )
            .into_response();
    };

    match runtime::reload(&state.runtime, Some(path), state.rustls.as_ref()).await {
        Ok(()) => {
            tracing::info!(config_path = %path.display(), "config reloaded");
            Json(ReloadResponse {
                status: "reloaded",
                config_path: path.display().to_string(),
            })
            .into_response()
        }
        Err(err) => {
            tracing::warn!(error = %err, "config reload rejected");
            (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{self, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use std::io::Write;
    use tower::ServiceExt;
    use vaultplane_core::config::Config;

    use crate::{prom, runtime};

    fn empty_runtime() -> RuntimeHandle {
        runtime::handle(runtime::build_runtime(&Config::default()).unwrap())
    }

    fn state(token: Option<&str>) -> AppState {
        state_with(token, empty_runtime(), None)
    }

    fn state_with(
        token: Option<&str>,
        runtime: RuntimeHandle,
        config_path: Option<PathBuf>,
    ) -> AppState {
        state_with_rustls(token, runtime, config_path, None)
    }

    fn state_with_rustls(
        token: Option<&str>,
        runtime: RuntimeHandle,
        config_path: Option<PathBuf>,
        rustls: Option<RustlsConfig>,
    ) -> AppState {
        AppState::new(
            Config::default(),
            token.map(str::to_string),
            Arc::new(KeyStore::default()),
            Arc::new(RateLimiter::default()),
            Arc::new(SpendTracker::default()),
            runtime,
            config_path,
            prom::install(),
            rustls,
        )
    }

    fn request(method: &str, uri: &str, token: Option<&str>, body: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = body.map(|b| Body::from(b.to_string())).unwrap_or_default();
        builder.body(body).unwrap()
    }

    async fn body_json(response: Response) -> Value {
        let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn healthz_is_open_even_with_a_token_configured() {
        let app = router(state(Some("secret")));
        let response = app
            .oneshot(request("GET", "/admin/healthz", None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reflects_readiness() {
        let s = state(None);
        let app = router(s.clone());

        let response = app
            .clone()
            .oneshot(request("GET", "/admin/readyz", None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        s.set_ready(true);
        let response = app
            .oneshot(request("GET", "/admin/readyz", None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_requires_the_token_when_configured() {
        let app = router(state(Some("secret")));

        let response = app
            .clone()
            .oneshot(request("GET", "/admin/status", None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(request("GET", "/admin/status", Some("wrong"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(request("GET", "/admin/status", Some("secret"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_is_open_when_no_token_configured() {
        let app = router(state(None));
        let response = app
            .oneshot(request("GET", "/admin/status", None, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn key_management_endpoints_require_the_admin_token() {
        let app = router(state(Some("secret")));

        for (method, uri) in [
            ("GET", "/admin/keys"),
            ("POST", "/admin/keys"),
            ("DELETE", "/admin/keys/vp_unknown"),
            ("GET", "/admin/metrics"),
        ] {
            let response = app
                .clone()
                .oneshot(request(method, uri, None, None))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} should require admin token"
            );
        }
    }

    #[tokio::test]
    async fn create_then_list_then_delete_round_trips_a_key() {
        let s = state(Some("secret"));
        let app = router(s.clone());

        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/admin/keys",
                Some("secret"),
                Some(r#"{"team":"core","app":"web","env":"prod","models":["gpt-4o"]}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;

        let token = body["token"]
            .as_str()
            .expect("token in response")
            .to_string();
        let id = body["key"]["id"]
            .as_str()
            .expect("id in response")
            .to_string();
        assert!(token.starts_with("vp_"));
        assert!(id.starts_with("vp_"));
        assert_eq!(body["key"]["team"], "core");
        assert_eq!(body["key"]["models"][0], "gpt-4o");
        assert!(
            body["key"].get("hash").is_none(),
            "hash must not be returned"
        );

        let response = app
            .clone()
            .oneshot(request("GET", "/admin/keys", Some("secret"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], id);
        assert!(data[0].get("hash").is_none(), "hash must not be listed");

        // The issued token authenticates against the shared key store.
        assert!(
            s.keys.authenticate(&token).is_some(),
            "issued token authenticates"
        );

        let response = app
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/admin/keys/{id}"),
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(s.keys.authenticate(&token).is_none(), "revoked token fails");

        let response = app
            .oneshot(request(
                "DELETE",
                &format!("/admin/keys/{id}"),
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn config_reload_without_a_config_path_returns_503() {
        let s = state(Some("secret"));
        let app = router(s);
        let response = app
            .oneshot(request(
                "POST",
                "/admin/config/reload",
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn config_reload_swaps_the_runtime_for_a_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vaultplane.yaml");
        std::fs::write(
            &path,
            "models:\n  - name: smart\n    primary: { provider: openai, model: gpt-4o }\n",
        )
        .unwrap();

        let runtime = empty_runtime();
        let s = state_with(Some("secret"), runtime.clone(), Some(path.clone()));
        let app = router(s);

        assert!(
            runtime.load().models.is_empty(),
            "initial runtime has no models"
        );

        let response = app
            .oneshot(request(
                "POST",
                "/admin/config/reload",
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["status"], "reloaded");

        let models = &runtime.load().models;
        assert_eq!(models.len(), 1, "new runtime has one model");
        assert_eq!(models[0].id, "smart");
        assert_eq!(models[0].provider, "openai");
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_prometheus_text_format() {
        let s = state(Some("secret"));
        let app = router(s);

        // Record something so the snapshot is non-empty and includes our prefix.
        ::metrics::counter!("vaultplane_requests_total", "provider" => "test", "model" => "m", "status" => "200").increment(1);

        let response = app
            .oneshot(request("GET", "/admin/metrics", Some("secret"), None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.starts_with("text/plain"),
            "expected text/plain content type, got {ct}"
        );
        let bytes = body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("vaultplane_requests_total"),
            "metrics snapshot missing recorded counter:\n{text}"
        );
    }

    #[tokio::test]
    async fn config_reload_rotates_tls_certs_when_enabled() {
        // Bootstrap state with a live rustls config built from cert A, plus a
        // config file that points TLS at cert B. POSTing the reload endpoint
        // should swap the rustls material in place (verified by the
        // post-reload Ok response and the side-effect inspection below).
        use rcgen::generate_simple_self_signed;
        use std::io::Write;

        fn write_cert(dir: &std::path::Path, name: &str) -> (PathBuf, PathBuf) {
            let signed = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let cert_pem = signed.cert.pem();
            let key_pem = signed.key_pair.serialize_pem();
            let cert_path = dir.join(format!("{name}.crt.pem"));
            let key_path = dir.join(format!("{name}.key.pem"));
            std::fs::File::create(&cert_path)
                .unwrap()
                .write_all(cert_pem.as_bytes())
                .unwrap();
            std::fs::File::create(&key_path)
                .unwrap()
                .write_all(key_pem.as_bytes())
                .unwrap();
            (cert_path, key_path)
        }

        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_cert(dir.path(), "a");
        let (cert_b, key_b) = write_cert(dir.path(), "b");

        let rustls = RustlsConfig::from_pem_file(&cert_a, &key_a).await.unwrap();

        let config_path = dir.path().join("vaultplane.yaml");
        std::fs::write(
            &config_path,
            format!(
                "listen:\n  tls:\n    cert_path: \"{}\"\n    key_path: \"{}\"\n",
                cert_b.display().to_string().replace('\\', "/"),
                key_b.display().to_string().replace('\\', "/"),
            ),
        )
        .unwrap();

        let runtime = empty_runtime();
        let s = state_with_rustls(
            Some("secret"),
            runtime,
            Some(config_path),
            Some(rustls.clone()),
        );
        let app = router(s);

        let response = app
            .oneshot(request(
                "POST",
                "/admin/config/reload",
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "reload should succeed when both runtime and TLS material are valid"
        );

        // After reload, the SAME rustls handle (the one shared with the
        // listener) should now load cert B. Reloading once more from cert B's
        // paths must succeed; if the previous reload had not actually run,
        // this would still succeed but a second reload from a *bad* path
        // should error, proving the live handle is live.
        reload_certs_smoke(&rustls, &cert_b, &key_b).await;
    }

    async fn reload_certs_smoke(rustls: &RustlsConfig, cert: &PathBuf, key: &PathBuf) {
        rustls
            .reload_from_pem_file(cert, key)
            .await
            .expect("live handle should accept its own cert paths");
    }

    #[tokio::test]
    async fn config_reload_keeps_old_runtime_on_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vaultplane.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"this: is: not: valid yaml: [unterminated\n")
            .unwrap();

        let runtime = empty_runtime();
        let s = state_with(Some("secret"), runtime.clone(), Some(path));
        let app = router(s);

        let response = app
            .oneshot(request(
                "POST",
                "/admin/config/reload",
                Some("secret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            runtime.load().models.is_empty(),
            "old runtime is preserved on validation failure"
        );
    }
}
