// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Gateway configuration.
//!
//! Configuration is layered: defaults, then an optional YAML file, then environment
//! variables prefixed `VAULTPLANE_` (nested keys split on `__`). Command-line flags
//! are applied by the binary on top of the loaded configuration. The schema is
//! intentionally small today and grows with the runtime.

use std::collections::HashMap;
use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};

use crate::auth::VirtualKey;
use crate::error::{Error, Result};
use crate::plugin::PiiPattern;

/// Top-level gateway configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listener addresses for the proxy and admin APIs.
    pub listen: Listen,
    /// Upstream provider configuration.
    pub providers: Providers,
    /// Authentication configuration (admin token and virtual keys).
    pub auth: Auth,
    /// Virtual model registry: maps a model name to a primary provider and fallbacks.
    pub models: Vec<ModelConfig>,
    /// Pricing table for cost accounting, keyed by provider then model.
    pub pricing: Pricing,
    /// Exact-match response cache configuration.
    pub cache: CacheConfig,
    /// Inline request-inspection plugins, applied in order.
    pub plugins: Vec<PluginConfig>,
    /// Where configuration comes from: a local file (open source) or the Cloud
    /// control plane API.
    pub control_plane: ControlPlane,
    /// Graceful shutdown behavior.
    pub shutdown: Shutdown,
}

/// Configuration source selection.
///
/// The same binary serves both the open-source file-based path and the Cloud
/// control-plane API path; `mode` picks between them. The Cloud client is a stub
/// in this release: in `api` mode the gateway logs that the control plane is not
/// yet wired and serves from the last-known-good local configuration, so the
/// data plane keeps running whether or not a control plane is reachable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlPlane {
    /// Configuration source: `file` (default) or `api`.
    pub mode: ControlPlaneMode,
    /// Directory the file-based path reads configuration from.
    pub config_dir: String,
    /// Cloud control plane endpoint (used when `mode` is `api`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Environment variable holding the control plane token (used when `mode` is `api`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

impl Default for ControlPlane {
    fn default() -> Self {
        Self {
            mode: ControlPlaneMode::File,
            config_dir: "/etc/vaultplane".to_string(),
            endpoint: None,
            token_env: None,
        }
    }
}

/// The configuration source the gateway runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlPlaneMode {
    /// Read configuration from local files.
    #[default]
    File,
    /// Pull configuration from the Cloud control plane API (stubbed in this release).
    Api,
}

/// Graceful shutdown behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Shutdown {
    /// How long, in seconds, to let in-flight requests finish on SIGTERM before
    /// the listeners are forced closed.
    pub drain_timeout_seconds: u64,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self {
            drain_timeout_seconds: 30,
        }
    }
}

/// Listener addresses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Listen {
    /// Address the OpenAI-compatible proxy API binds to.
    pub address: String,
    /// Address the admin API binds to (health, status, metrics, reload).
    pub admin_address: String,
    /// Optional TLS for the proxy listener. When set, the proxy listener
    /// serves HTTPS using the given cert and key. The admin listener stays
    /// plain HTTP (intended for cluster-internal access).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:8080".to_string(),
            admin_address: "0.0.0.0:9091".to_string(),
            tls: None,
        }
    }
}

/// TLS material for the proxy listener.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Path to a PEM-encoded certificate chain (server cert first, then any
    /// intermediates).
    pub cert_path: String,
    /// Path to the PEM-encoded private key for the certificate.
    pub key_path: String,
    /// Optional path to a PEM-encoded CA bundle. When set, the proxy listener
    /// requires mutual TLS: clients must present a certificate that chains to
    /// one of these CAs, or the connection is refused at the handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ca_path: Option<String>,
}

/// Upstream provider configuration. One provider family for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Providers {
    /// OpenAI and OpenAI-compatible self-hosted servers.
    pub openai: OpenAiProvider,
    /// Anthropic.
    pub anthropic: AnthropicProvider,
    /// Azure OpenAI.
    pub azure: AzureProvider,
    /// AWS Bedrock.
    pub bedrock: BedrockProvider,
}

/// Configuration for the OpenAI-compatible provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiProvider {
    /// Base URL of the OpenAI-compatible API.
    pub base_url: String,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
        }
    }
}

/// Configuration for the Anthropic provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicProvider {
    /// Base URL of the Anthropic Messages API.
    pub base_url: String,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
        }
    }
}

/// Configuration for the Azure OpenAI provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AzureProvider {
    /// Resource base URL, for example `https://my-resource.openai.azure.com`.
    pub base_url: String,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
    /// Azure OpenAI API version (a query parameter on every request).
    pub api_version: String,
}

