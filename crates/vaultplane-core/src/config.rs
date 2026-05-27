//! Gateway configuration.
//!
//! Configuration is layered (command-line flags, environment variables, a YAML
//! file, then defaults) and is reloadable at runtime. This module currently defines
//! a placeholder shape; the full schema lands with the runtime.

use serde::{Deserialize, Serialize};

/// Top-level gateway configuration. Placeholder shape pending the runtime.
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
