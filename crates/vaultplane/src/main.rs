//! VaultPlane Gateway data plane entry point.
//!
//! Loads layered configuration, builds the provider connector and the virtual key
//! store, binds the OpenAI-compatible proxy API and the admin API, and serves until
//! a shutdown signal arrives.

mod admin;
mod proxy;
mod telemetry;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use clap::Parser;
use tokio::net::TcpListener;
use vaultplane_core::auth::{KeyStore, RateLimiter, SpendTracker};
use vaultplane_core::cache::ResponseCache;
use vaultplane_core::config::{Config, PluginConfig};
use vaultplane_core::plugin::{PiiRedactionPlugin, Plugin, PluginChain};
use vaultplane_core::provider::Connector;
use vaultplane_core::provider::anthropic::AnthropicConnector;
use vaultplane_core::provider::azure::AzureConnector;
use vaultplane_core::provider::bedrock::BedrockConnector;
use vaultplane_core::provider::openai::OpenAiConnector;
use vaultplane_core::provider::registry::Registry;

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
    let tracer_provider = telemetry::init()?;

    let args = Args::parse();
    let config = Config::load(args.config.as_deref().map(std::path::Path::new))
        .context("failed to load configuration")?;

    tracing::info!(
        version = vaultplane_core::VERSION,
        "VaultPlane Gateway starting"
    );

    let result = run(config).await;
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

/// Read a provider API key from the named environment variable, warning if unset.
fn read_key(var: &str, provider: &str) -> String {
    let key = std::env::var(var).unwrap_or_default();
    if key.is_empty() {
        tracing::warn!(
            provider,
            var,
            "API key is not set; requests to this provider will fail"
        );
    }
    key
}

/// Read an optional environment variable, returning `None` when unset or empty.
fn optional_env(var: &str) -> Option<String> {
    let value = std::env::var(var).unwrap_or_default();
    if value.is_empty() { None } else { Some(value) }
}

/// Build the provider connectors and the model registry from configuration.
fn build_router(config: &Config) -> anyhow::Result<Arc<dyn Connector>> {
    let openai_cfg = &config.providers.openai;
    let openai: Arc<dyn Connector> = Arc::new(
        OpenAiConnector::new(
            openai_cfg.base_url.clone(),
            read_key(&openai_cfg.api_key_env, "openai"),
        )
        .context("failed to build OpenAI connector")?,
    );

    let anthropic_cfg = &config.providers.anthropic;
    let anthropic: Arc<dyn Connector> = Arc::new(
        AnthropicConnector::new(
            anthropic_cfg.base_url.clone(),
            read_key(&anthropic_cfg.api_key_env, "anthropic"),
        )
        .context("failed to build Anthropic connector")?,
    );

    let azure_cfg = &config.providers.azure;
    let azure: Arc<dyn Connector> = Arc::new(
        AzureConnector::new(
            azure_cfg.base_url.clone(),
            read_key(&azure_cfg.api_key_env, "azure"),
            azure_cfg.api_version.clone(),
        )
        .context("failed to build Azure OpenAI connector")?,
    );

    let bedrock_cfg = &config.providers.bedrock;
    let bedrock: Arc<dyn Connector> = Arc::new(
        BedrockConnector::new(
            bedrock_cfg.region.clone(),
            read_key(&bedrock_cfg.access_key_env, "bedrock"),
            std::env::var(&bedrock_cfg.secret_key_env).unwrap_or_default(),
            optional_env(&bedrock_cfg.session_token_env),
        )
        .context("failed to build Bedrock connector")?,
    );

    let connectors = HashMap::from([
        ("openai".to_string(), openai),
        ("anthropic".to_string(), anthropic),
        ("azure".to_string(), azure),
        ("bedrock".to_string(), bedrock),
    ]);
    let registry =
        Registry::new(connectors, &config.models).context("failed to build model registry")?;
    if !config.models.is_empty() {
        tracing::info!(count = config.models.len(), "loaded model registry");
    }
    Ok(Arc::new(registry))
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

    let connector = build_router(&config)?;
    let admin_token = read_admin_token(&config);

    let keys = Arc::new(KeyStore::new(config.auth.keys.clone()));
    if keys.is_empty() {
        tracing::warn!("no virtual keys configured; the proxy API is unauthenticated");
    } else {
        tracing::info!(count = keys.len(), "loaded virtual keys");
    }

    let pricing = Arc::new(config.pricing.clone());

    let rate_limiter = Arc::new(RateLimiter::default());
    let spend_tracker = Arc::new(SpendTracker::default());

    let cache = if config.cache.enabled {
        let size_bytes = config.cache.size_mb.saturating_mul(1024 * 1024);
        let ttl = std::time::Duration::from_secs(config.cache.ttl_seconds);
        tracing::info!(
            size_mb = config.cache.size_mb,
            ttl_seconds = config.cache.ttl_seconds,
            "response cache enabled"
        );
        Some(Arc::new(ResponseCache::new(size_bytes, ttl)))
    } else {
        None
    };

    let models = Arc::new(
        config
            .models
            .iter()
            .map(|m| proxy::RegisteredModel {
                id: m.name.clone(),
                provider: m.primary.provider.clone(),
            })
            .collect::<Vec<_>>(),
    );

    let plugins: PluginChain = Arc::new(
        config
            .plugins
            .iter()
            .map(|p| -> Box<dyn Plugin> {
                match p {
                    PluginConfig::PiiRedaction(c) => {
                        Box::new(PiiRedactionPlugin::new(&c.patterns, c.replacement.clone()))
                    }
                }
            })
            .collect(),
    );
    if !config.plugins.is_empty() {
        tracing::info!(count = config.plugins.len(), "loaded inline plugins");
    }

    let state = AppState::new(
        config,
        admin_token,
        keys.clone(),
        rate_limiter.clone(),
        spend_tracker.clone(),
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

    let proxy_app = proxy::router(
        connector,
        keys,
        pricing,
        cache,
        rate_limiter,
        spend_tracker,
        models,
        plugins,
    );
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
