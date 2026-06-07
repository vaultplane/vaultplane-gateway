// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Hot-swappable runtime state.
//!
//! Everything the request handler reads off the wire is collected into a single
//! [`Runtime`]. Handlers reach it through a [`RuntimeHandle`] (an `Arc<ArcSwap>`),
//! so a configuration reload swaps the whole bundle atomically: in-flight
//! requests keep using the snapshot they already loaded, and the next request
//! sees the new one.
//!
//! The keystore, rate limiter, and spend tracker are deliberately NOT part of
//! the swap. Virtual keys are now sourced from the admin API (issued through
//! `POST /admin/keys`), and rate-limit and spend buckets accumulate per key
//! over time. Replacing them on every reload would drop admin-issued keys and
//! reset usage counters, which is surprising and unsafe.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use axum_server::tls_rustls::RustlsConfig;
use vaultplane_core::cache::ResponseCache;
use vaultplane_core::config::{Config, Pricing};
use vaultplane_core::plugin::{PiiRedactionPlugin, Plugin, PluginChain};
use vaultplane_core::provider::Connector;
use vaultplane_core::provider::anthropic::AnthropicConnector;
use vaultplane_core::provider::azure::AzureConnector;
use vaultplane_core::provider::bedrock::BedrockConnector;
use vaultplane_core::provider::openai::OpenAiConnector;
use vaultplane_core::provider::registry::Registry;

use vaultplane_core::config::PluginConfig;

/// A virtual model entry surfaced from the registry by `GET /v1/models`.
#[derive(Clone, Debug)]
pub struct RegisteredModel {
    pub id: String,
    pub provider: String,
}

/// The bundle of state that gets swapped on a configuration reload.
pub struct Runtime {
    pub connector: Arc<dyn Connector>,
    pub pricing: Arc<Pricing>,
    pub cache: Option<Arc<ResponseCache>>,
    pub plugins: PluginChain,
    pub models: Arc<Vec<RegisteredModel>>,
}

/// Shared handle over a [`Runtime`]. Clone-cheap; readers call `.load()` once
/// at the top of each request and the writer (the reload path) calls
/// `.store(Arc::new(new))`.
pub type RuntimeHandle = Arc<ArcSwap<Runtime>>;

/// Wrap a freshly built runtime in a swappable handle.
pub fn handle(runtime: Runtime) -> RuntimeHandle {
    Arc::new(ArcSwap::from_pointee(runtime))
}

/// Reload the config from `config_path` and swap it into `handle`. When a
/// `rustls` config is provided and the new configuration still has a `tls:`
/// block, the cert and key are re-read and swapped in place at the same time.
///
/// Validates the new configuration (by parsing and constructing every
/// connector from scratch) and reloads TLS BEFORE swapping the runtime. On
/// any failure the old runtime and the old TLS material both stay in place:
/// in-flight requests are unaffected and the gateway continues to serve.
///
/// Two structural changes are NOT applied on reload (they require a restart):
///
/// * Adding `tls:` to a gateway started without it. The proxy listener is
///   bound at startup; adding TLS means rebinding.
/// * Removing `tls:` from a gateway that was started with it. Same reason.
///
/// Both are logged as warnings and the rest of the reload proceeds.
pub async fn reload(
    handle: &RuntimeHandle,
    config_path: Option<&std::path::Path>,
    rustls: Option<&RustlsConfig>,
) -> anyhow::Result<()> {
    let config = Config::load(config_path).context("failed to load configuration")?;
    let new_runtime = build_runtime(&config).context("failed to build runtime from new config")?;

    match (rustls, config.listen.tls.as_ref()) {
        (Some(rustls), Some(tls)) => {
            crate::tls::reload_certs(rustls, tls).await?;
        }
        (None, Some(_)) => {
            tracing::warn!(
                "config now sets listen.tls but the gateway started without TLS; \
                 add or remove TLS requires a restart"
            );
        }
        (Some(_), None) => {
            tracing::warn!(
                "config no longer sets listen.tls but the gateway started with TLS; \
                 the existing cert remains in use until restart"
            );
        }
        (None, None) => {}
    }

    handle.store(Arc::new(new_runtime));
    Ok(())
}

/// Build a [`Runtime`] from a parsed [`Config`].
///
/// Reads provider API keys from the environment variables named in the config.
/// Logs a warning (but does not fail) when a referenced env var is unset, so an
/// operator can bring the gateway up with only the providers they intend to
/// use, and unconfigured providers cleanly return errors when called.
pub fn build_runtime(config: &Config) -> anyhow::Result<Runtime> {
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
    let connector: Arc<dyn Connector> = Arc::new(registry);

    let pricing = Arc::new(config.pricing.clone());

    let cache = if config.cache.enabled {
        let size_bytes = config.cache.size_mb.saturating_mul(1024 * 1024);
        let ttl = std::time::Duration::from_secs(config.cache.ttl_seconds);
        Some(Arc::new(ResponseCache::new(size_bytes, ttl)))
    } else {
        None
    };

    let models = Arc::new(
        config
            .models
            .iter()
            .map(|m| RegisteredModel {
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

    Ok(Runtime {
        connector,
        pricing,
        cache,
        plugins,
        models,
    })
}

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

fn optional_env(var: &str) -> Option<String> {
    let value = std::env::var(var).unwrap_or_default();
    if value.is_empty() { None } else { Some(value) }
}
