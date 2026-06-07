// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the WebAssembly plugin host against the real reference
//! PII redaction component.
//!
//! The component is built separately for `wasm32-wasip2` (see
//! `plugins/pii-redaction/README.md`). These tests locate the artifact via the
//! `VAULTPLANE_PII_WASM` environment variable, falling back to the in-tree build
//! path. When the artifact is absent (the plugin has not been built), each test
//! skips with a notice rather than failing, so `cargo test` stays green on a host
//! without the wasm toolchain. CI builds the component and runs these for real.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use vaultplane_core::config::FailMode;
use vaultplane_core::plugin::wasm::WasmPlugin;
use vaultplane_core::plugin::{Decision, Plugin, PluginRequest};

/// Locate the built PII component, or `None` if it has not been built.
fn artifact() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VAULTPLANE_PII_WASM") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    let in_tree = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/pii-redaction/target/wasm32-wasip2/release/pii_redaction.wasm");
    in_tree.exists().then_some(in_tree)
}

/// Load the plugin with the given budget and fail mode, or skip (returning
/// `None`) if the artifact is missing.
fn load(budget_ms: u32, on_timeout: FailMode) -> Option<WasmPlugin> {
    let path = match artifact() {
        Some(path) => path,
        None => {
            eprintln!(
                "skipping: PII wasm component not built; build it or set VAULTPLANE_PII_WASM"
            );
            return None;
        }
    };
    Some(
        WasmPlugin::load(
            "pii-redaction",
            path.to_string_lossy(),
            budget_ms,
            on_timeout,
        )
        .expect("plugin should load"),
    )
}

fn request(body: &str) -> PluginRequest {
    PluginRequest {
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: Vec::new(),
        body: Bytes::from(body.to_string()),
    }
}

#[test]
fn redacts_the_four_pii_classes() {
    let Some(plugin) = load(1000, FailMode::FailOpen) else {
        return;
    };
    let body = r#"{"messages":[{"role":"user","content":"SSN 123-45-6789, email foo@example.com, call 555-123-4567, card 4111 1111 1111 1111"}]}"#;

    let Decision::Modify(modified) = plugin.inspect_request(&request(body)) else {
        panic!("expected the body to be modified");
    };

    let text = String::from_utf8(modified.body.to_vec()).unwrap();
    assert!(!text.contains("123-45-6789"), "SSN not redacted: {text}");
    assert!(
        !text.contains("foo@example.com"),
        "email not redacted: {text}"
    );
    assert!(!text.contains("555-123-4567"), "phone not redacted: {text}");
    assert!(
        !text.contains("4111 1111 1111 1111"),
        "card not redacted: {text}"
    );
    assert!(text.contains("[REDACTED]"), "missing marker: {text}");
}

#[test]
fn passes_clean_and_non_json_bodies_through() {
    let Some(plugin) = load(1000, FailMode::FailOpen) else {
        return;
    };

    let clean = r#"{"messages":[{"role":"user","content":"hello world"}]}"#;
    assert!(
        matches!(plugin.inspect_request(&request(clean)), Decision::Pass),
        "a clean body should pass through"
    );

    assert!(
        matches!(plugin.inspect_request(&request("not json")), Decision::Pass),
        "a non-JSON body should pass through"
    );
}

/// A body large enough that scanning it overruns a 1ms budget. The size is
/// generous so the test is not timing-sensitive on fast machines.
fn oversized_body_with_pii() -> String {
    let filler = "lorem ipsum dolor sit amet ".repeat(200_000); // a few MB
    format!(r#"{{"messages":[{{"role":"user","content":"SSN 123-45-6789 {filler}"}}]}}"#)
}

#[test]
fn budget_overrun_fails_open_by_forwarding_unmodified() {
    let Some(plugin) = load(1, FailMode::FailOpen) else {
        return;
    };
    // With a 1ms budget the scan traps; failing open forwards the original body,
    // so the SSN is NOT redacted (a completed run would have returned Modify).
    let decision = plugin.inspect_request(&request(&oversized_body_with_pii()));
    assert!(
        matches!(decision, Decision::Pass),
        "a budget overrun with fail-open should pass the request through"
    );
}

#[test]
fn budget_overrun_fails_closed_by_rejecting() {
    let Some(plugin) = load(1, FailMode::FailClosed) else {
        return;
    };
    let decision = plugin.inspect_request(&request(&oversized_body_with_pii()));
    match decision {
        Decision::Reject(info) => assert_eq!(info.status_code, 403),
        other => panic!("a budget overrun with fail-closed should reject, got {other:?}"),
    }
}
