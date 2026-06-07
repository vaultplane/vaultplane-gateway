// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Reference PII redaction plugin for VaultPlane Gateway.
//!
//! A WebAssembly component implementing the `inspect` interface from the plugin
//! SDK. It walks the `messages[].content` strings of an OpenAI Chat Completions
//! request body and replaces matches of four PII pattern classes (US Social
//! Security numbers, US-format credit card numbers, US phone numbers, and email
//! addresses) with a fixed marker.
//!
//! The pattern set and replacement are fixed in this reference build. Passing
//! per-deployment plugin config into a component requires an additional WIT entry
//! point and is left for a follow-up; operators who need configurable patterns
//! today can use the gateway's built-in native redaction plugin instead.

// The wit-bindgen `generate!`/`export!` macros expand to component glue whose
// generated `unsafe fn`s perform unsafe work without inner `unsafe` blocks. That
// trips the Rust 2024 `unsafe_op_in_unsafe_fn` lint inside code we do not author.
#![allow(unsafe_op_in_unsafe_fn)]

use std::sync::OnceLock;

use regex::Regex;

wit_bindgen::generate!({
    world: "plugin",
    path: "../../crates/vaultplane-plugin-sdk/wit",
});

use exports::vaultplane::plugin::inspect::{Action, Guest, Request};

/// The PII pattern classes this plugin redacts, by stable name.
pub const PATTERNS: &[&str] = &["ssn", "credit-card", "phone-us", "email"];

const REPLACEMENT: &str = "[REDACTED]";

/// Compiled patterns, built once on first use. The expressions are constant and
/// known-valid, so compilation cannot fail in practice.
fn patterns() -> &'static [Regex] {
    static PATTERNS_CELL: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS_CELL.get_or_init(|| {
        [
            r"\b\d{3}-\d{2}-\d{4}\b",
            r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b",
            r"\b\d{3}[-.]\d{3}[-.]\d{4}\b",
            r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    })
}

/// Replace every configured pattern in `text` with the marker.
fn redact(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in patterns() {
        result = pattern.replace_all(&result, REPLACEMENT).into_owned();
    }
    result
}

struct Component;

impl Guest for Component {
    fn inspect_request(req: Request) -> Action {
        // Anything that is not a JSON object with a `messages` array passes
        // through untouched; this plugin only knows how to read chat bodies.
        let mut value: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(value) => value,
            Err(_) => return Action::Pass,
        };

        let mut modified = false;
        if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
            for message in messages.iter_mut() {
                if let Some(content) = message.get_mut("content") {
                    if let Some(text) = content.as_str() {
                        let redacted = redact(text);
                        if redacted != text {
                            *content = serde_json::Value::String(redacted);
                            modified = true;
                        }
                    }
                }
            }
        }

        if !modified {
            return Action::Pass;
        }

        match serde_json::to_vec(&value) {
            Ok(body) => Action::Modify(Request {
                method: req.method,
                path: req.path,
                headers: req.headers,
                body,
            }),
            // If re-serialization somehow fails, forward the original rather than
            // dropping the request.
            Err(_) => Action::Pass,
        }
    }
}

export!(Component);
