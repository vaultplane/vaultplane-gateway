// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Observable state of the Control Node link, surfaced on `GET /admin/status`.
//!
//! The api-mode client keeps serving last-known-good config on every failure,
//! which is the right behavior for traffic but leaves an operator with only a
//! `warn!` line to notice that the gateway has silently stopped following the
//! Control Node. This module gives that condition a place to live: the client
//! records the outcome of every poll tick here, and the admin API renders a
//! snapshot so `vaultplane-ctl status` (or a dashboard polling
//! `/admin/status`) shows whether the link is `synced` or `degraded`, which
//! versions are applied, and what the last failure was.
//!
//! Readiness (`/admin/readyz`) deliberately does NOT reflect this state. A
//! Control Node outage must not pull gateways out of rotation; that is the
//! data-plane survival rule. The link state is informational and alerting is
//! the operator's call.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

/// Where the gateway's configuration comes from and, in api mode, whether the
/// Control Node link is currently healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// `control_plane.mode: file`; there is no Control Node link.
    File,
    /// Api mode, and no poll has completed yet.
    Pending,
    /// The last poll of both `/config` and `/keys` succeeded (`200` or `304`).
    Synced,
    /// The last poll failed on at least one resource; the gateway is serving
    /// its last-known-good config and keys.
    Degraded,
}

/// Which Control Node resource a failure refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Config,
    Keys,
}

impl Resource {
    /// The stable string form used on audit events.
    pub const fn as_str(self) -> &'static str {
        match self {
            Resource::Config => "config",
            Resource::Keys => "keys",
        }
    }
}

/// Why a poll failed. A closed set so dashboards and alerts can match on it;
/// the free-form detail rides alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The client was not started (a prerequisite such as `endpoint` or the
    /// token env var is missing); the gateway is serving local config only.
    NotStarted,
    /// The Control Node returned `401`: the token is revoked, expired, or wrong.
    Unauthorized,
    /// The request never completed: DNS, connect, TLS, or timeout.
    Transport,
    /// The Control Node answered with a status the contract does not define.
    UnexpectedStatus,
    /// A `200` body did not parse as the contract's shape.
    InvalidBody,
    /// The bundle parsed but the runtime could not be built from it (for
    /// example an unknown provider or a plugin that failed to load).
    BuildFailed,
}

impl Reason {
    /// The stable string form used on audit events.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::NotStarted => "not_started",
            Reason::Unauthorized => "unauthorized",
            Reason::Transport => "transport",
            Reason::UnexpectedStatus => "unexpected_status",
            Reason::InvalidBody => "invalid_body",
            Reason::BuildFailed => "build_failed",
        }
    }
}

/// One failed poll of one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFailure {
    pub resource: Resource,
    pub reason: Reason,
    pub detail: String,
}

impl SyncFailure {
    pub fn new(resource: Resource, reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            resource,
            reason,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.resource.as_str(),
            self.reason.as_str(),
            self.detail
        )
    }
}

/// The fleet assignment the Control Node resolved for this gateway's token
/// (from `GET /identity`), kept so an operator can confirm enrollment without
/// digging through startup logs.
#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub gateway_id: String,
    pub org_id: String,
    pub group: Option<String>,
    pub environment: String,
}

/// What a tick changed about the link state, so the caller can audit the
/// transition (and only the transition, never every failing tick).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing worth auditing: still healthy, or still failing the same way.
    None,
    /// The link went from healthy (or pending) to degraded, or the failure
    /// changed shape (for example `transport` became `unauthorized`).
    Degraded(SyncFailure),
    /// The link is healthy again after being degraded.
    Recovered,
}

#[derive(Debug)]
struct Inner {
    state: LinkState,
    endpoint: Option<String>,
    config_version: Option<String>,
    keys_version: Option<String>,
    identity: Option<Identity>,
    last_success: Option<Instant>,
    last_failure: Option<(Instant, SyncFailure)>,
    consecutive_failures: u32,
}

/// Shared handle to the link state. Cloned into the client and the admin
/// state; cheap to clone, mutations take a short lock.
#[derive(Debug, Clone)]
pub struct StatusHandle(Arc<Mutex<Inner>>);

impl StatusHandle {
    /// Handle for `control_plane.mode: file`. Never changes.
    pub fn file() -> Self {
        Self::new(LinkState::File, None)
    }

