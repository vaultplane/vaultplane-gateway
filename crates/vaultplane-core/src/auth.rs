//! Virtual key authentication and rate limiting.
//!
//! A virtual key is an opaque bearer token (prefixed `vp_`) plus the scope it is
//! attributed to. The plaintext token is never persisted by the gateway: at-rest
//! storage holds a SHA-256 hex digest of the token, and incoming tokens are hashed
//! on every request before the key store is queried. Operators generate keys with
//! `vaultplane-ctl key create` and copy the resulting record into config.
//!
//! The PRD example shows `argon2id` for the at-rest hash, but argon2id is
//! intentionally slow (~10ms per verify) and would gate every gateway request on
//! that cost. API keys are high-entropy (32 random bytes), so a fast SHA-256 digest
//! is the correct choice and matches industry practice for API key storage.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TOKEN_PREFIX: &str = "vp_";
const TOKEN_BYTES: usize = 32;
const ID_PREFIX_LEN: usize = 12;

/// A virtual key record (the form stored in config and the gateway's `KeyStore`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualKey {
    /// Non-secret identifier safe to put in logs and spans (for example
    /// `vp_AbCdEfGhIjKl`).
    #[serde(default)]
    pub id: String,
    /// SHA-256 hex digest of the secret token. The plaintext token is never stored.
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub env: String,
    /// Allowed models. Empty, or containing `*`, allows any model.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-key rate limit in requests per second. `None` means no limit.
    #[serde(default)]
    pub rate_limit_rps: Option<u32>,
}

impl VirtualKey {
    /// An unscoped key used when no keys are configured (allows any model, no rate
    /// limit).
    pub fn anonymous() -> Self {
        Self {
            id: String::new(),
            hash: String::new(),
            team: String::new(),
            app: String::new(),
            env: String::new(),
            models: Vec::new(),
            rate_limit_rps: None,
        }
    }

    /// Whether this key is allowed to call the given model.
    pub fn allows_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == "*" || m == model)
    }

    /// A non-secret identifier for the key, safe to put in spans and logs.
    /// Returns `"anonymous"` when no `id` is set.
    pub fn identifier(&self) -> String {
        if self.id.is_empty() {
            "anonymous".to_string()
        } else {
            self.id.clone()
        }
    }
}

/// A freshly generated virtual key bundle.
pub struct GeneratedKey {
    /// The plaintext secret token: shown once at generation, then discarded by the
    /// gateway (never persisted).
    pub token: String,
    /// The non-secret identifier (`vp_` plus 12 characters from the token body).
    pub id: String,
    /// The SHA-256 hex digest of the token; this is what is stored at rest.
    pub hash: String,
}

/// Generate a fresh virtual key with 32 bytes of system entropy.
pub fn generate_key() -> GeneratedKey {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf).expect("system entropy must be available");
    let body = URL_SAFE_NO_PAD.encode(buf);
    let token = format!("{TOKEN_PREFIX}{body}");
    let id = format!("{TOKEN_PREFIX}{}", &body[..ID_PREFIX_LEN]);
    let hash = hash_token(&token);
    GeneratedKey { token, id, hash }
}

/// SHA-256 hex digest of an opaque bearer token.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// An in-memory lookup of virtual keys keyed by token hash.
#[derive(Debug, Clone, Default)]
pub struct KeyStore {
    by_hash: HashMap<String, VirtualKey>,
}

impl KeyStore {
    /// Build a store from a list of key records.
    pub fn new(keys: Vec<VirtualKey>) -> Self {
        let by_hash = keys.into_iter().map(|k| (k.hash.clone(), k)).collect();
        Self { by_hash }
    }

    /// Hash the incoming token and look up the matching key.
    pub fn authenticate(&self, token: &str) -> Option<&VirtualKey> {
        self.by_hash.get(&hash_token(token))
    }

    /// Number of configured keys.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether no keys are configured (proxy authentication is then disabled).
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// Constant-time comparison of two byte slices, for secrets like the admin token.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A per-key token-bucket rate limiter.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

struct TokenBucket {
    capacity: f64,
    refill_per_second: f64,
    available: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rps: u32) -> Self {
        let cap = f64::from(rps);
        Self {
            capacity: cap,
            refill_per_second: cap,
            available: cap,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.available = (self.available + elapsed * self.refill_per_second).min(self.capacity);
        self.last_refill = now;
        if self.available >= 1.0 {
            self.available -= 1.0;
            true
        } else {
            false
        }
    }
}

impl RateLimiter {
    /// Try to consume one request from the bucket for the given key id.
    ///
    /// Returns `true` if the request is allowed and `false` if the bucket is empty.
    pub fn check(&self, key_id: &str, rps: u32) -> bool {
        let mut buckets = self
            .buckets
            .lock()
            .expect("rate limiter mutex must not be poisoned");
        let bucket = buckets
            .entry(key_id.to_string())
            .or_insert_with(|| TokenBucket::new(rps));
        bucket.try_consume()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_scope_is_enforced() {
        let mut key = VirtualKey::anonymous();
        assert!(key.allows_model("anything"), "empty allowlist allows all");

        key.models = vec!["gpt-4o".to_string()];
        assert!(key.allows_model("gpt-4o"));
        assert!(!key.allows_model("gpt-3.5"));

        key.models = vec!["*".to_string()];
        assert!(key.allows_model("gpt-3.5"));
    }

    #[test]
    fn key_store_hashes_token_for_lookup() {
        let token = "vp_abc";
        let mut key = VirtualKey::anonymous();
        key.id = "vp_abc_id".to_string();
        key.hash = hash_token(token);
        let store = KeyStore::new(vec![key]);

        assert_eq!(store.len(), 1);
        assert!(store.authenticate(token).is_some());
        assert!(store.authenticate("vp_nope").is_none());
        assert!(KeyStore::default().is_empty());
    }

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secres"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }

    #[test]
    fn generates_a_key_with_prefix_and_matching_hash() {
        let key = generate_key();
        assert!(key.token.starts_with("vp_"));
        assert!(key.id.starts_with("vp_"));
        assert_eq!(key.id.len(), 3 + ID_PREFIX_LEN);
        assert_eq!(key.hash, hash_token(&key.token));
        // Two generations produce different tokens (overwhelming probability).
        let other = generate_key();
        assert_ne!(key.token, other.token);
    }

    #[test]
    fn rate_limiter_enforces_per_key_rps() {
        let limiter = RateLimiter::default();
        // Bucket starts full at capacity = rps.
        assert!(limiter.check("k1", 2));
        assert!(limiter.check("k1", 2));
        assert!(!limiter.check("k1", 2), "third request should be rejected");
        // A different key has its own bucket.
        assert!(limiter.check("k2", 2));
    }
}