impl Default for AzureProvider {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key_env: "AZURE_OPENAI_API_KEY".to_string(),
            api_version: "2024-10-21".to_string(),
        }
    }
}

/// Configuration for the AWS Bedrock provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BedrockProvider {
    /// AWS region, for example `us-east-1`.
    pub region: String,
    /// Environment variable holding the AWS access key id.
    pub access_key_env: String,
    /// Environment variable holding the AWS secret access key.
    pub secret_key_env: String,
    /// Environment variable holding the AWS session token (optional credentials).
    pub session_token_env: String,
}

impl Default for BedrockProvider {
    fn default() -> Self {
        Self {
            region: "us-east-1".to_string(),
            access_key_env: "AWS_ACCESS_KEY_ID".to_string(),
            secret_key_env: "AWS_SECRET_ACCESS_KEY".to_string(),
            session_token_env: "AWS_SESSION_TOKEN".to_string(),
        }
    }
}

/// Authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Auth {
    /// Environment variable holding the admin API token. If unset, the admin API
    /// privileged endpoints are unauthenticated.
    pub admin_token_env: String,
    /// Virtual keys accepted by the proxy API. If empty, the proxy is unauthenticated.
    pub keys: Vec<VirtualKey>,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            admin_token_env: "VAULTPLANE_ADMIN_TOKEN".to_string(),
            keys: Vec::new(),
        }
    }
}

/// A virtual model: a primary provider route plus ordered fallbacks and a failover
/// policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// The virtual model name clients request.
    pub name: String,
    /// The primary provider route.
    pub primary: Route,
    /// Ordered fallback routes, tried in turn when the primary fails.
    #[serde(default)]
    pub fallbacks: Vec<Route>,
    /// HTTP status codes that trigger failover to the next route.
    #[serde(default = "default_retry_on")]
    pub retry_on: Vec<u16>,
    /// Per-attempt timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// A provider plus the upstream model name to send it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Provider name (for example `openai` or `anthropic`).
    pub provider: String,
    /// Upstream model name to send to that provider.
    pub model: String,
}

fn default_retry_on() -> Vec<u16> {
    vec![429, 500, 502, 503, 504]
}

fn default_timeout_ms() -> u64 {
    30_000
}

/// Pricing table used to compute per-request cost. Pricing is config-driven: an
/// empty table means cost is not reported.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    /// Per-provider, per-model token pricing.
    pub providers: HashMap<String, HashMap<String, ModelPricing>>,
}

/// The default pricing table shipped with the binary. Approximate list prices;
/// update by PR and override per deployment via the `pricing` config block.
const DEFAULT_PRICING: &str = include_str!("default_pricing.yaml");

impl Pricing {
    /// The pricing table baked into the binary.
    pub fn bundled() -> Pricing {
        Figment::from(Yaml::string(DEFAULT_PRICING))
            .extract()
            .unwrap_or_default()
    }

    /// A copy of this table with `overrides` applied on top: any provider/model
    /// present in `overrides` replaces the entry here, and new ones are added.
    pub fn overlaid_with(&self, overrides: &Pricing) -> Pricing {
        let mut providers = self.providers.clone();
        for (provider, models) in &overrides.providers {
            let entry = providers.entry(provider.clone()).or_default();
            for (model, price) in models {
                entry.insert(model.clone(), *price);
            }
        }
        Pricing { providers }
    }
}

/// USD price per 1,000 input and output tokens for a single model.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_1k_tokens_usd: f64,
    pub output_per_1k_tokens_usd: f64,
}

/// Exact-match response cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Whether the in-process response cache is enabled.
    pub enabled: bool,
    /// Maximum cache size in megabytes (byte-weighted by body length).
    pub size_mb: u64,
    /// Time-to-live for cached responses, in seconds.
    pub ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            size_mb: 256,
            ttl_seconds: 3600,
        }
    }
}

/// Configuration for a single inline plugin. The gateway ships one built-in
/// native plugin (`pii_redaction`) and loads third-party WebAssembly components
/// (`wasm`) through the wasmtime host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginConfig {
    PiiRedaction(PiiRedactionConfig),
    Wasm(WasmPluginConfig),
}

/// Configuration for a WebAssembly component plugin loaded by the wasmtime host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmPluginConfig {
    /// A stable identifier for the plugin, used in logs and audit events.
    pub name: String,
    /// Filesystem path to the `.wasm` component implementing the plugin contract.
    pub path: String,
    /// The hook the plugin binds to. Only `inspect-request` is supported today.
    #[serde(default = "default_plugin_hook")]
    pub hook: String,
    /// Hard wall-clock latency budget in milliseconds. The host traps the plugin
    /// if a single invocation runs past this budget.
    #[serde(default = "default_latency_budget_ms")]
    pub latency_budget_ms: u32,
    /// What the host does when the plugin overruns its budget, traps, or fails to
    /// instantiate: fail open (forward the request) or fail closed (reject it).
    #[serde(default = "default_on_timeout")]
    pub on_timeout: FailMode,
    /// Virtual model names this plugin applies to. Empty means every route.
    #[serde(default)]
    pub bind_routes: Vec<String>,
}

