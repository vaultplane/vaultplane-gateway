// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Wire DTOs for the Control Node "Gateway Control API" (INT-1).
//!
//! These mirror the OpenAPI contract
//! (`vaultplane-control-plane/packages/api-contract/openapi/openapi.yaml`), which
//! is camelCase, while the Gateway's own config and key structs are snake_case.
//! Keeping a dedicated DTO layer lets the contract stay idiomatic for the
//! TypeScript client while the Gateway owns the mapping into its internal types
//! (see [`super::map`]). The DTOs deserialize the wire shape only; nothing here
//! enforces policy.
//!
//! Status: these DTOs track Gateway Control API v0.3.1. Everything the v0.3.1
//! contract added is implemented in this file: the `bedrock` provider config
//! (SigV4 credential env-var names plus region), the `pii_redaction` plugin
//! with `patterns` and `replacement`, the `onTimeout` enum on Wasm plugins, and
//! `Pricing` wrapped under `providers`.

use std::collections::HashMap;

use serde::Deserialize;
use vaultplane_core::config::FailMode;
use vaultplane_core::plugin::PiiPattern;

/// `GET /config` body: the effective config bundle for the calling gateway's
/// resolved `(group, environment)` assignment. Node-local settings (`listen`,
/// `shutdown`, `control_plane`) and OTLP are intentionally not distributed.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    /// Opaque version; mirrors the `/config` ETag. Used only for logging here
    /// (the ETag header is the cache key the client actually compares).
    pub version: String,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub providers: Providers,
    #[serde(default)]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub plugins: Vec<Plugin>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
}

/// A virtual model and its routing (primary plus ordered fallbacks).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub name: String,
    pub primary: Route,
    #[serde(default)]
    pub fallbacks: Vec<Route>,
    #[serde(default)]
    pub retry_on: Vec<u16>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// A concrete upstream target: a provider key plus the model id at that provider.
#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub provider: String,
    pub model: String,
}

/// Non-secret provider connection config. API keys and AWS credentials are
/// referenced by env-var NAME; no secret material is ever sent on the wire.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Providers {
    pub openai: Option<ProviderConfig>,
    pub anthropic: Option<ProviderConfig>,
    pub azure: Option<AzureProviderConfig>,
    pub bedrock: Option<BedrockProviderConfig>,
}

/// OpenAI / Anthropic shape: base URL plus the env-var name holding the API key.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// Azure adds an API version on top of the base provider shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureProviderConfig {
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_version: Option<String>,
}

/// Bedrock uses SigV4 with three credential env-var NAMES and a region, not a
/// single API key or base URL (v0.3.1 punch-list item P1).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockProviderConfig {
    pub region: Option<String>,
    pub access_key_env: Option<String>,
    pub secret_key_env: Option<String>,
    pub session_token_env: Option<String>,
}

/// `provider -> model -> rates`. Optional: the Gateway ships a bundled default
/// pricing table and overlays this when present.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub providers: HashMap<String, HashMap<String, ModelPricing>>,
}

/// USD rates per 1,000 input and output tokens for one model.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPricing {
    #[serde(default)]
    pub input_per1k: f64,
    #[serde(default)]
    pub output_per1k: f64,
}

