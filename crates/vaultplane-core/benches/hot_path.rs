// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Criterion microbenchmarks for the gateway's per-request hot path.
//!
//! These measure the synchronous work every request pays, in isolation from the
//! network: virtual-key authentication (token hashing plus keystore lookup),
//! cache-key derivation (hashing the normalized request), and per-request cost
//! accounting. They exist to catch regressions in the components the N1 latency
//! budget depends on.
//!
//! Run with `cargo bench -p vaultplane-core`.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use vaultplane_core::auth::{KeyStore, VirtualKey, generate_key, hash_token};
use vaultplane_core::cache::ResponseCache;
use vaultplane_core::config::{ModelPricing, Pricing};
use vaultplane_core::cost;
use vaultplane_core::provider::Usage;

/// A representative chat request body (roughly 1 KB of messages).
fn sample_body() -> Vec<u8> {
    let content = "Summarize the following text in three sentences. ".repeat(20);
    format!(r#"{{"model":"chat-default","messages":[{{"role":"user","content":"{content}"}}]}}"#)
        .into_bytes()
}

fn bench_auth(c: &mut Criterion) {
    let mut group = c.benchmark_group("auth");

    let token = generate_key().token;
    group.bench_function("hash_token", |b| b.iter(|| hash_token(black_box(&token))));

    // A keystore with a realistic number of keys; the looked-up token sits in
    // the middle so the measurement is not a best-case first hit.
    let mut keys = Vec::with_capacity(1000);
    let mut known = String::new();
    for i in 0..1000 {
        let generated = generate_key();
        if i == 500 {
            known = generated.token.clone();
        }
        keys.push(VirtualKey {
            id: generated.id,
            hash: generated.hash,
            ..VirtualKey::anonymous()
        });
    }
    let store = KeyStore::new(keys);
    group.bench_function("authenticate_1000_keys", |b| {
        b.iter(|| store.authenticate(black_box(&known)))
    });

    group.finish();
}

fn bench_cache_key(c: &mut Criterion) {
    let body = sample_body();
    c.bench_function("cache/key", |b| {
        b.iter(|| {
            ResponseCache::key(
                black_box("vp_AbCdEfGhIjKl"),
                black_box("chat-default"),
                black_box(&body),
            )
        })
    });
}

fn bench_cost(c: &mut Criterion) {
    let mut models = HashMap::new();
    models.insert(
        "gpt-4o".to_string(),
        ModelPricing {
            input_per_1k_tokens_usd: 0.005,
            output_per_1k_tokens_usd: 0.015,
        },
    );
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), models);
    let pricing = Pricing { providers };
    let usage = Usage {
        prompt_tokens: 1200,
        completion_tokens: 800,
    };

    c.bench_function("cost/compute", |b| {
        b.iter(|| {
            cost::compute(
                black_box(&pricing),
                black_box("openai"),
                black_box("gpt-4o"),
                black_box(&usage),
            )
        })
    });
}

criterion_group! {
    name = benches;
    // A short warm-up/measurement keeps these quick; the absolute numbers are
    // sub-microsecond, so the defaults would over-sample.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets = bench_auth, bench_cache_key, bench_cost
}
criterion_main!(benches);
