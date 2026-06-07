// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Cloud control plane client.
//!
//! The same binary serves both the open-source file-based configuration path and
//! the Cloud control-plane API path, selected by `control_plane.mode`. The Cloud
//! client is a stub in this release: in `api` mode the gateway logs that the
//! control plane is not yet wired and runs from the local (last-known-good)
//! configuration. The data plane therefore keeps serving traffic whether or not
//! a control plane is reachable, which is the architectural guarantee the full
//! client will preserve when it lands.

use vaultplane_core::config::{ControlPlane, ControlPlaneMode};

/// Apply the configured control-plane mode at startup. A no-op in file mode;
/// a logged stub in api mode.
pub fn bootstrap(config: &ControlPlane) {
    match config.mode {
        ControlPlaneMode::File => {
            tracing::info!(
                config_dir = %config.config_dir,
                "control plane: file mode"
            );
        }
        ControlPlaneMode::Api => {
            tracing::warn!(
                endpoint = config.endpoint.as_deref().unwrap_or("(unset)"),
                "control plane: api mode is a stub in this release; serving from \
                 last-known-good local configuration. The data plane keeps running \
                 if the control plane is unreachable."
            );
        }
    }
}
