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

use crate::error::{Error, Result};

/// Top-level gateway configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listener addresses for the proxy and admin APIs.
    pub listen: Listen,
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

            // A YAML file overrides one field; the other keeps its default.
            jail.create_file("vp.yaml", "listen:\n  address: \"127.0.0.1:9000\"\n")?;
            let cfg = Config::load(Some(Path::new("vp.yaml"))).unwrap();
            assert_eq!(cfg.listen.address, "127.0.0.1:9000");
            assert_eq!(cfg.listen.admin_address, "0.0.0.0:9091");

            // An environment variable overrides on top of the file.
            jail.set_env("VAULTPLANE_LISTEN__ADMIN_ADDRESS", "127.0.0.1:9100");
            let cfg = Config::load(Some(Path::new("vp.yaml"))).unwrap();
            assert_eq!(cfg.listen.address, "127.0.0.1:9000");
            assert_eq!(cfg.listen.admin_address, "127.0.0.1:9100");

            Ok(())
        });
    }
}
