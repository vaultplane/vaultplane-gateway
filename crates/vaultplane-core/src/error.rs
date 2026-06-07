// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Error types for the data plane core.

use thiserror::Error;

/// Errors produced by the gateway core. This enum will grow as the runtime lands.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration was missing or invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// A provider connector failed (network error, decode error, malformed
    /// response, or any other upstream failure that is not a clean timeout).
    /// The proxy surfaces this as a 502 Bad Gateway.
    #[error("provider error: {0}")]
    Provider(String),

    /// An upstream request did not complete within the configured timeout.
    /// The proxy surfaces this as a 504 Gateway Timeout so clients (and
    /// observability) can distinguish slow upstreams from broken ones.
    #[error("upstream timed out: {0}")]
    UpstreamTimeout(String),
}

/// Convenience result type for the core crate.
pub type Result<T> = std::result::Result<T, Error>;