/// Circuit-breaker policy applied when a plugin overruns its budget or fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailMode {
    /// Forward the request unchanged when the plugin fails.
    FailOpen,
    /// Reject the request when the plugin fails.
    FailClosed,
}

fn default_plugin_hook() -> String {
    "inspect-request".to_string()
}

fn default_latency_budget_ms() -> u32 {
    5
}

fn default_on_timeout() -> FailMode {
    FailMode::FailOpen
}

/// Knobs for the PII redaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiRedactionConfig {
    #[serde(default = "default_pii_patterns")]
    pub patterns: Vec<PiiPattern>,
    #[serde(default = "default_redaction_replacement")]
    pub replacement: String,
}

impl Default for PiiRedactionConfig {
    fn default() -> Self {
        Self {
            patterns: default_pii_patterns(),
            replacement: default_redaction_replacement(),
        }
    }
}

fn default_pii_patterns() -> Vec<PiiPattern> {
    PiiPattern::ALL.to_vec()
}

fn default_redaction_replacement() -> String {
    "[REDACTED]".to_string()
}

impl Config {
    /// Load configuration by layering defaults, an optional YAML file, and
    /// environment variables (prefixed `VAULTPLANE_`, nested keys split on `__`).
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if let Some(path) = path {
            figment = figment.merge(Yaml::file(path));
        }
        figment = figment.merge(Env::prefixed("VAULTPLANE_").split("__"));
        figment.extract().map_err(|e| Error::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // `Jail::expect_with` requires a closure returning `Result<(), figment::Error>`,
    // and `figment::Error` is large; the lint is unavoidable here.
    #[allow(clippy::result_large_err)]
    fn defaults_then_yaml_then_env_layer_correctly() {
        figment::Jail::expect_with(|jail| {
            // Defaults only.
            let cfg = Config::load(None).unwrap();
            assert_eq!(cfg.listen.address, "0.0.0.0:8080");
            assert_eq!(cfg.listen.admin_address, "0.0.0.0:9091");
            assert_eq!(cfg.providers.openai.base_url, "https://api.openai.com");
            assert_eq!(cfg.providers.openai.api_key_env, "OPENAI_API_KEY");
            assert_eq!(
                cfg.providers.anthropic.base_url,
                "https://api.anthropic.com"
            );
            assert_eq!(cfg.providers.anthropic.api_key_env, "ANTHROPIC_API_KEY");
            assert_eq!(cfg.providers.azure.api_key_env, "AZURE_OPENAI_API_KEY");
            assert_eq!(cfg.providers.azure.api_version, "2024-10-21");
            assert!(cfg.providers.azure.base_url.is_empty());
            assert_eq!(cfg.providers.bedrock.region, "us-east-1");
            assert_eq!(cfg.providers.bedrock.access_key_env, "AWS_ACCESS_KEY_ID");
            assert_eq!(cfg.auth.admin_token_env, "VAULTPLANE_ADMIN_TOKEN");
            assert!(cfg.auth.keys.is_empty());
            assert!(cfg.models.is_empty());
            assert!(cfg.pricing.providers.is_empty());
            assert!(cfg.cache.enabled);
            assert_eq!(cfg.cache.size_mb, 256);
            assert_eq!(cfg.cache.ttl_seconds, 3600);
            assert!(cfg.plugins.is_empty());

            // A YAML file overrides one field; the other keeps its default.
            jail.create_file("vp.yaml", "listen:\n  address: \"127.0.0.1:9000\"\n")?;
            let cfg = Config::load(Some(Path::new("vp.yaml"))).unwrap();
            assert_eq!(cfg.listen.address, "127.0.0.1:9000");
            assert_eq!(cfg.listen.admin_address, "0.0.0.0:9091");

            // Environment variables override on top of the file, including nested
            // provider fields.
            jail.set_env("VAULTPLANE_LISTEN__ADMIN_ADDRESS", "127.0.0.1:9100");
            jail.set_env(
                "VAULTPLANE_PROVIDERS__OPENAI__BASE_URL",
                "http://localhost:1234",
            );
            let cfg = Config::load(Some(Path::new("vp.yaml"))).unwrap();
            assert_eq!(cfg.listen.address, "127.0.0.1:9000");
            assert_eq!(cfg.listen.admin_address, "127.0.0.1:9100");
            assert_eq!(cfg.providers.openai.base_url, "http://localhost:1234");

            // The model registry parses, with failover defaults filled in.
            jail.create_file(
                "models.yaml",
                "models:\n  - name: smart\n    primary: { provider: openai, model: gpt-4o }\n    fallbacks:\n      - { provider: anthropic, model: claude-3-7-sonnet }\n",
            )?;
            let cfg = Config::load(Some(Path::new("models.yaml"))).unwrap();
            assert_eq!(cfg.models.len(), 1);
            assert_eq!(cfg.models[0].name, "smart");
            assert_eq!(cfg.models[0].primary.provider, "openai");
            assert_eq!(cfg.models[0].fallbacks.len(), 1);
            assert_eq!(cfg.models[0].retry_on, vec![429, 500, 502, 503, 504]);
            assert_eq!(cfg.models[0].timeout_ms, 30_000);

            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn parses_control_plane_and_shutdown() {
        figment::Jail::expect_with(|jail| {
            let cfg = Config::load(None).unwrap();
            assert_eq!(cfg.control_plane.mode, ControlPlaneMode::File);
            assert_eq!(cfg.control_plane.config_dir, "/etc/vaultplane");
            assert!(cfg.control_plane.endpoint.is_none());
            assert_eq!(cfg.shutdown.drain_timeout_seconds, 30);

            jail.create_file(
                "cp.yaml",
                "control_plane:\n  mode: api\n  endpoint: \"https://cloud.example\"\n  \
                 token_env: VAULTPLANE_CP_TOKEN\nshutdown:\n  drain_timeout_seconds: 5\n",
            )?;
            let cfg = Config::load(Some(Path::new("cp.yaml"))).unwrap();
            assert_eq!(cfg.control_plane.mode, ControlPlaneMode::Api);
            assert_eq!(
                cfg.control_plane.endpoint.as_deref(),
                Some("https://cloud.example")
            );
            assert_eq!(
                cfg.control_plane.token_env.as_deref(),
                Some("VAULTPLANE_CP_TOKEN")
            );
            assert_eq!(cfg.shutdown.drain_timeout_seconds, 5);
            Ok(())
        });
    }

    #[test]
    fn bundled_pricing_is_populated_and_overridable() {
        let bundled = Pricing::bundled();
        assert!(bundled.providers.contains_key("openai"));
        assert!(bundled.providers["openai"].contains_key("gpt-4o"));

        let mut over_models = HashMap::new();
        over_models.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 1.0,
                output_per_1k_tokens_usd: 2.0,
            },
        );
        over_models.insert(
            "custom-model".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 0.1,
                output_per_1k_tokens_usd: 0.2,
            },
        );
        let overrides = Pricing {
            providers: HashMap::from([("openai".to_string(), over_models)]),
        };

        let merged = bundled.overlaid_with(&overrides);
        // Override replaces the bundled entry, adds the new one, and leaves other
        // bundled providers intact.
        assert_eq!(
            merged.providers["openai"]["gpt-4o"].input_per_1k_tokens_usd,
            1.0
        );
        assert!(merged.providers["openai"].contains_key("custom-model"));
        assert!(merged.providers["anthropic"].contains_key("claude-3-7-sonnet"));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn parses_wasm_plugins_with_defaults_and_overrides() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "plugins.yaml",
                "plugins:\n  \
                 - type: wasm\n    name: pii\n    path: /etc/vp/pii.wasm\n  \
                 - type: wasm\n    name: guard\n    path: /etc/vp/guard.wasm\n    \
                 hook: inspect-request\n    latency_budget_ms: 10\n    \
                 on_timeout: fail-closed\n    bind_routes: [chat-default]\n",
            )?;
            let cfg = Config::load(Some(Path::new("plugins.yaml"))).unwrap();
            assert_eq!(cfg.plugins.len(), 2);

            // First entry relies on every default.
            let PluginConfig::Wasm(first) = &cfg.plugins[0] else {
                panic!("expected a wasm plugin");
            };
            assert_eq!(first.name, "pii");
            assert_eq!(first.path, "/etc/vp/pii.wasm");
            assert_eq!(first.hook, "inspect-request");
            assert_eq!(first.latency_budget_ms, 5);
            assert_eq!(first.on_timeout, FailMode::FailOpen);
            assert!(first.bind_routes.is_empty());

            // Second entry overrides every knob.
            let PluginConfig::Wasm(second) = &cfg.plugins[1] else {
                panic!("expected a wasm plugin");
            };
            assert_eq!(second.latency_budget_ms, 10);
            assert_eq!(second.on_timeout, FailMode::FailClosed);
            assert_eq!(second.bind_routes, vec!["chat-default".to_string()]);

            Ok(())
        });
    }
}
