# Integrations

VaultPlane Gateway integrates with the rest of your stack through three surfaces
that need no per-vendor code in the gateway: OpenTelemetry (OTLP), inline Wasm
plugins, and webhooks. Most observability integrations are therefore a
configuration exercise, not an engineering one.

The gateway does not embed the OpenTelemetry Collector. It emits OTLP and exposes
Prometheus metrics; you run a Collector alongside it (a sidecar, a DaemonSet, or
a host process) and point it at your backend. The files here are ready-to-edit
Collector presets and launch-partner materials.

## What the gateway emits

| Signal | How | Where |
| --- | --- | --- |
| Traces | OTLP HTTP, one span per request with GenAI + `vaultplane.*` attributes | `${OTEL_EXPORTER_OTLP_ENDPOINT}/v1/traces` |
| Logs | OTLP HTTP, every event including the `vaultplane.audit=true` stream | `${OTEL_EXPORTER_OTLP_ENDPOINT}/v1/logs` |
| Metrics | Prometheus text, scraped (not pushed) | `:9091/admin/metrics` (admin token) |

Set `OTEL_EXPORTER_OTLP_ENDPOINT` on the gateway to your Collector's OTLP HTTP
address, for example `http://otel-collector:4318`. The gateway appends
`/v1/traces` and `/v1/logs` itself.

## Collector presets

`collector/` holds one Collector config per backend. Each receives OTLP from the
gateway, scrapes the gateway's Prometheus metrics, and exports traces, metrics,
and logs to that backend. Credentials are read from environment variables, so no
secrets live in the file.

| Backend | Preset | Notes |
| --- | --- | --- |
| Dynatrace | `collector/dynatrace.yaml` | OTLP via the environment's OTLP endpoint. Launch partner; see `dynatrace/`. |
| Datadog | `collector/datadog.yaml` | Requires the Collector Contrib distribution (datadog exporter). |
| Splunk Observability | `collector/splunk-observability.yaml` | Requires Contrib (sapm + signalfx exporters). |
| New Relic | `collector/new-relic.yaml` | OTLP. |
| Grafana Cloud | `collector/grafana-cloud.yaml` | OTLP with basic auth. |
| Elastic | `collector/elastic.yaml` | OTLP to Elastic APM. |
| Honeycomb | `collector/honeycomb.yaml` | OTLP. |

Run a Collector with one of these:

```bash
export VAULTPLANE_ADMIN_TOKEN=...     # so the Collector can scrape /admin/metrics
export DT_ENDPOINT=... DT_API_TOKEN=...   # backend-specific (see each file's header)
otelcol-contrib --config integrations/collector/dynatrace.yaml
```

## Launch partners

- `dynatrace/`: the Collector preset plus a starter dashboard and a ten-minute
  setup guide.
- `rubrik/`: the audit-stream feed for Rubrik Agent Rewind. This is a stub
  pending Rubrik's finalized ingest schema; see its README.

## Adding a backend

If a backend ingests OTLP, copy the closest preset, swap the exporter block, and
send a PR. Bespoke connectors are reserved for partnerships with mutual product
value; everything else rides OTLP, a plugin, or a webhook.
