// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Control Node ("api" mode) client: pulls config and keys and swaps them into
//! the running gateway, while preserving the data-plane survival guarantee.
//!
//! The loop is deliberately fail-soft. On every tick it conditionally fetches
//! `/config` and `/keys` (ETag / `If-None-Match`, so an unchanged resource is a
//! cheap `304`). On *any* failure to obtain a fresh resource (network error,
//! `401` from a revoked/expired token, a bundle that fails to build) it logs,
//! emits an audit event, and KEEPS SERVING the last-known-good runtime. An auth
//! failure or an unreachable Control Node never drops traffic, which is the
//! contract's non-negotiable rule.
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

/// Actor recorded on audit events for control-plane-driven changes.
const ACTOR: &str = "control-plane";

/// Per-request timeout for the polling fetches. Bounds how long a slow Control
/// Node can stall a tick; the loop keeps serving the old config meanwhile.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of a conditional fetch.
enum Fetch<T> {
    /// A new resource was returned (status 200); the stored ETag is updated.
    Modified(T),
    /// The resource is unchanged (status 304); keep what we have.
    NotModified,
    /// The token was rejected (status 401). Per the contract, keep serving
    /// last-known-good rather than dropping traffic.
    Unauthorized,
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
}

impl ControlNodeClient {
    /// Build a client for `endpoint` (e.g. `https://control-node.example/v1`)
    /// authenticating with `token`.
    pub fn new(endpoint: &str, token: String) -> anyhow::Result<Self> {
        let base = endpoint.trim_end_matches('/');
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            config_url: format!("{base}/config"),
            keys_url: format!("{base}/keys"),
            identity_url: format!("{base}/identity"),
            token,
            config_etag: None,
            keys_etag: None,
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
            self.poll_config(&runtime, &base).await;
            self.poll_keys(&keys).await;
            tokio::time::sleep(interval).await;
        }
    }

    async fn poll_config(&mut self, runtime: &RuntimeHandle, base: &Config) {
        match self.fetch::<dto::GatewayConfig>(Resource::Config).await {
            Ok(Fetch::Modified(bundle)) => {
                let version = bundle.version.clone();
                let config = map::apply_config(base, bundle);
                match runtime::build_runtime(&config) {
                    Ok(new_runtime) => {
                        runtime.store(Arc::new(new_runtime));
                        tracing::info!(version = %version, "control plane: applied new config bundle");
                        audit::config_reloaded(ACTOR, Outcome::Success, &format!("config {version}"));
                    }
                    Err(err) => {
                        tracing::warn!(
                            version = %version, error = %err,
                            "control plane: fetched config failed to build; keeping last-known-good"
                        );
                        audit::config_reloaded(ACTOR, Outcome::Failure, &format!("{err:#}"));
                    }
                }
            }
            Ok(Fetch::NotModified) => {}
            Ok(Fetch::Unauthorized) => {
                tracing::warn!(
                    "control plane: 401 fetching config; keeping last-known-good \
                     (check control_plane.token_env / token rotation)"
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "control plane: config fetch failed; keeping last-known-good"
                );
            }
        }
    }

    async fn poll_keys(&mut self, keys: &Arc<KeyStore>) {
        match self.fetch::<dto::KeySet>(Resource::Keys).await {
            Ok(Fetch::Modified(set)) => {
                let version = set.version.clone();
                let mapped = map::to_virtual_keys(set.keys);
                let count = mapped.len();
                keys.replace_all(mapped);
                tracing::info!(version = %version, count, "control plane: applied new key set");
            }
            Ok(Fetch::NotModified) => {}
            Ok(Fetch::Unauthorized) => {
                tracing::warn!("control plane: 401 fetching keys; keeping last-known-good key set");
            }
            Err(err) => {
                tracing::warn!(error = %err, "control plane: key fetch failed; keeping last-known-good");
            }
        }
    }

    /// Best-effort one-shot enrollment check: fetch `/identity` and log the
    /// assignment the Control Node resolved for this token, so an operator can
    /// confirm the gateway landed in the intended fleet slot. Never fatal.
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
                    Ok(id) => tracing::info!(
                        gateway_id = %id.gateway_id,
                        org_id = %id.org_id,
                        group = id.group.as_deref().unwrap_or("(none)"),
                        environment = %id.environment,
                        config_version = id.config_version.as_deref().unwrap_or("(none)"),
                        keys_version = id.keys_version.as_deref().unwrap_or("(none)"),
                        "control plane: enrolled identity confirmed"
                    ),
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

    /// Conditionally GET one resource, updating its stored ETag on a 200.
    async fn fetch<T: serde::de::DeserializeOwned>(
        &mut self,
        resource: Resource,
    ) -> anyhow::Result<Fetch<T>> {
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

        let resp = req.send().await?;
        match resp.status() {
            StatusCode::OK => {
                let new_etag = resp
                    .headers()
                    .get(ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = resp.json::<T>().await?;
                self.store_etag(resource, new_etag);
                Ok(Fetch::Modified(body))
            }
            StatusCode::NOT_MODIFIED => Ok(Fetch::NotModified),
            StatusCode::UNAUTHORIZED => Ok(Fetch::Unauthorized),
            status => {
                anyhow::bail!("control node returned unexpected status {status} for {url}")
            }
        }
    }

    fn store_etag(&mut self, resource: Resource, etag: Option<String>) {
        match resource {
            Resource::Config => self.config_etag = etag,
            Resource::Keys => self.keys_etag = etag,
        }
    }
}

#[derive(Clone, Copy)]
enum Resource {
    Config,
    Keys,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CONFIG_BODY: &str = r#"{ "version": "v1", "models": [], "providers": {} }"#;

    #[tokio::test]
    async fn fetch_sets_then_sends_etag_and_handles_304() {
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

        let mut client = ControlNodeClient::new(&server.uri(), "t0ken".to_string()).unwrap();

        let first = client.fetch::<dto::GatewayConfig>(Resource::Config).await.unwrap();
        assert!(matches!(first, Fetch::Modified(_)), "first fetch returns the body");
        assert_eq!(client.config_etag.as_deref(), Some("\"v1\""), "etag is stored");

        let second = client.fetch::<dto::GatewayConfig>(Resource::Config).await.unwrap();
        assert!(matches!(second, Fetch::NotModified), "second fetch is conditional -> 304");
    }

    #[tokio::test]
    async fn unauthorized_is_soft_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let mut client = ControlNodeClient::new(&server.uri(), "stale".to_string()).unwrap();
        let outcome = client.fetch::<dto::KeySet>(Resource::Keys).await.unwrap();
        assert!(
            matches!(outcome, Fetch::Unauthorized),
            "401 maps to Unauthorized (keep last-known-good), not Err"
        );
    }
}
