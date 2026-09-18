// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Control Node ("api" mode) client: pulls config and keys and swaps them into
//! the running gateway, while preserving the data-plane survival guarantee.
//!
//! The loop is deliberately fail-soft. On every tick it conditionally fetches
//! `/config` and `/keys` (ETag / `If-None-Match`, so an unchanged resource is a
//! cheap `304`). On *any* failure to obtain a fresh resource (network error,
//! `401` from a revoked/expired token, a bundle that fails to build) it logs,
//! records the failure on the shared [`StatusHandle`] so `/admin/status` shows
//! the link as `degraded`, and KEEPS SERVING the last-known-good runtime. An
//! auth failure or an unreachable Control Node never drops traffic, which is
//! the contract's non-negotiable rule.
//!
//! Audit events for the link (`control_plane.sync`) fire on state transitions
//! only: once when the link degrades or the failure changes shape, once when
//! it recovers. Every failing tick still logs a `warn!`.
//!
//! An ETag is committed only after its resource has been applied. A bundle
//! that parses but fails to build leaves the previous ETag in place, so the
//! next tick fetches the same version again and applies it as soon as the
//! Control Node ships a fixed one, rather than treating the broken version as
//! current and answering `304` forever.
//!
//! `GET /watch` (SSE push for fast propagation) is a planned follow-up; the
//! contract specifies clients fall back to polling when the stream is
//! unavailable, so this poll loop is both the fallback and the MVP.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use vaultplane_core::audit::{self, Outcome};
use vaultplane_core::auth::KeyStore;
use vaultplane_core::config::Config;

use crate::runtime::{self, RuntimeHandle};

use super::dto;
use super::map;
use super::status::{Identity, Reason, Resource, StatusHandle, SyncFailure, Transition};

/// Actor recorded on audit events for control-plane-driven changes.
const ACTOR: &str = "control-plane";

/// Per-request timeout for the polling fetches. Bounds how long a slow Control
/// Node can stall a tick; the loop keeps serving the old config meanwhile.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of a conditional fetch that reached the Control Node and was
/// answered within the contract.
#[derive(Debug)]
enum Fetch<T> {
    /// A new resource was returned (status 200) along with the ETag to commit
    /// once it has been applied.
    Modified { body: T, etag: Option<String> },
    /// The resource is unchanged (status 304); keep what we have.
    NotModified,
}

/// A client bound to one Control Node, holding the per-resource ETags so each
/// poll is conditional.
pub struct ControlNodeClient {
    http: reqwest::Client,
    config_url: String,
    keys_url: String,
    identity_url: String,
    token: String,
    config_etag: Option<String>,
    keys_etag: Option<String>,
    status: StatusHandle,
}

