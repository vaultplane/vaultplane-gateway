// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tracing and OpenTelemetry setup.
//!
//! Installs the standard tracing fmt subscriber, and if
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, additionally installs the
//! `tracing-opentelemetry` bridge (request spans flow to a collector as OTLP
//! traces) and an OTLP logs bridge (every tracing event, including the audit
//! stream, flows to the collector as OTLP logs). Without the endpoint set, the
//! gateway runs with local-only fmt logging.

use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::LoggerProvider as SdkLoggerProvider;
use opentelemetry_sdk::trace::TracerProvider as SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const SERVICE_NAME: &str = "vaultplane-gateway";

/// The OpenTelemetry providers installed when OTLP export is enabled, returned so
/// the caller can flush and shut them down on exit.
#[derive(Default)]
pub(crate) struct Providers {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
}

/// Initialize logging and tracing. Returns the OpenTelemetry providers (empty
/// when OTLP export is disabled) so the caller can shut them down on exit.
pub(crate) fn init() -> anyhow::Result<Providers> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty());

    let Some(endpoint) = endpoint else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        return Ok(Providers::default());
    };

    let base = endpoint.trim_end_matches('/');
    let resource = Resource::new(vec![KeyValue::new("service.name", SERVICE_NAME)]);

    // Traces: request spans to {endpoint}/v1/traces.
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/traces"))
        .build()
        .context("failed to build OTLP span exporter")?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource.clone())
        .build();
    let tracer = tracer_provider.tracer(SERVICE_NAME);
    let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Logs: every tracing event (including the audit stream) to {endpoint}/v1/logs.
    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{base}/v1/logs"))
        .build()
        .context("failed to build OTLP log exporter")?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build();
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .init();

    tracing::info!(endpoint = base, "OTLP trace and log export enabled");
    Ok(Providers {
        tracer: Some(tracer_provider),
        logger: Some(logger_provider),
    })
}

/// Flush and shut down any installed OpenTelemetry providers.
pub(crate) fn shutdown(providers: Providers) {
    if let Some(tracer) = providers.tracer
        && let Err(err) = tracer.shutdown()
    {
        tracing::warn!(error = ?err, "OTLP tracer provider shutdown reported an error");
    }
    if let Some(logger) = providers.logger
        && let Err(err) = logger.shutdown()
    {
        tracing::warn!(error = ?err, "OTLP logger provider shutdown reported an error");
    }
}
