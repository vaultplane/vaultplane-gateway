//! Shared types and runtime building blocks for the VaultPlane Gateway data plane.
//!
//! This crate is the home for the pieces the gateway binary and the `vaultplane-ctl`
//! CLI both depend on: configuration, the provider connector contract, error types,
//! and (later) virtual key, cache, and telemetry plumbing. The runtime is not yet
//! implemented; the items here are minimal scaffolding that establishes the module
//! layout the data plane will grow into.

pub mod config;
pub mod error;
pub mod provider;

pub use error::{Error, Result};

/// The crate version, sourced from Cargo at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
