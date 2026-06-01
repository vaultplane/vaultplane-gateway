//! Inline request inspection plugins.
//!
//! The plugin host runs a chain of plugins on each chat request before the gateway
//! dispatches to the provider. A plugin can pass the request through, modify it,
//! or reject it with a status code and reason. The shape mirrors the
//! `vaultplane:plugin/inspect.inspect-request` WIT contract in
//! `crates/vaultplane-plugin-sdk/wit/world.wit`, so a future WebAssembly component
//! host can slot in behind the same trait.
//!
//! The reference PII redaction implementation here is native Rust. Shipping it as
//! a separate WebAssembly component (as the spec ultimately calls for) requires
//! adding a wasmtime host and cross-compiling the plugin to `wasm32-wasip2`; that
//! is a follow-up. The redaction patterns and the integration point on the
//! request path are the same in both cases.

use std::sync::Arc;

use bytes::Bytes;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// A request as seen by an inline plugin before the gateway forwards it.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// The action a plugin returns for a request.
#[derive(Debug)]
pub enum Decision {
    /// Forward the request unchanged.
    Pass,
    /// Forward a modified request.
    Modify(PluginRequest),
    /// Reject the request with the given reason and status code.
    Reject(RejectInfo),
}

#[derive(Debug, Clone)]
pub struct RejectInfo {
    pub reason: String,
    pub status_code: u16,
}

/// The contract every inline plugin implements.
pub trait Plugin: Send + Sync {
    /// A stable identifier for the plugin, for example `pii-redaction`.
    fn name(&self) -> &str;

    /// Inspect an inbound request and decide what the gateway should do with it.
    fn inspect_request(&self, request: &PluginRequest) -> Decision;
}

/// An ordered chain of inline plugins, suitable for cheap cloning into request
/// state.
pub type PluginChain = Arc<Vec<Box<dyn Plugin>>>;

/// Classes of personally identifiable information the reference plugin can redact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiPattern {
    Ssn,
    CreditCard,
    PhoneUs,
    Email,
}

impl PiiPattern {
    /// Every pattern the reference plugin supports.
    pub const ALL: &'static [PiiPattern] = &[
        PiiPattern::Ssn,
        PiiPattern::CreditCard,
        PiiPattern::PhoneUs,
        PiiPattern::Email,
    ];

    fn regex(self) -> &'static str {
        match self {
            PiiPattern::Ssn => r"\b\d{3}-\d{2}-\d{4}\b",
            PiiPattern::CreditCard => r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b",
            PiiPattern::PhoneUs => r"\b\d{3}[-.]\d{3}[-.]\d{4}\b",
            PiiPattern::Email => r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        }
    }
}

/// The reference PII redaction plugin.
///
/// Walks `messages[].content` strings in an OpenAI Chat Completions request body
/// and replaces matches of each configured pattern with the configured replacement.
pub struct PiiRedactionPlugin {
    patterns: Vec<Regex>,
    replacement: String,
}

impl PiiRedactionPlugin {
    /// Build a plugin from the given patterns and replacement string.
    pub fn new(patterns: &[PiiPattern], replacement: impl Into<String>) -> Self {
        let regexes = patterns
            .iter()
            .filter_map(|p| Regex::new(p.regex()).ok())
            .collect();
        Self {
            patterns: regexes,
            replacement: replacement.into(),
        }
    }

    fn redact(&self, text: &str) -> String {
        let mut result = text.to_string();
        for pattern in &self.patterns {
            result = pattern
                .replace_all(&result, self.replacement.as_str())
                .into_owned();
        }
        result
    }
}

impl Default for PiiRedactionPlugin {
    fn default() -> Self {
        Self::new(PiiPattern::ALL, "[REDACTED]")
    }
}

impl Plugin for PiiRedactionPlugin {
    fn name(&self) -> &str {
        "pii-redaction"
    }

    fn inspect_request(&self, request: &PluginRequest) -> Decision {
        let mut value: serde_json::Value = match serde_json::from_slice(&request.body) {
            Ok(v) => v,
            Err(_) => return Decision::Pass,
        };

        let mut modified = false;
        if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages.iter_mut() {
                if let Some(content_val) = message.get_mut("content") {
                    let replacement = content_val
                        .as_str()
                        .map(|s| self.redact(s))
                        .filter(|new| Some(new.as_str()) != content_val.as_str());
                    if let Some(new) = replacement {
                        *content_val = serde_json::Value::String(new);
                        modified = true;
                    }
                }
            }
        }

        if !modified {
            return Decision::Pass;
        }

        match serde_json::to_vec(&value) {
            Ok(bytes) => Decision::Modify(PluginRequest {
                method: request.method.clone(),
                path: request.path.clone(),
                headers: request.headers.clone(),
                body: Bytes::from(bytes),
            }),
            Err(_) => Decision::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(body: &str) -> PluginRequest {
        PluginRequest {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: Vec::new(),
            body: Bytes::copy_from_slice(body.as_bytes()),
        }
    }

    #[test]
    fn redacts_ssn_email_and_phone_numbers() {
        let plugin = PiiRedactionPlugin::default();
        let body = r#"{"messages":[{"role":"user","content":"My SSN is 123-45-6789, email me at foo@example.com or call 555-123-4567"}]}"#;
        let decision = plugin.inspect_request(&req(body));
        let Decision::Modify(modified) = decision else {
            panic!("expected Modify");
        };
        let text = String::from_utf8(modified.body.to_vec()).unwrap();
        assert!(
            !text.contains("123-45-6789"),
            "SSN should be redacted: {text}"
        );
        assert!(
            !text.contains("foo@example.com"),
            "email should be redacted: {text}"
        );
        assert!(
            !text.contains("555-123-4567"),
            "phone should be redacted: {text}"
        );
        assert!(text.contains("[REDACTED]"), "missing marker: {text}");
    }

    #[test]
    fn passes_through_unchanged_when_no_pii_is_present() {
        let plugin = PiiRedactionPlugin::default();
        let body = r#"{"messages":[{"role":"user","content":"hello world"}]}"#;
        match plugin.inspect_request(&req(body)) {
            Decision::Pass => {}
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn passes_through_when_body_is_not_json() {
        let plugin = PiiRedactionPlugin::default();
        match plugin.inspect_request(&req("not json")) {
            Decision::Pass => {}
            other => panic!("expected Pass, got {other:?}"),
        }
    }
}
