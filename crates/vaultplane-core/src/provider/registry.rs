// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Configuration-driven model registry with provider failover.
//!
//! A virtual model name resolves to a primary provider route plus ordered fallbacks.
//! On a retryable status code, a connector error, or a timeout, the registry fails
//! over to the next route, rewriting the request to that route's upstream model and
//! tracking the attempt count on the returned response. Models that are not in the
//! registry fall back to a provider chosen by name prefix.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::audit;
use crate::config::ModelConfig;
use crate::error::{Error, Result};
use crate::provider::{ChatRequest, ChatResponse, Connector, EmbeddingsRequest};

/// A single resolved attempt: a connector and the upstream model name to send it.
struct ResolvedRoute {
    connector: Arc<dyn Connector>,
    model: String,
}

/// A virtual model resolved to ordered routes plus its failover policy.
struct ResolvedModel {
    routes: Vec<ResolvedRoute>,
    retry_on: HashSet<u16>,
    timeout: Duration,
}

/// Routes chat requests to provider connectors via the configured model registry,
/// failing over across a model's primary and fallback providers.
pub struct Registry {
    connectors: HashMap<String, Arc<dyn Connector>>,
    models: HashMap<String, ResolvedModel>,
}

impl Registry {
    /// Build a registry from named connectors and the model configuration.
    ///
    /// Returns a configuration error if a model references an unknown provider.
    pub fn new(
        connectors: HashMap<String, Arc<dyn Connector>>,
        models: &[ModelConfig],
    ) -> Result<Self> {
        let mut resolved = HashMap::new();
        for model in models {
            let mut routes = Vec::new();
            for route in std::iter::once(&model.primary).chain(&model.fallbacks) {
                let connector = connectors.get(&route.provider).cloned().ok_or_else(|| {
                    Error::Config(format!(
                        "model '{}' references unknown provider '{}'",
                        model.name, route.provider
                    ))
                })?;
                routes.push(ResolvedRoute {
                    connector,
                    model: route.model.clone(),
                });
            }
            resolved.insert(
                model.name.clone(),
                ResolvedModel {
                    routes,
                    retry_on: model.retry_on.iter().copied().collect(),
                    timeout: Duration::from_millis(model.timeout_ms),
                },
            );
        }
        Ok(Self {
            connectors,
            models: resolved,
        })
    }

    /// Pick a connector for a model that is not in the registry, by name prefix.
    fn fallback_connector(&self, model: &str) -> Option<&Arc<dyn Connector>> {
        let provider = if model.starts_with("claude") {
            "anthropic"
        } else {
            "openai"
        };
        self.connectors.get(provider)
    }
}

/// Try a model's routes in order, failing over on retryable outcomes. The returned
/// response's `attempts` field reflects how many providers were tried.
async fn dispatch(
    resolved: &ResolvedModel,
    virtual_model: &str,
    request: ChatRequest,
) -> Result<ChatResponse> {
    let last = resolved.routes.len().saturating_sub(1);
    for (index, route) in resolved.routes.iter().enumerate() {
        let is_last = index == last;
        let attempt = ChatRequest {
            model: route.model.clone(),
            stream: request.stream,
            body: request.body.clone(),
        };

        match tokio::time::timeout(resolved.timeout, route.connector.chat(attempt)).await {
            Ok(Ok(mut response)) => {
                if !is_last && resolved.retry_on.contains(&response.status) {
                    let next = route_name(resolved, index + 1);
                    tracing::warn!(
                        provider = route.connector.name(),
                        status = response.status,
                        "retryable status; failing over to the next provider"
                    );
                    audit::failover(
                        virtual_model,
                        route.connector.name(),
                        next,
                        &format!("status {}", response.status),
                    );
                    continue;
                }
                response.attempts = (index + 1) as u32;
                return Ok(response);
            }
            Ok(Err(err)) => {
                if is_last {
                    return Err(err);
                }
                let next = route_name(resolved, index + 1);
                tracing::warn!(
                    provider = route.connector.name(),
                    error = %err,
                    "provider error; failing over to the next provider"
                );
                audit::failover(
                    virtual_model,
                    route.connector.name(),
                    next,
                    &err.to_string(),
                );
            }
            Err(_) => {
                if is_last {
                    return Err(Error::UpstreamTimeout(format!(
                        "request timed out on all configured providers (last: {})",
                        route.connector.name()
                    )));
                }
                let next = route_name(resolved, index + 1);
                tracing::warn!(
                    provider = route.connector.name(),
                    timeout_ms = resolved.timeout.as_millis(),
                    "provider timed out; failing over to the next provider"
                );
                audit::failover(
                    virtual_model,
                    route.connector.name(),
                    next,
                    &format!("timeout after {}ms", resolved.timeout.as_millis()),
                );
            }
        }
    }

    Err(Error::Provider("no providers were attempted".to_string()))
}

