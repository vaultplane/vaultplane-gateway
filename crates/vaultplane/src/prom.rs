// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Prometheus metrics for the gateway.
//!
//! The gateway installs a global `metrics` recorder once at startup; the
//! returned [`PrometheusHandle`] is shared with the admin module, which renders
//! the current snapshot from `GET /admin/metrics` in the Prometheus text
//! exposition format.
//!
//! Metric names live in the [`names`] submodule and label values in
//! [`label_values`] so any rename is a single-line change. Cardinality is
//! deliberately bounded: labels are restricted to provider, model, status, a
//! small cache (hit/miss) tag, and a small rejection-reason tag. Virtual key
//! ids, teams, apps, and envs are NOT used as labels because they grow without
//! bound under multi-tenant load; per-key dimensions belong on the
//! OpenTelemetry trace spans, not on the metrics path.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder and return a clone of the handle.
///
/// Idempotent: subsequent calls return the original handle and do not attempt
/// to install a second recorder (which `metrics` would reject).
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus recorder")
        })
        .clone()
}

/// Metric names. Centralized so a rename is a one-line change.
pub mod names {
    pub const REQUESTS_TOTAL: &str = "vaultplane_requests_total";
    pub const REQUEST_DURATION_SECONDS: &str = "vaultplane_request_duration_seconds";
    /// Cumulative upstream cost in US-dollar cents (the `metrics` crate only
    /// offers integer counters; cents keeps the unit useful at sub-dollar
    /// granularity). Divide by 100 in dashboards to display dollars.
    pub const COST_CENTS_TOTAL: &str = "vaultplane_cost_cents_total";
    pub const REJECTIONS_TOTAL: &str = "vaultplane_rejections_total";
}

/// Stable label values used across the codebase. Kept narrow to bound cardinality.
pub mod label_values {
    pub const CACHE_HIT: &str = "hit";
    pub const CACHE_MISS: &str = "miss";

    pub const REASON_AUTH: &str = "auth";
    pub const REASON_EXPIRED: &str = "expired";
    pub const REASON_RATE_LIMIT: &str = "rate_limit";
    pub const REASON_SPEND_LIMIT: &str = "spend_limit";
    pub const REASON_FORBIDDEN_MODEL: &str = "forbidden_model";
    pub const REASON_PLUGIN: &str = "plugin";
    pub const REASON_UPSTREAM_ERROR: &str = "upstream_error";
}
