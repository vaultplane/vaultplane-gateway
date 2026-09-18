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
//! bundle that fails to build (see [`client`]). Because that fallback is
//! silent by design, the link's health is surfaced separately through the
//! [`status`] handle that `GET /admin/status` renders.

mod client;
mod dto;
mod map;
pub mod status;

use std::sync::Arc;
use std::time::Duration;

use vaultplane_core::auth::KeyStore;
use vaultplane_core::config::{Config, ControlPlaneMode};

use crate::runtime::RuntimeHandle;

pub use client::ControlNodeClient;
pub use status::StatusHandle;

/// Apply the configured control-plane mode at startup. A no-op (logging) in file
/// mode; in api mode it spawns the background client that keeps `runtime` and
/// `keys` in sync with the Control Node.
///
/// Returns the handle the admin API renders on `GET /admin/status`. In api
/// mode a prerequisite that stops the client from starting is recorded on it
/// as `degraded` / `not_started`, so a misconfigured deployment is visible
/// there and not only in the startup log.
pub fn start(config: &Config, runtime: RuntimeHandle, keys: Arc<KeyStore>) -> StatusHandle {
    let cp = &config.control_plane;
    match cp.mode {
        ControlPlaneMode::File => {
            tracing::info!(config_dir = %cp.config_dir, "control plane: file mode");
            StatusHandle::file()
        }
        ControlPlaneMode::Api => {
            let status = StatusHandle::api(cp.endpoint.clone().unwrap_or_default());

            // Any missing prerequisite is non-fatal: the data plane keeps serving
            // from its local last-known-good config rather than refusing to start.
            let Some(endpoint) = cp.endpoint.clone() else {
                not_started(&status, "control_plane.endpoint is unset");
                return status;
            };
            let Some(token_env) = cp.token_env.clone() else {
                not_started(&status, "control_plane.token_env is unset");
                return status;
            };
            let token = std::env::var(&token_env).unwrap_or_default();
            if token.is_empty() {
                not_started(&status, format!("token env var {token_env} is empty"));
                return status;
            }

            let client = match ControlNodeClient::new(&endpoint, token, status.clone()) {
                Ok(client) => client,
                Err(err) => {
                    not_started(&status, format!("failed to build api-mode client: {err:#}"));
                    return status;
                }
            };

            let interval = Duration::from_secs(cp.poll_interval_seconds.max(1));
            let base = config.clone();
            tracing::info!(%endpoint, "control plane: api mode; starting Control Node client");
            tokio::spawn(async move {
                client.run(runtime, keys, base, interval).await;
            });
            status
        }
    }
}

/// Api mode was requested but the client cannot start: log it, and record it
/// on the status handle so `/admin/status` shows why.
fn not_started(status: &StatusHandle, detail: impl Into<String>) {
    let detail = detail.into();
    tracing::warn!(
        %detail,
        "control plane: api mode client not started; serving from local last-known-good config"
    );
    status.not_started(detail);
}
