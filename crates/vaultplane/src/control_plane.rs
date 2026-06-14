// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Configuration source selection: local file vs the Cloud Control Node API.
//!
//! The same binary serves both the open-source file-based path and the Control
//! Node API path, selected by `control_plane.mode`. In `file` mode this is a
//! no-op (the gateway runs from its local config and hot-reloads from disk). In
//! `api` mode it spawns the [`client`] poll loop, which pulls config and keys
//! from the Control Node and swaps them into the running gateway.
//!
//! The data-plane survival guarantee holds in both modes: `file` mode never
//! depends on a control plane, and `api` mode keeps serving the last-known-good
//! runtime whenever the Control Node is unreachable, returns `401`, or sends a
//! bundle that fails to build (see [`client`]).

mod client;
mod dto;
mod map;

use std::sync::Arc;
use std::time::Duration;

use vaultplane_core::auth::KeyStore;
use vaultplane_core::config::{Config, ControlPlaneMode};

use crate::runtime::RuntimeHandle;

pub use client::ControlNodeClient;

/// Apply the configured control-plane mode at startup. A no-op (logging) in file
/// mode; in api mode it spawns the background client that keeps `runtime` and
/// `keys` in sync with the Control Node.
pub fn start(config: &Config, runtime: RuntimeHandle, keys: Arc<KeyStore>) {
    let cp = &config.control_plane;
    match cp.mode {
        ControlPlaneMode::File => {
            tracing::info!(config_dir = %cp.config_dir, "control plane: file mode");
        }
        ControlPlaneMode::Api => {
            // Any missing prerequisite is non-fatal: the data plane keeps serving
            // from its local last-known-good config rather than refusing to start.
            let Some(endpoint) = cp.endpoint.clone() else {
                tracing::warn!(
                    "control plane: api mode but control_plane.endpoint is unset; \
                     serving from local last-known-good config"
                );
                return;
            };
            let Some(token_env) = cp.token_env.clone() else {
                tracing::warn!(
                    "control plane: api mode but control_plane.token_env is unset; \
                     serving from local last-known-good config"
                );
                return;
            };
            let token = std::env::var(&token_env).unwrap_or_default();
            if token.is_empty() {
                tracing::warn!(
                    var = %token_env,
                    "control plane: api mode token env var is empty; \
                     serving from local last-known-good config"
                );
                return;
            }

            let client = match ControlNodeClient::new(&endpoint, token) {
                Ok(client) => client,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "control plane: failed to build api-mode client; \
                         serving from local last-known-good config"
                    );
                    return;
                }
            };

            let interval = Duration::from_secs(cp.poll_interval_seconds.max(1));
            let base = config.clone();
            tracing::info!(%endpoint, "control plane: api mode; starting Control Node client");
            tokio::spawn(async move {
                client.run(runtime, keys, base, interval).await;
            });
        }
    }
}
