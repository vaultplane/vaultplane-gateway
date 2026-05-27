//! Gateway configuration.
//!
//! Configuration is layered: defaults, then an optional YAML file, then environment
//! variables prefixed `VAULTPLANE_` (nested keys split on `__`). Command-line flags
//! are applied by the binary on top of the loaded configuration. The schema is
//! intentionally small today and grows with the runtime.

use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};

use crate::auth::VirtualKey;
use crate::error::{Error, Result};

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
}

/// Listener addresses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Listen {
    /// Address the OpenAI-compatible proxy API binds to.
    pub address: String,
    /// Address the admin API binds to (health, status, metrics, reload).
    pub admin_address: String,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:8080".to_string(),
            admin_address: "0.0.0.0:9091".to_string(),
        }
    }
}

/// Upstream provider configuration. One provider family for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Providers {
    /// OpenAI and OpenAI-compatible self-hosted servers.
    pub openai: OpenAiProvider,
    /// Anthropic.
    pub anthropic: AnthropicProvider,
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
            assert_eq!(cfg.auth.admin_token_env, "VAULTPLANE_ADMIN_TOKEN");
            assert!(cfg.auth.keys.is_empty());

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

            Ok(())
        });
    }
}
