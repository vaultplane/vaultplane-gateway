# Dynatrace

Send VaultPlane Gateway traces, metrics, and logs to Dynatrace through an
OpenTelemetry Collector, and visualize them with the starter dashboard.

## Ten-minute setup

1. **Create a Dynatrace API token** with the OTLP ingest scopes:
   `metrics.ingest`, `logs.ingest`, and `openTelemetryTrace.ingest`. Note your
   environment URL (for example `https://abc12345.live.dynatrace.com`).

2. **Run a Collector** with the preset, supplying the token and endpoint:

   ```bash
   export DT_ENDPOINT=https://abc12345.live.dynatrace.com
   export DT_API_TOKEN=dt0c01.XXXX...
   export VAULTPLANE_ADMIN_TOKEN=...     # so the Collector can scrape /admin/metrics
   otelcol-contrib --config integrations/collector/dynatrace.yaml
   ```

3. **Point the gateway at the Collector:**

   ```bash
   export OTEL_EXPORTER_OTLP_ENDPOINT=http://<collector-host>:4318
   ```

   Restart the gateway. Within a minute, traces appear under Distributed Traces,
   metrics under Metrics (keys prefixed `vaultplane_`), and the audit log stream
   under Logs (filter on `vaultplane.audit = true`).

4. **Import the dashboard.** Upload `dashboard.json` via Dashboards > Upload (or
   the Dashboards API). Adjust the metric keys if your environment ingests them
   under a different prefix.

## Metrics the gateway exposes

| Metric | Type | Key dimensions |
| --- | --- | --- |
| `vaultplane_requests_total` | counter | `provider`, `model`, `status` |
| `vaultplane_request_duration_seconds` | histogram | `provider`, `model`, `cache` |
| `vaultplane_cost_cents_total` | counter | `provider`, `model` |
| `vaultplane_rejections_total` | counter | `reason` |

Cost is reported in integer cents; divide by 100 for dollars. Per-key dimensions
(virtual key, team, app, env) live on the trace spans, not the metrics, so metric
cardinality stays bounded.

## Example metric selectors

- Request rate by model: `vaultplane_requests_total:splitBy("model"):rate`
- P99 latency: `vaultplane_request_duration_seconds:splitBy("model"):percentile(99)`
- Spend per provider (USD): `vaultplane_cost_cents_total:splitBy("provider"):rate / 100`
- Rejections by reason: `vaultplane_rejections_total:splitBy("reason"):rate`

## Trace attributes

Each request is one span carrying the OpenTelemetry GenAI conventions
(`gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, ...) plus
`vaultplane.*` attributes (`vaultplane.team`, `vaultplane.app`, `vaultplane.env`,
`vaultplane.cost_usd`, `vaultplane.cache.hit`, `vaultplane.provider.attempts`).
Use these to filter and group in Distributed Traces and Notebooks.