/// Inline plugin binding. Tagged by `type`; snake_case discriminator matches the
/// Gateway's own enum (v0.3.1 punch-list item P2).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Plugin {
    PiiRedaction(PiiRedactionPlugin),
    Wasm(WasmPlugin),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PiiRedactionPlugin {
    #[serde(default)]
    pub patterns: Vec<PiiPattern>,
    #[serde(default)]
    pub replacement: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmPlugin {
    pub name: String,
    pub path: String,
    pub hook: String,
    #[serde(default)]
    pub latency_budget_ms: Option<u32>,
    #[serde(default)]
    pub on_timeout: Option<FailMode>,
    #[serde(default)]
    pub bind_routes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheConfig {
    pub enabled: Option<bool>,
    pub size_mb: Option<u64>,
    pub ttl_seconds: Option<u64>,
}

/// `GET /keys` body: the in-scope virtual key set, hashed only.
#[derive(Debug, Clone, Deserialize)]
pub struct KeySet {
    pub version: String,
    #[serde(default)]
    pub keys: Vec<VirtualKey>,
}

/// A virtual key as distributed by the Control Node. The raw secret never
/// leaves the Control Node; only the SHA-256 hex `hashedKey` is sent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualKey {
    pub id: String,
    pub hashed_key: String,
    #[serde(default)]
    pub scope: KeyScope,
    #[serde(default)]
    pub limits: Option<KeyLimits>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyScope {
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub allowed_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyLimits {
    #[serde(default)]
    pub requests_per_second: Option<u32>,
    #[serde(default)]
    pub spend_limit: Option<SpendLimit>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendLimit {
    pub amount_usd: f64,
    pub period: SpendPeriod,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpendPeriod {
    Day,
    Week,
    Month,
}

/// `GET /identity` body: the assignment the Control Node resolved for the
/// calling token. Informational; used for startup enrollment verification and
/// logging.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayIdentity {
    pub gateway_id: String,
    pub org_id: String,
    #[serde(default)]
    pub group: Option<String>,
    pub environment: String,
    #[serde(default)]
    pub config_version: Option<String>,
    #[serde(default)]
    pub keys_version: Option<String>,
}

/// `GET /watch` SSE event: signals which resource changed so the client can
/// re-pull `/config` or `/keys`.
///
/// The SSE `watch` stream is a planned follow-up to the polling MVP (the
/// contract has clients fall back to polling when the stream is unavailable), so
/// this typed surface is defined but not yet consumed.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeEvent {
    pub kind: ChangeKind,
    pub version: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Config,
    Keys,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_a_full_config_bundle() {
        let json = r#"{
            "version": "v42",
            "models": [
                {
                    "name": "smart",
                    "primary": { "provider": "openai", "model": "gpt-4o" },
                    "fallbacks": [ { "provider": "anthropic", "model": "claude-3-7-sonnet" } ],
                    "retryOn": [429, 503],
                    "timeoutMs": 20000
                }
            ],
            "providers": {
                "openai": { "baseUrl": "https://api.openai.com", "apiKeyEnv": "OPENAI_API_KEY" },
                "bedrock": { "region": "us-east-1", "accessKeyEnv": "AWS_ACCESS_KEY_ID",
                             "secretKeyEnv": "AWS_SECRET_ACCESS_KEY", "sessionTokenEnv": "AWS_SESSION_TOKEN" }
            },
            "plugins": [
                { "type": "pii_redaction", "patterns": ["ssn", "email"], "replacement": "[X]" },
                { "type": "wasm", "name": "guard", "path": "/etc/vp/guard.wasm",
                  "hook": "inspect-request", "latencyBudgetMs": 10, "onTimeout": "fail-closed",
                  "bindRoutes": ["smart"] }
            ],
            "cache": { "enabled": true, "sizeMb": 128, "ttlSeconds": 600 }
        }"#;

        let cfg: GatewayConfig = serde_json::from_str(json).expect("config bundle parses");
        assert_eq!(cfg.version, "v42");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "smart");
        assert_eq!(cfg.models[0].primary.model, "gpt-4o");
        assert_eq!(cfg.models[0].retry_on, vec![429, 503]);
        assert_eq!(cfg.models[0].timeout_ms, Some(20000));

        let bedrock = cfg.providers.bedrock.as_ref().expect("bedrock present");
        assert_eq!(bedrock.region.as_deref(), Some("us-east-1"));
        assert_eq!(
            bedrock.secret_key_env.as_deref(),
            Some("AWS_SECRET_ACCESS_KEY")
        );

        assert_eq!(cfg.plugins.len(), 2);
        match &cfg.plugins[0] {
            Plugin::PiiRedaction(p) => {
                assert_eq!(p.patterns.len(), 2);
                assert_eq!(p.replacement.as_deref(), Some("[X]"));
            }
            _ => panic!("expected pii_redaction first"),
        }
        match &cfg.plugins[1] {
            Plugin::Wasm(w) => {
                assert_eq!(w.name, "guard");
                assert_eq!(w.on_timeout, Some(FailMode::FailClosed));
                assert_eq!(w.bind_routes, vec!["smart".to_string()]);
            }
            _ => panic!("expected wasm second"),
        }
    }

    #[test]
    fn deserializes_a_key_set() {
        let json = r#"{
            "version": "k7",
            "keys": [
                {
                    "id": "vp_abc123def456",
                    "hashedKey": "deadbeef",
                    "scope": { "team": "core", "app": "web", "env": "prod", "allowedModels": ["smart"] },
                    "limits": { "requestsPerSecond": 50, "spendLimit": { "amountUsd": 100.0, "period": "month" } },
                    "expiresAt": "2026-12-31T23:59:59Z"
                }
            ]
        }"#;

        let set: KeySet = serde_json::from_str(json).expect("key set parses");
        assert_eq!(set.version, "k7");
        assert_eq!(set.keys.len(), 1);
        let k = &set.keys[0];
        assert_eq!(k.id, "vp_abc123def456");
        assert_eq!(k.hashed_key, "deadbeef");
        assert_eq!(k.scope.allowed_models, vec!["smart".to_string()]);
        let limits = k.limits.as_ref().unwrap();
        assert_eq!(limits.requests_per_second, Some(50));
        let spend = limits.spend_limit.unwrap();
        assert_eq!(spend.amount_usd, 100.0);
        assert!(matches!(spend.period, SpendPeriod::Month));
    }
}
