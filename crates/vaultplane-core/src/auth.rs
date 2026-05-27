//! Virtual key authentication.
//!
//! A virtual key is an opaque bearer token (prefixed `vp_`) plus the scope it is
//! attributed to. Keys are loaded from configuration and matched by token. At-rest
//! hashing/encryption and rate/spend-limit enforcement are not yet implemented.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A virtual key: an opaque token plus the scope it is attributed to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualKey {
    /// The opaque bearer token (prefixed `vp_`).
    pub token: String,
    /// Team the key is attributed to.
    #[serde(default)]
    pub team: String,
    /// Application the key is attributed to.
    #[serde(default)]
    pub app: String,
    /// Environment the key is attributed to (for example `prod`).
    #[serde(default)]
    pub env: String,
    /// Allowed models. Empty, or containing `*`, allows any model.
    #[serde(default)]
    pub models: Vec<String>,
}

impl VirtualKey {
    /// An unscoped key used when no keys are configured (allows any model).
    pub fn anonymous() -> Self {
        Self {
            token: String::new(),
            team: String::new(),
            app: String::new(),
            env: String::new(),
            models: Vec::new(),
        }
    }

    /// Whether this key is allowed to call the given model.
    pub fn allows_model(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == "*" || m == model)
    }
}

/// An in-memory lookup of virtual keys by token.
#[derive(Debug, Clone, Default)]
pub struct KeyStore {
    by_token: HashMap<String, VirtualKey>,
}

impl KeyStore {
    /// Build a store from a list of keys.
    pub fn new(keys: Vec<VirtualKey>) -> Self {
        let by_token = keys.into_iter().map(|k| (k.token.clone(), k)).collect();
        Self { by_token }
    }

    /// Look up a key by its token.
    pub fn authenticate(&self, token: &str) -> Option<&VirtualKey> {
        self.by_token.get(token)
    }

    /// Number of configured keys.
    pub fn len(&self) -> usize {
        self.by_token.len()
    }

    /// Whether no keys are configured (proxy authentication is then disabled).
    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// Constant-time comparison of two byte slices, to compare secrets without leaking
/// per-byte timing. The length comparison is not constant time.
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
    fn key_store_looks_up_by_token() {
        let mut key = VirtualKey::anonymous();
        key.token = "vp_abc".to_string();
        let store = KeyStore::new(vec![key]);

        assert_eq!(store.len(), 1);
        assert!(store.authenticate("vp_abc").is_some());
        assert!(store.authenticate("vp_nope").is_none());
        assert!(KeyStore::default().is_empty());
    }

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secres"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }
}