    /// Handle for api mode against `endpoint`, starting in `pending`.
    pub fn api(endpoint: impl Into<String>) -> Self {
        Self::new(LinkState::Pending, Some(endpoint.into()))
    }

    fn new(state: LinkState, endpoint: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            state,
            endpoint,
            config_version: None,
            keys_version: None,
            identity: None,
            last_success: None,
            last_failure: None,
            consecutive_failures: 0,
        })))
    }

    /// Record that api mode was requested but the client could not start.
    /// The link shows as `degraded` with reason `not_started` so a
    /// misconfigured deployment is visible on `/admin/status` instead of only
    /// in the startup log.
    pub fn not_started(&self, detail: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = LinkState::Degraded;
        inner.last_failure = Some((
            Instant::now(),
            SyncFailure::new(Resource::Config, Reason::NotStarted, detail),
        ));
    }

    /// Record the identity the Control Node resolved for this gateway.
    pub fn set_identity(&self, identity: Identity) {
        self.lock().identity = Some(identity);
    }

    /// Record the outcome of one poll tick. `config` and `keys` carry the
    /// version that was newly applied (`Some`) or `None` when the resource was
    /// unchanged (`304`); an `Err` is the failure for that resource. Returns
    /// the transition, if any, for the caller to audit.
    pub fn record_tick(
        &self,
        config: Result<Option<String>, SyncFailure>,
        keys: Result<Option<String>, SyncFailure>,
    ) -> Transition {
        let mut inner = self.lock();
        let was = inner.state;
        let now = Instant::now();

        // Versions update independently: a keys failure must not discard the
        // config version that did apply this tick.
        if let Ok(Some(version)) = &config {
            inner.config_version = Some(version.clone());
        }
        if let Ok(Some(version)) = &keys {
            inner.keys_version = Some(version.clone());
        }

        // Config is polled first, so its failure is the more useful one to
        // surface when both fail (a keys failure is usually the same cause).
        let failure = match (config, keys) {
            (Err(f), _) | (Ok(_), Err(f)) => Some(f),
            (Ok(_), Ok(_)) => None,
        };

        match failure {
            Some(failure) => {
                inner.state = LinkState::Degraded;
                inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
                let changed_shape = match &inner.last_failure {
                    Some((_, previous)) => {
                        previous.resource != failure.resource || previous.reason != failure.reason
                    }
                    None => true,
                };
                inner.last_failure = Some((now, failure.clone()));
                if was != LinkState::Degraded || changed_shape {
                    Transition::Degraded(failure)
                } else {
                    Transition::None
                }
            }
            None => {
                inner.state = LinkState::Synced;
                inner.consecutive_failures = 0;
                inner.last_success = Some(now);
                if was == LinkState::Degraded {
                    Transition::Recovered
                } else {
                    Transition::None
                }
            }
        }
    }

    /// A point-in-time snapshot for the admin API.
    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        let now = Instant::now();
        Snapshot {
            state: inner.state,
            endpoint: inner.endpoint.clone(),
            config_version: inner.config_version.clone(),
            keys_version: inner.keys_version.clone(),
            identity: inner.identity.clone(),
            last_success_seconds_ago: inner
                .last_success
                .map(|t| now.saturating_duration_since(t).as_secs()),
            consecutive_failures: inner.consecutive_failures,
            last_error: inner.last_failure.as_ref().map(|(at, failure)| LastError {
                resource: failure.resource,
                reason: failure.reason,
                detail: failure.detail.clone(),
                seconds_ago: now.saturating_duration_since(*at).as_secs(),
            }),
        }
    }

    /// The current link state alone.
    #[cfg(test)]
    pub fn state(&self) -> LinkState {
        self.lock().state
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // The guarded data is plain state with no invariants a panic could
        // break mid-update, so a poisoned lock is still safe to read.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The `control_plane` block of `GET /admin/status`.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub state: LinkState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Version of the config bundle currently applied from the Control Node.
    /// Absent until the first bundle applies (local config is serving).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    /// Version of the key set currently applied from the Control Node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_seconds_ago: Option<u64>,
    pub consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,
}

/// The most recent failure, kept after recovery so an operator can see what
/// went wrong even once the link is `synced` again.
#[derive(Debug, Clone, Serialize)]
pub struct LastError {
    pub resource: Resource,
    pub reason: Reason,
    pub detail: String,
    pub seconds_ago: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(resource: Resource) -> SyncFailure {
        SyncFailure::new(resource, Reason::Transport, "connection refused")
    }

