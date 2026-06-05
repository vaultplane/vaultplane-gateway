# Observability example

Gateway + Jaeger (for traces) + Prometheus (for metrics), all on one
docker-compose network. No code or instrumentation needed in any
application: every call through the gateway is observed.

## Run

```bash
export OPENAI_API_KEY=sk-...
docker compose up
```

## Make a request

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "smart",
    "messages": [{"role": "user", "content": "say hi in five words"}]
  }'
```

## See traces in Jaeger

Open http://localhost:16686 in your browser. Pick the
`vaultplane-gateway` service from the dropdown and click "Find Traces".
Each request is one span tagged with GenAI semantic-convention attributes
(`gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`,
...) plus `vaultplane.*` attributes for the virtual key, team, app, env,
cost, and cache outcome.

## See metrics in Prometheus

Open http://localhost:9090. Try these queries:

```promql
# Request rate per provider over the last minute
rate(vaultplane_requests_total[1m])

# p95 latency by provider
histogram_quantile(
  0.95,
  sum by (le, provider) (rate(vaultplane_request_duration_seconds_bucket[5m]))
)

# Cache hit rate
sum(rate(vaultplane_request_duration_seconds_count{cache="hit"}[5m])) /
sum(rate(vaultplane_request_duration_seconds_count[5m]))

# Why are we rejecting requests?
sum by (reason) (rate(vaultplane_rejections_total[5m]))

# Cost in dollars per provider (cents / 100)
sum by (provider) (vaultplane_cost_cents_total) / 100
```

## What this shows

* The gateway emits OTLP traces to whatever endpoint
  `OTEL_EXPORTER_OTLP_ENDPOINT` points at. Jaeger 1.62 receives OTLP
  natively on port 4318; that's the address the gateway is configured
  to send to.
* Prometheus scrapes `/admin/metrics` on the gateway's admin port
  directly. No exporter sidecar.
* The Gateway does not embed an OpenTelemetry Collector; the Jaeger
  container here plays that role for the example. In production, point
  the gateway at your own Collector (or directly at any OTLP-compatible
  backend).

## Production notes

* Gate `/admin/metrics` with a token: set `VAULTPLANE_ADMIN_TOKEN` on
  the gateway and configure Prometheus with
  `authorization.credentials_file`. See the comment in
  [`prometheus.yml`](./prometheus.yml).
* Lower the scrape interval if you want sub-5s granularity; raise it to
  reduce load.
* Run the Collector as a sidecar (not as Jaeger) in real deployments so
  the trace pipeline survives Jaeger maintenance.