impl ControlNodeClient {
    /// Build a client for `endpoint` (e.g. `https://control-node.example/v1`)
    /// authenticating with `token`, reporting link state through `status`.
    pub fn new(endpoint: &str, token: String, status: StatusHandle) -> anyhow::Result<Self> {
        let base = endpoint.trim_end_matches('/');
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            config_url: format!("{base}/config"),
            keys_url: format!("{base}/keys"),
            identity_url: format!("{base}/identity"),
            token,
            config_etag: None,
            keys_etag: None,
            status,
        })
    }

    /// Run the poll loop forever, applying changes into `runtime` and `keys`.
    /// `base` is the gateway's local config, onto which each fetched bundle is
    /// overlaid (the bundle does not carry node-local settings).
    pub async fn run(
        mut self,
        runtime: RuntimeHandle,
        keys: Arc<KeyStore>,
        base: Config,
        interval: Duration,
    ) {
        tracing::info!(
            interval_seconds = interval.as_secs(),
            "control plane: api-mode client started; polling the Control Node"
        );
        self.log_identity().await;
        loop {
            self.tick(&runtime, &keys, &base).await;
            tokio::time::sleep(interval).await;
        }
    }

    /// One poll pass: refresh config then keys, then record the tick on the
    /// status handle and audit any transition. Factored out of [`run`] so it
    /// can be driven once, deterministically, in tests.
    async fn tick(&mut self, runtime: &RuntimeHandle, keys: &Arc<KeyStore>, base: &Config) {
        let config = self.poll_config(runtime, base).await;
        let keys = self.poll_keys(keys).await;
        match self.status.record_tick(config, keys) {
            Transition::None => {}
            Transition::Degraded(failure) => {
                audit::control_plane_sync(
                    Outcome::Failure,
                    failure.resource.as_str(),
                    failure.reason.as_str(),
                    &failure.detail,
                );
            }
            Transition::Recovered => {
                tracing::info!("control plane: link recovered; following the Control Node again");
                audit::control_plane_sync(Outcome::Success, "", "", "");
            }
        }
    }

    /// Fetch and apply `/config`. `Ok(Some(version))` when a new bundle was
    /// applied, `Ok(None)` on `304`. Any failure keeps the previous runtime and
    /// the previous ETag.
    async fn poll_config(
        &mut self,
        runtime: &RuntimeHandle,
        base: &Config,
    ) -> Result<Option<String>, SyncFailure> {
        let fetched = self
            .fetch::<dto::GatewayConfig>(Resource::Config)
            .await
            .inspect_err(|failure| {
                tracing::warn!(
                    reason = failure.reason.as_str(), error = %failure.detail,
                    "control plane: config fetch failed; keeping last-known-good"
                );
            })?;
        let Fetch::Modified { body: bundle, etag } = fetched else {
            return Ok(None);
        };

        let version = bundle.version.clone();
        let config = map::apply_config(base, bundle);
        match runtime::build_runtime(&config) {
            Ok(new_runtime) => {
                runtime.store(Arc::new(new_runtime));
                // Only now is this version "current"; commit its ETag.
                self.config_etag = etag;
                tracing::info!(version = %version, "control plane: applied new config bundle");
                audit::config_reloaded(ACTOR, Outcome::Success, &format!("config {version}"));
                Ok(Some(version))
            }
            Err(err) => {
                // The ETag is deliberately left as it was: the next tick will
                // fetch this version again instead of getting a 304 for a
                // bundle that never applied.
                tracing::warn!(
                    version = %version, error = %err,
                    "control plane: fetched config failed to build; keeping last-known-good \
                     and retrying this version next tick"
                );
                audit::config_reloaded(ACTOR, Outcome::Failure, &format!("{err:#}"));
                Err(SyncFailure::new(
                    Resource::Config,
                    Reason::BuildFailed,
                    format!("config {version}: {err:#}"),
                ))
            }
        }
    }

    /// Fetch and apply `/keys`. `Ok(Some(version))` when a new key set was
    /// applied, `Ok(None)` on `304`. Any failure keeps the previous key set.
    async fn poll_keys(&mut self, keys: &Arc<KeyStore>) -> Result<Option<String>, SyncFailure> {
        let fetched = self
            .fetch::<dto::KeySet>(Resource::Keys)
            .await
            .inspect_err(|failure| {
                tracing::warn!(
                    reason = failure.reason.as_str(), error = %failure.detail,
                    "control plane: key fetch failed; keeping last-known-good key set"
                );
            })?;
        let Fetch::Modified { body: set, etag } = fetched else {
            return Ok(None);
        };

        let version = set.version.clone();
        let mapped = map::to_virtual_keys(set.keys);
        let count = mapped.len();
        keys.replace_all(mapped);
        self.keys_etag = etag;
        tracing::info!(version = %version, count, "control plane: applied new key set");
        Ok(Some(version))
    }

    /// Best-effort one-shot enrollment check: fetch `/identity`, log the
    /// assignment the Control Node resolved for this token, and record it on
    /// the status handle so an operator can confirm the gateway landed in the
    /// intended fleet slot from `/admin/status`. Never fatal.
    async fn log_identity(&self) {
        let result = self
            .http
            .get(&self.identity_url)
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await;
        match result {
            Ok(resp) if resp.status() == StatusCode::OK => {
                match resp.json::<dto::GatewayIdentity>().await {
                    Ok(id) => {
                        tracing::info!(
                            gateway_id = %id.gateway_id,
                            org_id = %id.org_id,
                            group = id.group.as_deref().unwrap_or("(none)"),
                            environment = %id.environment,
                            config_version = id.config_version.as_deref().unwrap_or("(none)"),
                            keys_version = id.keys_version.as_deref().unwrap_or("(none)"),
                            "control plane: enrolled identity confirmed"
                        );
                        self.status.set_identity(Identity {
                            gateway_id: id.gateway_id,
                            org_id: id.org_id,
                            group: id.group,
                            environment: id.environment,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "control plane: could not parse /identity response")
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "control plane: /identity check failed")
            }
            Err(err) => tracing::warn!(error = %err, "control plane: /identity check unreachable"),
        }
    }

    /// Conditionally GET one resource. On a `200` the response's ETag is
    /// returned to the caller, NOT stored: the caller commits it only after
    /// the resource has been applied.
    async fn fetch<T: serde::de::DeserializeOwned>(
        &self,
        resource: Resource,
    ) -> Result<Fetch<T>, SyncFailure> {
        let (url, etag) = match resource {
            Resource::Config => (&self.config_url, &self.config_etag),
            Resource::Keys => (&self.keys_url, &self.keys_etag),
        };

        let mut req = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT);
        if let Some(etag) = etag {
            req = req.header(IF_NONE_MATCH, etag);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| SyncFailure::new(resource, Reason::Transport, err.to_string()))?;
        match resp.status() {
            StatusCode::OK => {
                let etag = resp
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = resp.json::<T>().await.map_err(|err| {
                    SyncFailure::new(resource, Reason::InvalidBody, err.to_string())
                })?;
                Ok(Fetch::Modified { body, etag })
            }
            StatusCode::NOT_MODIFIED => Ok(Fetch::NotModified),
            StatusCode::UNAUTHORIZED => Err(SyncFailure::new(
                resource,
                Reason::Unauthorized,
                "401 Unauthorized (check control_plane.token_env / token rotation)",
            )),
            status => Err(SyncFailure::new(
                resource,
                Reason::UnexpectedStatus,
                format!("control node returned unexpected status {status} for {url}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CONFIG_BODY: &str = r#"{ "version": "v1", "models": [], "providers": {} }"#;

    fn client(server: &MockServer, token: &str) -> ControlNodeClient {
        ControlNodeClient::new(
            &server.uri(),
            token.to_string(),
            StatusHandle::api(server.uri()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn fetch_returns_the_etag_for_the_caller_to_commit() {
        let server = MockServer::start().await;

        // First call: no If-None-Match -> 200 with an ETag.
        Mock::given(method("GET"))
            .and(path("/config"))
            .and(header("authorization", "Bearer t0ken"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_string(CONFIG_BODY),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call: must carry If-None-Match -> 304.
        Mock::given(method("GET"))
            .and(path("/config"))
            .and(header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let mut client = client(&server, "t0ken");

        let first = client
            .fetch::<dto::GatewayConfig>(Resource::Config)
            .await
            .unwrap();
        let Fetch::Modified { etag, .. } = first else {
            panic!("first fetch returns the body");
        };
        assert_eq!(etag.as_deref(), Some("\"v1\""), "etag is returned");
        assert_eq!(client.config_etag, None, "fetch alone does not commit it");

        // Once committed, the next fetch is conditional -> 304.
        client.config_etag = etag;
        let second = client
            .fetch::<dto::GatewayConfig>(Resource::Config)
            .await
            .unwrap();
        assert!(matches!(second, Fetch::NotModified));
    }

    #[tokio::test]
    async fn unauthorized_is_a_typed_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = client(&server, "stale");
        let failure = client
            .fetch::<dto::KeySet>(Resource::Keys)
            .await
            .expect_err("401 is a failure");
        assert_eq!(failure.resource, Resource::Keys);
        assert_eq!(failure.reason, Reason::Unauthorized);
    }

    #[tokio::test]
    async fn an_unparseable_body_is_invalid_body_not_transport() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = client(&server, "tok");
        let failure = client
            .fetch::<dto::GatewayConfig>(Resource::Config)
            .await
            .expect_err("garbage body is a failure");
        assert_eq!(failure.reason, Reason::InvalidBody);
    }

    #[tokio::test]
    async fn an_unexpected_status_is_reported_as_such() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = client(&server, "tok");
        let failure = client
            .fetch::<dto::GatewayConfig>(Resource::Config)
            .await
            .expect_err("503 is a failure");
        assert_eq!(failure.reason, Reason::UnexpectedStatus);
        assert!(failure.detail.contains("503"), "{}", failure.detail);
    }
}

/// End-to-end tests: drive a full poll tick against a mock Control Node serving
/// the Gateway Control API, and assert the live runtime and key store actually
/// swap (and that failures preserve last-known-good). This also serves as a
/// conformance reference for the Control Node implementation: a real service
/// matching these request/response shapes will drive the client correctly.
#[cfg(test)]
mod e2e {
    use super::*;
    use std::sync::Arc;
    use vaultplane_core::auth::{KeyStore, VirtualKey};
    use vaultplane_core::config::{Config, ModelConfig, Route};
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::status::LinkState;
    use crate::runtime::{self, RuntimeHandle};

    const CONFIG_BUNDLE: &str = r#"{
        "version": "c1",
        "models": [
            { "name": "smart", "primary": { "provider": "openai", "model": "gpt-4o" } }
        ],
        "providers": { "openai": { "baseUrl": "https://api.openai.com", "apiKeyEnv": "OPENAI_API_KEY" } }
    }"#;

    /// Parses, but cannot build: the model routes to a provider that does not
    /// exist, which the registry rejects.
    const BROKEN_BUNDLE: &str = r#"{
        "version": "c-broken",
        "models": [
            { "name": "smart", "primary": { "provider": "nope", "model": "gpt-4o" } }
        ]
    }"#;

    const KEY_SET: &str = r#"{
        "version": "k1",
        "keys": [
            { "id": "vp_fromcontrolnode", "hashedKey": "abc123",
              "scope": { "team": "core", "env": "prod", "allowedModels": ["smart"] },
              "limits": { "requestsPerSecond": 10 } }
        ]
    }"#;

    fn empty_runtime() -> RuntimeHandle {
        runtime::handle(runtime::build_runtime(&Config::default()).unwrap())
    }

    fn runtime_with_one_model() -> RuntimeHandle {
        let config = Config {
            models: vec![ModelConfig {
                name: "local-only".to_string(),
                primary: Route {
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                },
                fallbacks: Vec::new(),
                retry_on: vec![429],
                timeout_ms: 30_000,
            }],
            ..Default::default()
        };
        runtime::handle(runtime::build_runtime(&config).unwrap())
    }

    fn client(endpoint: &str, token: &str) -> (ControlNodeClient, StatusHandle) {
        let status = StatusHandle::api(endpoint);
        let client = ControlNodeClient::new(endpoint, token.to_string(), status.clone()).unwrap();
        (client, status)
    }

    async fn mount_ok(server: &MockServer, route: &str, etag: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", etag)
                    .set_body_string(body),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn applies_config_and_keys_from_the_control_node() {
        let server = MockServer::start().await;
        mount_ok(&server, "/config", "\"c1\"", CONFIG_BUNDLE).await;
        mount_ok(&server, "/keys", "\"k1\"", KEY_SET).await;

        let runtime = empty_runtime();
        let keys = Arc::new(KeyStore::default());
        assert!(runtime.load().models.is_empty(), "starts with no models");
        assert!(keys.is_empty(), "starts with no keys");

        let (mut client, status) = client(&server.uri(), "tok");
        client.tick(&runtime, &keys, &Config::default()).await;

        // The runtime now reflects the fetched config bundle.
        let models = &runtime.load().models;
        assert_eq!(models.len(), 1, "config bundle was applied");
        assert_eq!(models[0].id, "smart");
        assert_eq!(models[0].provider, "openai");

        // The key store now holds the control-node-issued key (by its hash).
        assert_eq!(keys.len(), 1, "key set was applied");
        let key = keys.find_by_id("vp_fromcontrolnode").expect("key present");
        assert_eq!(key.hash, "abc123");
        assert_eq!(key.rate_limit_rps, Some(10));

        // Both ETags are committed, so the next tick is conditional.
        assert_eq!(client.config_etag.as_deref(), Some("\"c1\""));
        assert_eq!(client.keys_etag.as_deref(), Some("\"k1\""));

        // And the link reports as synced with the applied versions.
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Synced);
        assert_eq!(snap.config_version.as_deref(), Some("c1"));
        assert_eq!(snap.keys_version.as_deref(), Some("k1"));
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn unauthorized_keeps_last_known_good_and_degrades_the_link() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/config"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        // Seed a live runtime and a key so we can prove they survive a 401.
        let runtime = runtime_with_one_model();
        let keys = Arc::new(KeyStore::default());
        let mut seeded = VirtualKey::anonymous();
        seeded.id = "vp_seeded".to_string();
        seeded.hash = "seedhash".to_string();
        keys.insert(seeded);

        let (mut client, status) = client(&server.uri(), "revoked");
        client.tick(&runtime, &keys, &Config::default()).await;

        // Auth failure must not drop the running config or the existing keys.
        let models = &runtime.load().models;
        assert_eq!(models.len(), 1, "runtime preserved on 401");
        assert_eq!(models[0].id, "local-only");
        assert!(
            keys.find_by_id("vp_seeded").is_some(),
            "existing keys preserved on 401"
        );

        // But the condition is visible: the link is degraded with the reason.
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Degraded);
        assert_eq!(snap.consecutive_failures, 1);
        let err = snap.last_error.expect("failure recorded");
        assert_eq!(err.resource, Resource::Config);
        assert_eq!(err.reason, Reason::Unauthorized);
    }

    #[tokio::test]
    async fn unreachable_control_node_keeps_last_known_good_and_degrades_the_link() {
        // No server: point at a closed port so every fetch errors.
        let runtime = runtime_with_one_model();
        let keys = Arc::new(KeyStore::default());

        let (mut client, status) = client("http://127.0.0.1:1", "tok");
        client.tick(&runtime, &keys, &Config::default()).await;

        assert_eq!(
            runtime.load().models.len(),
            1,
            "runtime preserved when the control node is unreachable"
        );
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Degraded);
        assert_eq!(snap.last_error.unwrap().reason, Reason::Transport);
    }

    #[tokio::test]
    async fn a_bundle_that_fails_to_build_is_retried_not_cached() {
        let server = MockServer::start().await;
        // The Control Node serves a bundle that parses but cannot build. It
        // would answer 304 to a matching If-None-Match, which must never
        // happen: the client must not commit the ETag of a bundle it did not
        // apply.
        Mock::given(method("GET"))
            .and(path("/config"))
            .and(header("if-none-match", "\"c-broken\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"c-broken\"")
                    .set_body_string(BROKEN_BUNDLE),
            )
            .mount(&server)
            .await;
        mount_ok(&server, "/keys", "\"k1\"", KEY_SET).await;

        let runtime = runtime_with_one_model();
        let keys = Arc::new(KeyStore::default());
        let (mut client, status) = client(&server.uri(), "tok");

        // Tick 1: the broken bundle is fetched, fails to build, and is not
        // applied; the ETag stays uncommitted.
        client.tick(&runtime, &keys, &Config::default()).await;
        assert_eq!(
            runtime.load().models[0].id,
            "local-only",
            "runtime preserved"
        );
        assert_eq!(
            client.config_etag, None,
            "ETag of an unapplied bundle is not kept"
        );
        assert_eq!(
            client.keys_etag.as_deref(),
            Some("\"k1\""),
            "keys still applied"
        );
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Degraded);
        assert_eq!(snap.last_error.unwrap().reason, Reason::BuildFailed);
        assert_eq!(snap.keys_version.as_deref(), Some("k1"));
        assert!(snap.config_version.is_none());

        // Tick 2: the same version is fetched again (no If-None-Match sent, so
        // the 304 mock is not hit) and fails again, still last-known-good.
        client.tick(&runtime, &keys, &Config::default()).await;
        assert_eq!(runtime.load().models[0].id, "local-only");
        assert_eq!(client.config_etag, None);
        assert_eq!(status.snapshot().consecutive_failures, 2);

        // The Control Node ships a fixed version: it applies on the next tick.
        server.reset().await;
        mount_ok(&server, "/config", "\"c1\"", CONFIG_BUNDLE).await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .and(header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        client.tick(&runtime, &keys, &Config::default()).await;
        assert_eq!(runtime.load().models[0].id, "smart", "fixed bundle applied");
        assert_eq!(client.config_etag.as_deref(), Some("\"c1\""));
        let snap = status.snapshot();
        assert_eq!(snap.state, LinkState::Synced);
        assert_eq!(snap.config_version.as_deref(), Some("c1"));
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn identity_is_recorded_on_the_status_handle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/identity"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{ "gatewayId": "gw_demo", "orgId": "org_1", "group": "edge",
                     "environment": "prod", "configVersion": "c1", "keysVersion": "k1" }"#,
            ))
            .mount(&server)
            .await;

        let (client, status) = client(&server.uri(), "tok");
        client.log_identity().await;

        let identity = status.snapshot().identity.expect("identity recorded");
        assert_eq!(identity.gateway_id, "gw_demo");
        assert_eq!(identity.group.as_deref(), Some("edge"));
        assert_eq!(identity.environment, "prod");
    }
}
