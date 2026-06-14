// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Map Control Node wire DTOs ([`super::dto`]) into the Gateway's internal
//! config and key types.
//!
//! The `/config` bundle carries only the control-plane-owned slices of config
//! (models, providers, pricing, plugins, cache). Node-local settings (listen
//! addresses, admin token, the `control_plane` block itself, shutdown) are NOT
//! distributed, so a fetched bundle is overlaid onto the gateway's local base
//! config: distributed fields replace the base, everything else is preserved.

use vaultplane_core::auth::{self, VirtualKey};
use vaultplane_core::config::{
    AzureProvider, CacheConfig, Config, ModelConfig, ModelPricing, OpenAiProvider, PiiRedactionConfig,
    Pricing, Route, WasmPluginConfig,
};
use vaultplane_core::config::{FailMode, PluginConfig};
use vaultplane_core::plugin::PiiPattern;

use super::dto;

// Mirror the serde defaults on the Gateway's own `ModelConfig` so a bundle that
// omits these gets the same behavior as a local YAML file that omits them.
const DEFAULT_RETRY_ON: [u16; 5] = [429, 500, 502, 503, 504];
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WASM_LATENCY_BUDGET_MS: u32 = 5;
const DEFAULT_PII_REPLACEMENT: &str = "[REDACTED]";

/// Overlay a fetched config bundle onto the gateway's local base config.
///
/// Returns a new [`Config`] ready to hand to `runtime::build_runtime`. The base
/// is cloned so the caller's local config (with its listen/admin/control_plane
/// settings) is the source of everything the bundle does not carry.
pub fn apply_config(base: &Config, bundle: dto::GatewayConfig) -> Config {
    let mut config = base.clone();

    config.models = bundle.models.into_iter().map(map_model).collect();
    apply_providers(&mut config, bundle.providers);
    if let Some(pricing) = bundle.pricing {
        config.pricing = map_pricing(pricing);
    }
    if let Some(cache) = bundle.cache {
        apply_cache(&mut config, cache);
    }
    config.plugins = bundle.plugins.into_iter().map(map_plugin).collect();

    config
}

fn map_model(m: dto::ModelConfig) -> ModelConfig {
    ModelConfig {
        name: m.name,
        primary: map_route(m.primary),
        fallbacks: m.fallbacks.into_iter().map(map_route).collect(),
        retry_on: if m.retry_on.is_empty() {
            DEFAULT_RETRY_ON.to_vec()
        } else {
            m.retry_on
        },
        timeout_ms: m.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
    }
}

fn map_route(r: dto::Route) -> Route {
    Route {
        provider: r.provider,
        model: r.model,
    }
}

/// Apply each provider block present in the bundle onto the base providers.
/// A provider the bundle omits keeps the base config; a field a present block
/// omits keeps the base field. Secrets are referenced by env-var name only.
fn apply_providers(config: &mut Config, p: dto::Providers) {
    if let Some(openai) = p.openai {
        let base = &mut config.providers.openai;
        *base = OpenAiProvider {
            base_url: openai.base_url.unwrap_or_else(|| base.base_url.clone()),
            api_key_env: openai.api_key_env.unwrap_or_else(|| base.api_key_env.clone()),
        };
    }
    if let Some(anthropic) = p.anthropic {
        let base = &mut config.providers.anthropic;
        base.base_url = anthropic.base_url.unwrap_or_else(|| base.base_url.clone());
        base.api_key_env = anthropic
            .api_key_env
            .unwrap_or_else(|| base.api_key_env.clone());
    }
    if let Some(azure) = p.azure {
        let base = &mut config.providers.azure;
        *base = AzureProvider {
            base_url: azure.base_url.unwrap_or_else(|| base.base_url.clone()),
            api_key_env: azure.api_key_env.unwrap_or_else(|| base.api_key_env.clone()),
            api_version: azure.api_version.unwrap_or_else(|| base.api_version.clone()),
        };
    }
    if let Some(bedrock) = p.bedrock {
        let base = &mut config.providers.bedrock;
        base.region = bedrock.region.unwrap_or_else(|| base.region.clone());
        base.access_key_env = bedrock
            .access_key_env
            .unwrap_or_else(|| base.access_key_env.clone());
        base.secret_key_env = bedrock
            .secret_key_env
            .unwrap_or_else(|| base.secret_key_env.clone());
        base.session_token_env = bedrock
            .session_token_env
            .unwrap_or_else(|| base.session_token_env.clone());
    }
}

fn map_pricing(p: dto::Pricing) -> Pricing {
    let providers = p
        .providers
        .into_iter()
        .map(|(provider, models)| {
            let models = models
                .into_iter()
                .map(|(model, rates)| {
                    (
                        model,
                        ModelPricing {
                            input_per_1k_tokens_usd: rates.input_per1k,
                            output_per_1k_tokens_usd: rates.output_per1k,
                        },
                    )
                })
                .collect();
            (provider, models)
        })
        .collect();
    Pricing { providers }
}

fn apply_cache(config: &mut Config, c: dto::CacheConfig) {
    let base = &mut config.cache;
    *base = CacheConfig {
        enabled: c.enabled.unwrap_or(base.enabled),
        size_mb: c.size_mb.unwrap_or(base.size_mb),
        ttl_seconds: c.ttl_seconds.unwrap_or(base.ttl_seconds),
    };
}

