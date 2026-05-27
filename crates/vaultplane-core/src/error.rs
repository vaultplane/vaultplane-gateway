//! Error types for the data plane core.

use thiserror::Error;

/// Errors produced by the gateway core. This enum will grow as the runtime lands.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration was missing or invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// A provider connector failed.
    #[error("provider error: {0}")]
    Provider(String),
}

/// Convenience result type for the core crate.
pub type Result<T> = std::result::Result<T, Error>;
