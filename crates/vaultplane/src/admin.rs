//! Admin API: health, readiness, and status endpoints.
//!
//! The admin API binds to its own port (default 9091) and is intended for
//! cluster-internal access. Authentication for the privileged endpoints is not yet
//! implemented.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use vaultplane_core::config::Config;

/// Shared state for the admin API.
#[derive(Clone)]
pub struct AppState {
    started_at: Instant,
    ready: Arc<AtomicBool>,
    config: Config,
}

impl AppState {
    /// Create admin state for a freshly started gateway.
    pub fn new(config: Config) -> Self {
        Self {
            started_at: Instant::now(),
            ready: Arc::new(AtomicBool::new(false)),
            config,
        }
    }

    /// Mark whether the gateway is ready to serve traffic.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }
}

/// Build the admin API router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/admin/healthz", get(healthz))
        .route("/admin/readyz", get(readyz))
        .route("/admin/status", get(status))
        .with_state(state)
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

    #[tokio::test]
    async fn healthz_is_ok() {
        let app = router(AppState::new(Config::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reflects_readiness() {
        let state = AppState::new(Config::default());
        let app = router(state.clone());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.set_ready(true);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
