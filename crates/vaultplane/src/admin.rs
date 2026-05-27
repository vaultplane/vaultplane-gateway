//! Admin API: health, readiness, and status endpoints.
//!
//! The admin API binds to its own port (default 9091) and is intended for
//! cluster-internal access. `/admin/status` is protected by a static admin token
//! when one is configured; the health and readiness probes are always open.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use vaultplane_core::auth::constant_time_eq;
use vaultplane_core::config::Config;

/// Shared state for the admin API.
#[derive(Clone)]
pub struct AppState {
    started_at: Instant,
    ready: Arc<AtomicBool>,
    admin_token: Option<String>,
    config: Config,
}

impl AppState {
    /// Create admin state for a freshly started gateway.
    pub fn new(config: Config, admin_token: Option<String>) -> Self {
        Self {
            started_at: Instant::now(),
            ready: Arc::new(AtomicBool::new(false)),
            admin_token,
            config,
        }
    }

    /// Mark whether the gateway is ready to serve traffic.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }
}

/// Build the admin API router. Health and readiness are open; status is protected
/// by the admin token when one is configured.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/admin/status", get(status))
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
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    Json(StatusBody {
        version: vaultplane_core::VERSION,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        ready: state.ready.load(Ordering::SeqCst),
        proxy_address: state.config.listen.address.clone(),
        admin_address: state.config.listen.admin_address.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{AppState, router};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use vaultplane_core::config::Config;

    fn request(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn healthz_is_open_even_with_a_token_configured() {
        let app = router(AppState::new(Config::default(), Some("secret".to_string())));
        let response = app.oneshot(request("/admin/healthz", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reflects_readiness() {
        let state = AppState::new(Config::default(), None);
        let app = router(state.clone());

        let response = app
            .clone()
            .oneshot(request("/admin/readyz", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.set_ready(true);
        let response = app.oneshot(request("/admin/readyz", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_requires_the_token_when_configured() {
        let app = router(AppState::new(Config::default(), Some("secret".to_string())));

        let response = app
            .clone()
            .oneshot(request("/admin/status", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(request("/admin/status", Some("wrong")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(request("/admin/status", Some("secret")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn status_is_open_when_no_token_configured() {
        let app = router(AppState::new(Config::default(), None));
        let response = app.oneshot(request("/admin/status", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