fn map_plugin(p: dto::Plugin) -> PluginConfig {
    match p {
        dto::Plugin::PiiRedaction(pii) => PluginConfig::PiiRedaction(PiiRedactionConfig {
            patterns: if pii.patterns.is_empty() {
                PiiPattern::ALL.to_vec()
            } else {
                pii.patterns
            },
            replacement: pii
                .replacement
                .unwrap_or_else(|| DEFAULT_PII_REPLACEMENT.to_string()),
        }),
        dto::Plugin::Wasm(w) => PluginConfig::Wasm(WasmPluginConfig {
            name: w.name,
            path: w.path,
            hook: w.hook,
            latency_budget_ms: w.latency_budget_ms.unwrap_or(DEFAULT_WASM_LATENCY_BUDGET_MS),
            on_timeout: w.on_timeout.unwrap_or(FailMode::FailOpen),
            bind_routes: w.bind_routes,
        }),
    }
}

/// Convert a fetched key set into the gateway's [`VirtualKey`] records. The
/// Control Node sends only the SHA-256 hash, which is exactly what the
/// [`KeyStore`](vaultplane_core::auth::KeyStore) authenticates against.
pub fn to_virtual_keys(keys: Vec<dto::VirtualKey>) -> Vec<VirtualKey> {
    keys.into_iter().map(map_key).collect()
}

fn map_key(k: dto::VirtualKey) -> VirtualKey {
    let (rate_limit_rps, spend_limit) = match k.limits {
        Some(limits) => (limits.requests_per_second, limits.spend_limit.map(map_spend)),
        None => (None, None),
    };
    VirtualKey {
        id: k.id,
        hash: k.hashed_key,
        team: k.scope.team,
        app: k.scope.app,
        env: k.scope.env,
        models: k.scope.allowed_models,
        rate_limit_rps,
        spend_limit,
        expires_at: k.expires_at,
    }
}

fn map_spend(s: dto::SpendLimit) -> auth::SpendLimit {
    auth::SpendLimit {
        amount_usd: s.amount_usd,
        period: match s.period {
            dto::SpendPeriod::Day => auth::Period::Day,
            dto::SpendPeriod::Week => auth::Period::Week,
            dto::SpendPeriod::Month => auth::Period::Month,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> dto::GatewayConfig {
        serde_json::from_str(
            r#"{
                "version": "v1",
                "models": [
                    { "name": "smart", "primary": { "provider": "openai", "model": "gpt-4o" },
                      "fallbacks": [ { "provider": "anthropic", "model": "claude-3-7-sonnet" } ] }
                ],
                "providers": {
                    "openai": { "baseUrl": "https://proxy.internal", "apiKeyEnv": "OAI" },
                    "bedrock": { "region": "eu-west-1" }
                },
                "cache": { "ttlSeconds": 120 },
                "plugins": [ { "type": "pii_redaction" } ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn overlays_distributed_fields_and_preserves_local_ones() {
        let mut base = Config::default();
        base.listen.address = "127.0.0.1:9999".to_string();
        base.auth.admin_token_env = "MY_ADMIN_TOKEN".to_string();

        let config = apply_config(&base, bundle());

        // Distributed fields are taken from the bundle.
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "smart");
        assert_eq!(config.models[0].primary.model, "gpt-4o");
        assert_eq!(config.models[0].fallbacks.len(), 1);
        // Omitted retry_on/timeout fall back to the same defaults as a YAML file.
        assert_eq!(config.models[0].retry_on, DEFAULT_RETRY_ON.to_vec());
        assert_eq!(config.models[0].timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(config.providers.openai.base_url, "https://proxy.internal");
        assert_eq!(config.providers.openai.api_key_env, "OAI");

        // A present provider block with omitted fields keeps base/default values.
        assert_eq!(config.providers.bedrock.region, "eu-west-1");
        assert_eq!(config.providers.bedrock.access_key_env, "AWS_ACCESS_KEY_ID");

        // A present cache block with one field overlays just that field.
        assert_eq!(config.cache.ttl_seconds, 120);
        assert!(config.cache.enabled, "enabled kept from base default");

        // Node-local fields are preserved from the base.
        assert_eq!(config.listen.address, "127.0.0.1:9999");
        assert_eq!(config.auth.admin_token_env, "MY_ADMIN_TOKEN");

        // PII plugin with no patterns gets the full built-in set.
        match &config.plugins[0] {
            PluginConfig::PiiRedaction(c) => {
                assert_eq!(c.patterns.len(), PiiPattern::ALL.len());
                assert_eq!(c.replacement, DEFAULT_PII_REPLACEMENT);
            }
            _ => panic!("expected a pii plugin"),
        }
    }

    #[test]
    fn maps_keys_including_limits_and_spend_period() {
        let set: dto::KeySet = serde_json::from_str(
            r#"{ "version": "k1", "keys": [
                { "id": "vp_a", "hashedKey": "h1",
                  "scope": { "team": "core", "env": "prod", "allowedModels": ["smart"] },
                  "limits": { "requestsPerSecond": 25, "spendLimit": { "amountUsd": 5.0, "period": "week" } } },
                { "id": "vp_b", "hashedKey": "h2" }
            ] }"#,
        )
        .unwrap();

        let keys = to_virtual_keys(set.keys);
        assert_eq!(keys.len(), 2);

        let a = &keys[0];
        assert_eq!(a.id, "vp_a");
        assert_eq!(a.hash, "h1");
        assert_eq!(a.team, "core");
        assert_eq!(a.models, vec!["smart".to_string()]);
        assert_eq!(a.rate_limit_rps, Some(25));
        let spend = a.spend_limit.expect("spend limit present");
        assert_eq!(spend.amount_usd, 5.0);
        assert!(matches!(spend.period, auth::Period::Week));

        // A key with no limits maps to no rate limit and no spend limit.
        let b = &keys[1];
        assert_eq!(b.rate_limit_rps, None);
        assert!(b.spend_limit.is_none());
    }
}
