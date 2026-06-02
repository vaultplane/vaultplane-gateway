//! VaultPlane Gateway data plane entry point.
//!
//! Loads layered configuration, builds the provider connectors and the virtual
//! key store, binds the OpenAI-compatible proxy API and the admin API, and
//! serves until a shutdown signal arrives. SIGHUP (on Unix) and
//! `POST /admin/config/reload` re-read the configured YAML file and atomically
//! swap the runtime state without dropping in-flight requests.

mod admin;
mod proxy;
mod runtime;
mod telemetry;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use clap::Parser;
use tokio::net::TcpListener;
use vaultplane_core::auth::{KeyStore, RateLimiter, SpendTracker};
use vaultplane_core::config::Config;

use crate::admin::AppState;
use crate::runtime::RuntimeHandle;

/// VaultPlane Gateway: every model call, on policy.
#[derive(Debug, Parser)]
#[command(name = "vaultplane", version, about)]
struct Args {
    /// Path to the YAML configuration file.
    #[arg(long, value_name = "PATH", env = "VAULTPLANE_CONFIG")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tracer_provider = telemetry::init()?;

    let args = Args::parse();
    let config_path = args.config.as_deref().map(PathBuf::from);
    let config = Config::load(config_path.as_deref()).context("failed to load configuration")?;

    tracing::info!(
        version = vaultplane_core::VERSION,
        "VaultPlane Gateway starting"
    );

    let result = run(config, config_path).await;
    telemetry::shutdown(tracer_provider);
    result
}

/// Extract a `Bearer` token from an `Authorization` header, if present.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Read the admin token from the configured environment variable.
fn read_admin_token(config: &Config) -> Option<String> {
    let token = std::env::var(&config.auth.admin_token_env).unwrap_or_default();
    if token.is_empty() {
        tracing::warn!(
            var = %config.auth.admin_token_env,
            "admin token is not set; the admin status endpoint is unauthenticated"
        );
        None
    } else {
        Some(token)
    }
}

async fn run(config: Config, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let proxy_addr: SocketAddr = config
        .listen
        .address
        .parse()
        .with_context(|| format!("invalid proxy listen address: {}", config.listen.address))?;
    let admin_addr: SocketAddr = config.listen.admin_address.parse().with_context(|| {
        format!(
            "invalid admin listen address: {}",
            config.listen.admin_address
        )
    })?;

    let admin_token = read_admin_token(&config);

    let keys = Arc::new(KeyStore::new(config.auth.keys.clone()));
    if keys.is_empty() {
        tracing::warn!("no virtual keys configured; the proxy API is unauthenticated");
    } else {
        tracing::info!(count = keys.len(), "loaded virtual keys");
    }

    let rate_limiter = Arc::new(RateLimiter::default());
    let spend_tracker = Arc::new(SpendTracker::default());

    let initial_runtime = runtime::build_runtime(&config).context("failed to build runtime")?;
    if !config.models.is_empty() {
        tracing::info!(count = config.models.len(), "loaded model registry");
    }
    if config.cache.enabled {
        tracing::info!(
            size_mb = config.cache.size_mb,
            ttl_seconds = config.cache.ttl_seconds,
            "response cache enabled"
        );
    }
    if !config.plugins.is_empty() {
        tracing::info!(count = config.plugins.len(), "loaded inline plugins");
    }
    let runtime: RuntimeHandle = runtime::handle(initial_runtime);

    let state = AppState::new(
        config,
        admin_token,
        keys.clone(),
        rate_limiter.clone(),
        spend_tracker.clone(),
        runtime.clone(),
        config_path.clone(),
    );

    let proxy_listener = TcpListener::bind(proxy_addr)
        .await
        .with_context(|| format!("failed to bind proxy listener on {proxy_addr}"))?;
    let admin_listener = TcpListener::bind(admin_addr)
        .await
        .with_context(|| format!("failed to bind admin listener on {admin_addr}"))?;

    tracing::info!(%proxy_addr, "proxy API listening");
    tracing::info!(%admin_addr, "admin API listening");

    // Configuration is loaded and both listeners are bound: ready to serve.
    state.set_ready(true);

    let proxy_app = proxy::router_with_runtime(runtime.clone(), keys, rate_limiter, spend_tracker);
    let admin_app = admin::router(state);

    // Broadcast a single shutdown signal to both servers for a graceful drain.
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; draining in-flight requests");
        let _ = tx.send(true);
    });

    // SIGHUP triggers the same reload path the admin API exposes. Unix only;
    // on Windows operators use POST /admin/config/reload.
    spawn_reload_signal(runtime.clone(), config_path);

    let mut proxy_rx = rx.clone();
    let proxy_server = tokio::spawn(async move {
        axum::serve(proxy_listener, proxy_app)
            .with_graceful_shutdown(async move {
                let _ = proxy_rx.changed().await;
            })
            .await
    });

    let mut admin_rx = rx;
    let admin_server = tokio::spawn(async move {
        axum::serve(admin_listener, admin_app)
            .with_graceful_shutdown(async move {
                let _ = admin_rx.changed().await;
            })
            .await
    });

    let (proxy_res, admin_res) =
        tokio::try_join!(proxy_server, admin_server).context("a server task panicked")?;
    proxy_res.context("proxy server error")?;
    admin_res.context("admin server error")?;

    tracing::info!("shutdown complete");
    Ok(())
}

/// Install a SIGHUP handler that re-reads the config file and swaps the runtime
/// in place. Unix only.
#[cfg(unix)]
fn spawn_reload_signal(runtime: RuntimeHandle, config_path: Option<PathBuf>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            tracing::warn!("failed to install SIGHUP handler; config hot-reload disabled");
            return;
        };
        while hup.recv().await.is_some() {
            match runtime::reload(&runtime, config_path.as_deref()) {
                Ok(()) => tracing::info!("config reloaded via SIGHUP"),
                Err(err) => tracing::error!(error = %err, "SIGHUP config reload failed"),
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_reload_signal(_runtime: RuntimeHandle, _config_path: Option<PathBuf>) {
    // SIGHUP is not available on Windows; operators trigger reload via the
    // admin API endpoint instead.
}

/// Resolve when the process receives Ctrl-C or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
