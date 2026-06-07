// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Virtual key authentication, rate limiting, and spend tracking.
//!
//! A virtual key is an opaque bearer token (prefixed `vp_`) plus the scope it is
//! attributed to. The plaintext token is never persisted by the gateway: at-rest
//! storage holds a SHA-256 hex digest of the token, and incoming tokens are hashed
//! on every request before the key store is queried. Operators generate keys with
//! `vaultplane-ctl key create` and copy the resulting record into config.
//!
//! Keys carry optional expiration (`expires_at`, RFC3339), a per-second rate limit,
//! and a per-period USD spend limit. The proxy enforces all three.
//!
//! The PRD example shows `argon2id` for the at-rest hash, but argon2id is
//! intentionally slow (~10ms per verify) and would gate every gateway request on
//! that cost. API keys are high-entropy (32 random bytes), so a fast SHA-256 digest
//! is the correct choice and matches industry practice for API key storage.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const TOKEN_PREFIX: &str = "vp_";
const TOKEN_BYTES: usize = 32;
const ID_PREFIX_LEN: usize = 12;

/// The period a spend limit resets over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    Month,
}

/// A per-period USD spend limit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SpendLimit {
    pub amount_usd: f64,
    pub period: Period,
}

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
    /// Per-period USD spend limit. `None` means no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<SpendLimit>,
    /// RFC3339 timestamp after which the key is rejected. `None` means no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl VirtualKey {
    /// An unscoped key used when no keys are configured (allows any model, no
    /// limits, no expiry).
    pub fn anonymous() -> Self {
        Self {
            id: String::new(),
            hash: String::new(),
            team: String::new(),
            app: String::new(),
            env: String::new(),
            models: Vec::new(),
            rate_limit_rps: None,
            spend_limit: None,
            expires_at: None,
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

    /// Whether the key has expired (per its RFC3339 `expires_at`).
    ///
    /// A `None` `expires_at` means no expiry. An unparseable timestamp is treated
    /// as expired so a typo in config locks the key out rather than silently
    /// disabling the expiry check.
    pub fn is_expired(&self) -> bool {
        match &self.expires_at {
            None => false,
            Some(when) => match OffsetDateTime::parse(when, &Rfc3339) {
                Ok(expiry) => OffsetDateTime::now_utc() > expiry,
                Err(_) => true,
            },
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
///
/// The store is interior-mutable so the admin API can issue and revoke keys at
/// runtime without taking the gateway down. Reads (authentication) take a
/// shared lock; writes (insert and remove) take an exclusive lock.
#[derive(Debug, Default)]
pub struct KeyStore {
    by_hash: RwLock<HashMap<String, VirtualKey>>,
}

impl KeyStore {
    /// Build a store from a list of key records.
    pub fn new(keys: Vec<VirtualKey>) -> Self {
        let by_hash: HashMap<String, VirtualKey> =
            keys.into_iter().map(|k| (k.hash.clone(), k)).collect();
        Self {
            by_hash: RwLock::new(by_hash),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, VirtualKey>> {
        self.by_hash
            .read()
            .expect("key store rwlock must not be poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, VirtualKey>> {
        self.by_hash
            .write()
            .expect("key store rwlock must not be poisoned")
    }

    /// Hash the incoming token and look up the matching key.
    pub fn authenticate(&self, token: &str) -> Option<VirtualKey> {
        self.read().get(&hash_token(token)).cloned()
    }

    /// Add a freshly issued key to the store. Silently overwrites on the
    /// astronomically unlikely event of a hash collision.
    pub fn insert(&self, key: VirtualKey) {
        self.write().insert(key.hash.clone(), key);
    }

    /// Remove a key by its non-secret identifier. Returns `true` if a key was
    /// removed.
    pub fn remove_by_id(&self, id: &str) -> bool {
        let mut guard = self.write();
        let hash = guard
            .iter()
            .find_map(|(h, k)| (k.id == id).then(|| h.clone()));
        match hash {
            Some(h) => {
                guard.remove(&h);
                true
            }
            None => false,
        }
    }

    /// Snapshot of every key currently in the store, in unspecified order.
    pub fn list(&self) -> Vec<VirtualKey> {
        self.read().values().cloned().collect()
    }

    /// Look up a key by its non-secret identifier (the `vp_` id, not the
    /// token). Returns a clone so the caller does not hold the lock.
    pub fn find_by_id(&self, id: &str) -> Option<VirtualKey> {
        self.read().values().find(|k| k.id == id).cloned()
    }

    /// Number of configured keys.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether no keys are configured (proxy authentication is then disabled).
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
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
    /// Returns `true` if the request is allowed.
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

    /// Drop the bucket for a key (used when the admin revokes the key).
    pub fn forget(&self, key_id: &str) {
        self.buckets
            .lock()
            .expect("rate limiter mutex must not be poisoned")
            .remove(key_id);
    }
}

/// Per-key cumulative USD spend, bucketed by period.
#[derive(Default)]
pub struct SpendTracker {
    entries: Mutex<HashMap<String, SpendState>>,
}

struct SpendState {
    period_token: u64,
    cumulative_usd: f64,
}

/// Encode the current period into a single integer for cheap equality comparison.
fn current_period_token(period: Period) -> u64 {
    let now = OffsetDateTime::now_utc();
    match period {
        Period::Day => (now.unix_timestamp() / 86_400) as u64,
        Period::Week => (now.year() as u64) * 100 + u64::from(now.iso_week()),
        Period::Month => (now.year() as u64) * 100 + u64::from(u8::from(now.month())),
    }
}

impl SpendTracker {
    /// Whether the key has remaining budget in the current period.
    pub fn pre_check(&self, key_id: &str, limit: &SpendLimit) -> bool {
        let token = current_period_token(limit.period);
        let entries = self
            .entries
            .lock()
            .expect("spend tracker mutex must not be poisoned");
        match entries.get(key_id) {
            Some(state) if state.period_token == token => state.cumulative_usd < limit.amount_usd,
            _ => true,
        }
    }

    /// Record the cost of a completed request against the current period. Resets
    /// the bucket on a period rollover.
    pub fn record(&self, key_id: &str, period: Period, cost: f64) {
        let token = current_period_token(period);
        let mut entries = self
            .entries
            .lock()
            .expect("spend tracker mutex must not be poisoned");
        let state = entries.entry(key_id.to_string()).or_insert(SpendState {
            period_token: token,
            cumulative_usd: 0.0,
        });
        if state.period_token != token {
            state.period_token = token;
            state.cumulative_usd = 0.0;
        }
        state.cumulative_usd += cost;
    }

    /// Drop the spend state for a key (used when the admin revokes the key).
    pub fn forget(&self, key_id: &str) {
        self.entries
            .lock()
            .expect("spend tracker mutex must not be poisoned")
            .remove(key_id);
    }

    /// Cumulative USD spend for `key_id` in the current period for `period`.
    /// Returns `0.0` when no spend has been recorded for the current period
    /// (including after a period rollover or for a key that has never spent).
    pub fn current_usd(&self, key_id: &str, period: Period) -> f64 {
        let token = current_period_token(period);
        let entries = self
            .entries
            .lock()
            .expect("spend tracker mutex must not be poisoned");
        match entries.get(key_id) {
            Some(state) if state.period_token == token => state.cumulative_usd,
            _ => 0.0,
        }
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
    fn key_store_find_by_id_returns_the_matching_record() {
        let store = KeyStore::default();
        let generated = generate_key();
        let mut key = VirtualKey::anonymous();
        key.id = generated.id.clone();
        key.hash = generated.hash.clone();
        key.team = "core".to_string();
        store.insert(key);

        let found = store.find_by_id(&generated.id).expect("key should exist");
        assert_eq!(found.id, generated.id);
        assert_eq!(found.team, "core");
        assert!(store.find_by_id("vp_nope").is_none());
    }

    #[test]
    fn spend_tracker_current_usd_reports_recorded_spend() {
        let tracker = SpendTracker::default();
        assert_eq!(
            tracker.current_usd("k1", Period::Day),
            0.0,
            "fresh key reports zero"
        );

        tracker.record("k1", Period::Day, 1.25);
        tracker.record("k1", Period::Day, 0.75);
        assert!(
            (tracker.current_usd("k1", Period::Day) - 2.0).abs() < 1e-9,
            "should sum to 2.0"
        );

        // A different period is its own bucket.
        assert_eq!(tracker.current_usd("k1", Period::Week), 0.0);
        // A different key is its own bucket.
        assert_eq!(tracker.current_usd("k2", Period::Day), 0.0);
    }

    #[test]
    fn key_store_insert_and_remove_are_visible_to_authenticate() {
        let store = KeyStore::default();
        let generated = generate_key();

        let mut key = VirtualKey::anonymous();
        key.id = generated.id.clone();
        key.hash = generated.hash.clone();
        store.insert(key);

        assert_eq!(store.len(), 1);
        assert!(
            store.authenticate(&generated.token).is_some(),
            "freshly issued key authenticates"
        );

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, generated.id);

        assert!(store.remove_by_id(&generated.id), "removal reports success");
        assert!(
            store.authenticate(&generated.token).is_none(),
            "revoked key no longer authenticates"
        );
        assert!(
            !store.remove_by_id(&generated.id),
            "second removal is a no-op"
        );
        assert!(store.is_empty());
    }

    #[test]
    fn rate_limiter_forget_clears_the_bucket() {
        let limiter = RateLimiter::default();
        assert!(limiter.check("k1", 1));
        assert!(!limiter.check("k1", 1), "bucket is empty");
        limiter.forget("k1");
        assert!(limiter.check("k1", 1), "forget restores a fresh bucket");
    }

    #[test]
    fn spend_tracker_forget_clears_state() {
        let tracker = SpendTracker::default();
        let limit = SpendLimit {
            amount_usd: 1.0,
            period: Period::Day,
        };
        tracker.record("k1", Period::Day, 2.0);
        assert!(
            !tracker.pre_check("k1", &limit),
            "over the limit before forget"
        );
        tracker.forget("k1");
        assert!(
            tracker.pre_check("k1", &limit),
            "forget resets the period total"
        );
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
        let other = generate_key();
        assert_ne!(key.token, other.token);
    }

    #[test]
    fn rate_limiter_enforces_per_key_rps() {
        let limiter = RateLimiter::default();
        assert!(limiter.check("k1", 2));
        assert!(limiter.check("k1", 2));
        assert!(!limiter.check("k1", 2), "third request should be rejected");
        assert!(limiter.check("k2", 2));
    }

    #[test]
    fn expiration_is_evaluated_against_now() {
        let mut key = VirtualKey::anonymous();
        assert!(!key.is_expired(), "no expires_at means no expiry");

        key.expires_at = Some("1970-01-01T00:00:00Z".to_string());
        assert!(key.is_expired(), "long past timestamp is expired");

        key.expires_at = Some("2999-12-31T23:59:59Z".to_string());
        assert!(!key.is_expired(), "distant future is not expired");

        key.expires_at = Some("not a timestamp".to_string());
        assert!(
            key.is_expired(),
            "unparseable timestamps are treated as expired"
        );
    }

    #[test]
    fn spend_tracker_blocks_after_limit_reached() {
        let tracker = SpendTracker::default();
        let limit = SpendLimit {
            amount_usd: 1.0,
            period: Period::Day,
        };

        assert!(tracker.pre_check("k1", &limit), "fresh key has budget");
        tracker.record("k1", Period::Day, 0.6);
        assert!(
            tracker.pre_check("k1", &limit),
            "0.6 < 1.0, still has budget"
        );
        tracker.record("k1", Period::Day, 0.5);
        assert!(
            !tracker.pre_check("k1", &limit),
            "1.1 >= 1.0, over the limit"
        );

        // A different key has its own bucket.
        assert!(tracker.pre_check("k2", &limit));
    }
}