/// The connector name of the route at `index`, or `"none"` if out of range.
fn route_name(resolved: &ResolvedModel, index: usize) -> &str {
    resolved
        .routes
        .get(index)
        .map(|route| route.connector.name())
        .unwrap_or("none")
}

/// Same shape as [`dispatch`] but for embeddings: tries the model's routes in
/// order, retrying on the same configured statuses.
async fn dispatch_embeddings(
    resolved: &ResolvedModel,
    virtual_model: &str,
    request: EmbeddingsRequest,
) -> Result<ChatResponse> {
    let last = resolved.routes.len().saturating_sub(1);
    for (index, route) in resolved.routes.iter().enumerate() {
        let is_last = index == last;
        let attempt = EmbeddingsRequest {
            model: route.model.clone(),
            body: request.body.clone(),
        };

        match tokio::time::timeout(resolved.timeout, route.connector.embeddings(attempt)).await {
            Ok(Ok(mut response)) => {
                if !is_last && resolved.retry_on.contains(&response.status) {
                    let next = route_name(resolved, index + 1);
                    tracing::warn!(
                        provider = route.connector.name(),
                        status = response.status,
                        "retryable status; failing over to the next provider"
                    );
                    audit::failover(
                        virtual_model,
                        route.connector.name(),
                        next,
                        &format!("status {}", response.status),
                    );
                    continue;
                }
                response.attempts = (index + 1) as u32;
                return Ok(response);
            }
            Ok(Err(err)) => {
                if is_last {
                    return Err(err);
                }
                let next = route_name(resolved, index + 1);
                tracing::warn!(
                    provider = route.connector.name(),
                    error = %err,
                    "provider error; failing over to the next provider"
                );
                audit::failover(
                    virtual_model,
                    route.connector.name(),
                    next,
                    &err.to_string(),
                );
            }
            Err(_) => {
                if is_last {
                    return Err(Error::UpstreamTimeout(format!(
                        "request timed out on all configured providers (last: {})",
                        route.connector.name()
                    )));
                }
                let next = route_name(resolved, index + 1);
                tracing::warn!(
                    provider = route.connector.name(),
                    timeout_ms = resolved.timeout.as_millis(),
                    "provider timed out; failing over to the next provider"
                );
                audit::failover(
                    virtual_model,
                    route.connector.name(),
                    next,
                    &format!("timeout after {}ms", resolved.timeout.as_millis()),
                );
            }
        }
    }

    // Unreachable: the final route always returns above.
    Err(Error::Provider("no providers were attempted".to_string()))
}

#[async_trait]
impl Connector for Registry {
    fn name(&self) -> &str {
        "registry"
    }

    async fn reachable(&self) -> bool {
        // Probe the distinct connectors backing configured models; the gateway is
        // ready to route once at least one of them answers. With no models
        // configured there is nothing to route to, so readiness is not blocked.
        let mut seen = HashSet::new();
        let mut connectors: Vec<Arc<dyn Connector>> = Vec::new();
        for model in self.models.values() {
            for route in &model.routes {
                if seen.insert(route.connector.name().to_string()) {
                    connectors.push(route.connector.clone());
                }
            }
        }
        if connectors.is_empty() {
            return true;
        }
        let probes = connectors.iter().map(|connector| connector.reachable());
        futures::future::join_all(probes)
            .await
            .into_iter()
            .any(|reachable| reachable)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if let Some(resolved) = self.models.get(&request.model) {
            let virtual_model = request.model.clone();
            return dispatch(resolved, &virtual_model, request).await;
        }

        match self.fallback_connector(&request.model) {
            Some(connector) => connector.chat(request).await,
            None => Err(Error::Provider(format!(
                "no provider configured for model '{}'",
                request.model
            ))),
        }
    }

