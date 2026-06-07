// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tracing and OpenTelemetry setup.
//!
//! Installs the standard tracing fmt subscriber, and if
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, additionally installs the
//! `tracing-opentelemetry` bridge with an OTLP HTTP/Protobuf exporter so request
//! spans flow to a collector. Without the endpoint set, the gateway runs with
//! local-only fmt logging.

use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::TracerProvider as SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const SERVICE_NAME: &str = "vaultplane-gateway";

/// Initialize logging and tracing. Returns the OTel tracer provider when OTLP
/// export is enabled so the caller can flush and shut it down on exit.
pub(crate) fn init() -> anyhow::Result<Option<SdkTracerProvider>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty());

    if let Some(endpoint) = endpoint {
        let traces_endpoint = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&traces_endpoint)
            .build()
            .context("failed to build OTLP span exporter")?;

        let resource = Resource::new(vec![KeyValue::new("service.name", SERVICE_NAME)]);

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(resource)
            .build();

        let tracer = provider.tracer(SERVICE_NAME);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(otel_layer)
            .init();

        tracing::info!(endpoint = traces_endpoint, "OTLP trace export enabled");
        Ok(Some(provider))
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        Ok(None)
    }
}

/// Flush and shut down the tracer provider, if one was installed.
pub(crate) fn shutdown(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider
        && let Err(err) = provider.shutdown()
    {
        tracing::warn!(error = ?err, "OTLP tracer provider shutdown reported an error");
    }
}
