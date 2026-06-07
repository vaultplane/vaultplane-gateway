// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! In-process exact-match response cache.
//!
//! A moka LRU stores 200-status chat completion responses keyed by
//! `sha256(scope || 0 || model || 0 || body)`. The default scope is the virtual
//! key id, so each key gets its own slice. Capacity is byte-weighted by the
//! cached body length and entries expire after a configured TTL. Streaming
//! responses are never cached.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use moka::future::Cache;
use sha2::{Digest, Sha256};

use crate::provider::Usage;

/// A cached chat completion response.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Bytes,
    pub provider: String,
    pub model: String,
    pub usage: Option<Usage>,
}

/// An in-process LRU cache of chat completion responses, weighted by body bytes.
pub struct ResponseCache {
    inner: Cache<String, Arc<CachedResponse>>,
}

impl ResponseCache {
    /// Build a cache with the given byte budget and time-to-live.
    pub fn new(max_size_bytes: u64, ttl: Duration) -> Self {
        let inner = Cache::builder()
            .max_capacity(max_size_bytes)
            .weigher(|_key: &String, value: &Arc<CachedResponse>| {
                u32::try_from(value.body.len()).unwrap_or(u32::MAX)
            })
            .time_to_live(ttl)
            .build();
        Self { inner }
    }

    /// Compute the cache key for a request.
    ///
    /// The scope is typically the virtual key id, so each key keeps its own slice
    /// of the cache.
    pub fn key(scope: &str, model: &str, body: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(scope.as_bytes());
        hasher.update([0]);
        hasher.update(model.as_bytes());
        hasher.update([0]);
        hasher.update(body);
        hex::encode(hasher.finalize())
    }

    /// Look up a cached response.
    pub async fn get(&self, key: &str) -> Option<Arc<CachedResponse>> {
        self.inner.get(key).await
    }

    /// Insert a response into the cache.
    pub async fn insert(&self, key: String, value: Arc<CachedResponse>) {
        self.inner.insert(key, value).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedResponse, ResponseCache};
    use bytes::Bytes;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn insert_then_get_returns_the_response() {
        let cache = ResponseCache::new(1024 * 1024, Duration::from_secs(60));
        let key = ResponseCache::key("vp_test", "gpt-4o", b"{}");
        cache
            .insert(
                key.clone(),
                Arc::new(CachedResponse {
                    status: 200,
                    content_type: Some("application/json".to_string()),
                    body: Bytes::from_static(b"hello"),
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                    usage: None,
                }),
            )
            .await;
        let hit = cache.get(&key).await.expect("cache hit");
        assert_eq!(hit.status, 200);
        assert_eq!(hit.body, Bytes::from_static(b"hello"));
    }

    #[test]
    fn key_depends_on_scope_model_and_body() {
        let a = ResponseCache::key("scope1", "gpt-4o", b"{}");
        let b = ResponseCache::key("scope2", "gpt-4o", b"{}");
        let c = ResponseCache::key("scope1", "claude", b"{}");
        let d = ResponseCache::key("scope1", "gpt-4o", b"{\"x\":1}");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a, ResponseCache::key("scope1", "gpt-4o", b"{}"));
    }
}
