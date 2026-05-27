//! VaultPlane Gateway data plane entry point.
//!
//! Loads layered configuration, binds the OpenAI-compatible proxy API and the admin
//! API on their configured ports, and serves until a shutdown signal arrives. The
//! proxy endpoints are stubs that return 501 until the connectors and routing land.

mod admin;
mod proxy;

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use vaultplane_core::config::Config;

use crate::admin::AppState;

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
    init_tracing();

    let args = Args::parse();
    let config = Config::load(args.config.as_deref().map(std::path::Path::new))
        .context("failed to load configuration")?;

    tracing::info!(
        version = vaultplane_core::VERSION,
        "VaultPlane Gateway starting"
    );

    run(config).await
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run(config: Config) -> anyhow::Result<()> {
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

    let state = AppState::new(config);

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

    let proxy_app = proxy::router();
    let admin_app = admin::router(state);

    // Broadcast a single shutdown signal to both servers for a graceful drain.
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received; draining in-flight requests");
        let _ = tx.send(true);
    });

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