    #[test]
    fn file_mode_is_static() {
        let status = StatusHandle::file();
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::File);
        assert!(snap.endpoint.is_none());
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[test]
    fn first_failure_degrades_and_audits_once() {
        let status = StatusHandle::api("http://cn/v1");
        assert_eq!(status.state(), LinkState::Pending);

        let t = status.record_tick(Err(transport(Resource::Config)), Ok(None));
        assert!(
            matches!(t, Transition::Degraded(_)),
            "pending -> degraded is a transition"
        );
        assert_eq!(status.state(), LinkState::Degraded);

        // The same failure again is not a new transition (no audit spam).
        let t = status.record_tick(Err(transport(Resource::Config)), Ok(None));
        assert_eq!(t, Transition::None);
        assert_eq!(status.snapshot().consecutive_failures, 2);
    }

    #[test]
    fn a_changed_failure_shape_is_a_transition() {
        let status = StatusHandle::api("http://cn/v1");
        status.record_tick(Err(transport(Resource::Config)), Ok(None));

        let unauthorized = SyncFailure::new(Resource::Config, Reason::Unauthorized, "401");
        let t = status.record_tick(Err(unauthorized.clone()), Ok(None));
        assert_eq!(t, Transition::Degraded(unauthorized));
    }

    #[test]
    fn recovery_is_a_transition_and_keeps_the_last_error() {
        let status = StatusHandle::api("http://cn/v1");
        status.record_tick(Err(transport(Resource::Keys)), Ok(None));

        let t = status.record_tick(Ok(Some("c2".into())), Ok(Some("k2".into())));
        assert_eq!(t, Transition::Recovered);

        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Synced);
        assert_eq!(snap.consecutive_failures, 0);
        assert_eq!(snap.config_version.as_deref(), Some("c2"));
        assert_eq!(snap.keys_version.as_deref(), Some("k2"));
        assert_eq!(snap.last_success_seconds_ago, Some(0));
        let last = snap.last_error.expect("last error retained after recovery");
        assert_eq!(last.resource, Resource::Keys);
        assert_eq!(last.reason, Reason::Transport);
    }

    #[test]
    fn a_healthy_tick_is_not_a_transition_and_304_keeps_versions() {
        let status = StatusHandle::api("http://cn/v1");
        let t = status.record_tick(Ok(Some("c1".into())), Ok(Some("k1".into())));
        assert_eq!(t, Transition::None, "pending -> synced needs no audit");

        // A 304 on both leaves the applied versions in place.
        let t = status.record_tick(Ok(None), Ok(None));
        assert_eq!(t, Transition::None);
        let snap = status.snapshot();
        assert_eq!(snap.config_version.as_deref(), Some("c1"));
        assert_eq!(snap.keys_version.as_deref(), Some("k1"));
    }

    #[test]
    fn a_keys_failure_does_not_discard_an_applied_config_version() {
        let status = StatusHandle::api("http://cn/v1");
        status.record_tick(Ok(Some("c1".into())), Err(transport(Resource::Keys)));
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Degraded);
        assert_eq!(snap.config_version.as_deref(), Some("c1"));
        assert!(snap.keys_version.is_none());
    }

    #[test]
    fn not_started_shows_as_degraded_with_a_reason() {
        let status = StatusHandle::api("http://cn/v1");
        status.not_started("control_plane.token_env is unset");
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Degraded);
        let last = snap.last_error.unwrap();
        assert_eq!(last.reason, Reason::NotStarted);
        assert!(last.detail.contains("token_env"));
    }

    #[test]
    fn snapshot_serializes_snake_case_and_omits_absent_fields() {
        let status = StatusHandle::api("http://cn/v1");
        let json = serde_json::to_value(status.snapshot()).unwrap();
        assert_eq!(json["state"], "pending");
        assert_eq!(json["endpoint"], "http://cn/v1");
        assert_eq!(json["consecutive_failures"], 0);
        assert!(json.get("config_version").is_none());
        assert!(json.get("last_error").is_none());

        status.record_tick(
            Err(SyncFailure::new(
                Resource::Config,
                Reason::Unauthorized,
                "401",
            )),
            Ok(None),
        );
        let json = serde_json::to_value(status.snapshot()).unwrap();
        assert_eq!(json["state"], "degraded");
        assert_eq!(json["last_error"]["reason"], "unauthorized");
        assert_eq!(json["last_error"]["resource"], "config");
    }
}