    async fn embeddings(&self, request: EmbeddingsRequest) -> Result<ChatResponse> {
        if let Some(resolved) = self.models.get(&request.model) {
            let virtual_model = request.model.clone();
            return dispatch_embeddings(resolved, &virtual_model, request).await;
        }

        match self.fallback_connector(&request.model) {
            Some(connector) => connector.embeddings(request).await,
            None => Err(Error::Provider(format!(
                "no provider configured for model '{}'",
                request.model
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Registry;
    use crate::config::{ModelConfig, Route};
    use crate::error::Result;
    use crate::provider::{ChatRequest, ChatResponse, Connector, single_chunk};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A connector that returns a fixed status/body and counts how often it is called.
    struct Stub {
        name: String,
        status: u16,
        body: String,
        calls: Arc<AtomicUsize>,
        reachable: bool,
    }

    impl Stub {
        fn new(name: &str, status: u16, body: &str) -> (Arc<Stub>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let stub = Arc::new(Stub {
                name: name.to_string(),
                status,
                body: body.to_string(),
                calls: calls.clone(),
                reachable: true,
            });
            (stub, calls)
        }

        /// A stub used only to exercise readiness probing.
        fn with_reachable(name: &str, reachable: bool) -> Arc<Stub> {
            Arc::new(Stub {
                name: name.to_string(),
                status: 200,
                body: String::new(),
                calls: Arc::new(AtomicUsize::new(0)),
                reachable,
            })
        }
    }

    #[async_trait]
    impl Connector for Stub {
        fn name(&self) -> &str {
            &self.name
        }

        async fn reachable(&self) -> bool {
            self.reachable
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                status: self.status,
                content_type: Some("application/json".to_string()),
                body: single_chunk(self.body.clone().into_bytes()),
                provider: self.name.clone(),
                model: request.model,
                usage: None,
                attempts: 1,
            })
        }
    }

    fn connectors(pairs: Vec<(&str, Arc<Stub>)>) -> HashMap<String, Arc<dyn Connector>> {
        pairs
            .into_iter()
            .map(|(name, stub)| (name.to_string(), stub as Arc<dyn Connector>))
            .collect()
    }

    fn smart_model(retry_on: Vec<u16>) -> ModelConfig {
        ModelConfig {
            name: "smart".to_string(),
            primary: Route {
                provider: "p1".to_string(),
                model: "m1".to_string(),
            },
            fallbacks: vec![Route {
                provider: "p2".to_string(),
                model: "m2".to_string(),
            }],
            retry_on,
            timeout_ms: 30_000,
        }
    }

    fn request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            stream: false,
            body: Bytes::from_static(b"{}"),
        }
    }

    #[tokio::test]
    async fn fails_over_on_retryable_status() {
        let (p1, c1) = Stub::new("p1", 503, "down");
        let (p2, c2) = Stub::new("p2", 200, "ok");
        let registry = Registry::new(
            connectors(vec![("p1", p1), ("p2", p2)]),
            &[smart_model(vec![503])],
        )
        .unwrap();

        let response = registry.chat(request("smart")).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.attempts, 2);
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_fail_over_on_success() {
        let (p1, c1) = Stub::new("p1", 200, "ok");
        let (p2, c2) = Stub::new("p2", 200, "ok");
        let registry = Registry::new(
            connectors(vec![("p1", p1), ("p2", p2)]),
            &[smart_model(vec![503])],
        )
        .unwrap();

        let response = registry.chat(request("smart")).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.attempts, 1);
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn does_not_fail_over_on_non_retryable_status() {
        let (p1, c1) = Stub::new("p1", 400, "bad request");
        let (p2, c2) = Stub::new("p2", 200, "ok");
        let registry = Registry::new(
            connectors(vec![("p1", p1), ("p2", p2)]),
            &[smart_model(vec![503])],
        )
        .unwrap();

        let response = registry.chat(request("smart")).await.unwrap();
        assert_eq!(response.status, 400);
        assert_eq!(response.attempts, 1);
        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_model_routes_by_prefix() {
        let (openai, oc) = Stub::new("openai", 200, "ok");
        let (anthropic, ac) = Stub::new("anthropic", 200, "ok");
        let registry = Registry::new(
            connectors(vec![("openai", openai), ("anthropic", anthropic)]),
            &[],
        )
        .unwrap();

        registry.chat(request("gpt-4o")).await.unwrap();
        registry.chat(request("claude-3-7-sonnet")).await.unwrap();
        assert_eq!(oc.load(Ordering::SeqCst), 1);
        assert_eq!(ac.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_provider_is_a_config_error() {
        let result = Registry::new(HashMap::new(), &[smart_model(vec![503])]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reachable_when_any_backing_provider_answers() {
        let registry = Registry::new(
            connectors(vec![
                ("p1", Stub::with_reachable("p1", false)),
                ("p2", Stub::with_reachable("p2", true)),
            ]),
            &[smart_model(vec![503])],
        )
        .unwrap();
        assert!(registry.reachable().await);
    }

    #[tokio::test]
    async fn not_reachable_when_all_backing_providers_are_down() {
        let registry = Registry::new(
            connectors(vec![
                ("p1", Stub::with_reachable("p1", false)),
                ("p2", Stub::with_reachable("p2", false)),
            ]),
            &[smart_model(vec![503])],
        )
        .unwrap();
        assert!(!registry.reachable().await);
    }

    #[tokio::test]
    async fn reachable_with_no_models_configured() {
        // Nothing to route to: readiness is not blocked on a provider probe.
        let registry = Registry::new(
            connectors(vec![("p1", Stub::with_reachable("p1", false))]),
            &[],
        )
        .unwrap();
        assert!(registry.reachable().await);
    }
}
