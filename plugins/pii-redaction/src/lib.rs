//! Reference PII redaction plugin for VaultPlane Gateway.
//!
//! This plugin detects and redacts common personally identifiable information in an
//! inbound request: US Social Security numbers, US-format credit card numbers, US
//! phone numbers, and email addresses. It is built as a WebAssembly component that
//! implements the `inspect` interface from the plugin SDK.
//!
//! This is a placeholder. The redaction logic and WIT bindings land with the plugin
//! milestone. See README.md for how this crate is built.

/// The PII pattern classes this plugin redacts.
pub const PATTERNS: &[&str] = &["ssn", "credit-card", "phone-us", "email"];
