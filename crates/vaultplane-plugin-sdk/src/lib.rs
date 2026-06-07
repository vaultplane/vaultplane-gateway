// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! SDK and WIT contract for VaultPlane Gateway inline plugins.
//!
//! Plugins are WebAssembly components that implement the `inspect` interface defined
//! in `wit/world.wit`. The host loads any component implementing the contract and
//! calls it on the configured hook. The WIT in this crate is the shared contract:
//! the gateway generates host bindings from it, and plugin authors generate guest
//! bindings from it (see `plugins/pii-redaction` for a worked example). This crate
//! also exposes small helper types for authoring plugins in Rust.

/// The hook a plugin binds to. The MVP supports inspecting the inbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// Inspect the inbound request before it is forwarded upstream.
    InspectRequest,
}

impl Hook {
    /// The stable string identifier used for this hook in configuration.
    pub const fn as_str(self) -> &'static str {
        match self {
            Hook::InspectRequest => "inspect-request",
        }
    }
}
