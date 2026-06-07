// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Structured audit log.
//!
//! The gateway records every administrative action and policy decision as a
//! structured event on the standard tracing pipeline: a virtual key created or
//! revoked, the configuration reloaded, a plugin loaded, a request rejected by a
//! plugin, and a provider failover. Each event is emitted under the
//! [`AUDIT_TARGET`] target and carries `vaultplane.audit = true` so operators can
//! filter the audit stream out of ordinary logs, plus the canonical fields
//! `action`, `actor`, `subject`, and `outcome` and any action-specific metadata.
//!
//! Audit retention, search, and a UI are control-plane (Cloud) capabilities; the
//! open-source gateway emits the stream and the operator stores it wherever they
//! like (it flows out over OTLP logs alongside the rest of the tracing pipeline).

/// The tracing target every audit event is emitted under. Operators filter on
/// this target (or on the `vaultplane.audit` field) to isolate the audit stream.
pub const AUDIT_TARGET: &str = "vaultplane::audit";

/// The outcome of an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

impl Outcome {
    /// The stable string form recorded on the event.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
        }
    }
}

/// A virtual key was issued. `actor` identifies who requested it (for example
/// `admin-api` or a CLI operator email).
pub fn key_created(actor: &str, key_id: &str, team: &str, app: &str, env: &str) {
    tracing::info!(
        target: AUDIT_TARGET,
        action = "key.create",
        "vaultplane.audit" = true,
        actor = actor,
        subject = key_id,
        outcome = Outcome::Success.as_str(),
        team = team,
        app = app,
        env = env,
        "virtual key issued",
    );
}

/// A virtual key revocation was attempted. `found` is false when no key matched
/// the id, which is recorded as a failed outcome.
pub fn key_revoked(actor: &str, key_id: &str, found: bool) {
    let outcome = if found {
        Outcome::Success
    } else {
        Outcome::Failure
    };
    tracing::info!(
        target: AUDIT_TARGET,
        action = "key.revoke",
        "vaultplane.audit" = true,
        actor = actor,
        subject = key_id,
        outcome = outcome.as_str(),
        "virtual key revocation",
    );
}

/// A configuration reload was attempted. `detail` carries the config source (for
/// example the file path) or, on failure, a short error summary.
pub fn config_reloaded(actor: &str, outcome: Outcome, detail: &str) {
    tracing::info!(
        target: AUDIT_TARGET,
        action = "config.reload",
        "vaultplane.audit" = true,
        actor = actor,
        subject = "config",
        outcome = outcome.as_str(),
        detail = detail,
        "configuration reload",
    );
}

/// A plugin was loaded into the request chain. `kind` is the plugin type (for
/// example `wasm` or `pii_redaction`) and `source` its origin (a path, or
/// `built-in` for native plugins).
pub fn plugin_loaded(name: &str, kind: &str, source: &str) {
    tracing::info!(
        target: AUDIT_TARGET,
        action = "plugin.load",
        "vaultplane.audit" = true,
        actor = "gateway",
        subject = name,
        outcome = Outcome::Success.as_str(),
        kind = kind,
        source = source,
        "plugin loaded",
    );
}

/// A plugin rejected a request. `subject` is the virtual key the request was
/// authenticated with.
pub fn plugin_rejected(virtual_key_id: &str, plugin: &str, reason: &str, status: u16) {
    tracing::info!(
        target: AUDIT_TARGET,
        action = "plugin.reject",
        "vaultplane.audit" = true,
        actor = "gateway",
        subject = virtual_key_id,
        outcome = Outcome::Failure.as_str(),
        plugin = plugin,
        reason = reason,
        status = status,
        "request rejected by plugin",
    );
}

/// A request failed over from one provider to the next. `subject` is the virtual
/// model being served.
pub fn failover(virtual_model: &str, from_provider: &str, to_provider: &str, reason: &str) {
    tracing::info!(
        target: AUDIT_TARGET,
        action = "failover",
        "vaultplane.audit" = true,
        actor = "gateway",
        subject = virtual_model,
        outcome = Outcome::Success.as_str(),
        from_provider = from_provider,
        to_provider = to_provider,
        reason = reason,
        "provider failover",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that captures everything written into a shared buffer.
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` with a capturing subscriber installed and return what it logged.
    fn capture(body: impl FnOnce()) -> String {
        let buf = Capture(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn key_created_event_carries_the_canonical_fields() {
        let out = capture(|| key_created("admin-api", "vp_123", "backend", "chatbot", "prod"));
        assert!(out.contains("vaultplane.audit"), "missing audit tag: {out}");
        assert!(
            out.contains("action=\"key.create\""),
            "missing action: {out}"
        );
        assert!(out.contains("actor=\"admin-api\""), "missing actor: {out}");
        assert!(out.contains("subject=\"vp_123\""), "missing subject: {out}");
        assert!(
            out.contains("outcome=\"success\""),
            "missing outcome: {out}"
        );
        assert!(out.contains("team=\"backend\""), "missing metadata: {out}");
    }

    #[test]
    fn revoking_a_missing_key_records_a_failure() {
        let out = capture(|| key_revoked("admin-api", "vp_missing", false));
        assert!(
            out.contains("action=\"key.revoke\""),
            "missing action: {out}"
        );
        assert!(
            out.contains("outcome=\"failure\""),
            "should be a failure: {out}"
        );
    }

    #[test]
    fn failover_event_names_both_providers() {
        let out = capture(|| failover("chat-default", "openai", "anthropic", "status 429"));
        assert!(out.contains("action=\"failover\""), "missing action: {out}");
        assert!(
            out.contains("from_provider=\"openai\""),
            "missing from: {out}"
        );
        assert!(
            out.contains("to_provider=\"anthropic\""),
            "missing to: {out}"
        );
    }
}
