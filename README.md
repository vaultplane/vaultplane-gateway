# VaultPlane Gateway

**Every model call, on policy.**

VaultPlane Gateway is an open-source, low-latency proxy for AI model traffic.
It gives platform and security teams one path to route, govern, and observe
every call their AI systems make, across commercial and self-hosted models.

It is the enforcement plane of [VaultPlane](https://vaultplane.com), the
control plane for AI infrastructure. The VaultPlane Registry defines what is
trusted; the Gateway enforces it on the wire.

> ⚠️ **Pre-1.0.** The runtime is shipping in the open. The surfaces below
> work end-to-end and are covered by tests, but APIs and the config schema
> may still change before 1.0. Pin to a specific commit.

## What works today

* **OpenAI-compatible API.** `POST /v1/chat/completions` (streaming and
  non-streaming), `POST /v1/embeddings`, `GET /v1/models`.
* **Multi-provider routing.** OpenAI, Anthropic, Azure OpenAI, AWS Bedrock
  (with hand-rolled SigV4). One virtual model name maps to a primary
  provider plus ordered fallbacks, with automatic failover on retryable
  status codes, connector errors, and timeouts. Embeddings route through
  OpenAI and Azure today; Anthropic Messages has no embeddings endpoint,
  and Bedrock embedding shapes (Titan, Cohere) are a follow-up.
* **Cross-provider schema translation.** Anthropic and Bedrock requests are
  translated to and from the OpenAI Chat Completions schema. Anthropic
  streaming is transformed into OpenAI SSE on the fly.
* **Virtual keys.** Bearer tokens (`vp_` prefix), SHA-256 hashed at rest,
  with per-key model scope, requests-per-second rate limit, per-period spend
  limit (day/week/month), and optional RFC3339 expiry.
* **Admin API and CLI.** Issue, list, and revoke keys at runtime through the
  admin API. `vaultplane-ctl` is the operator companion.
* **Exact-match cache.** Optional in-process cache for deterministic
  responses (chat and embeddings). LRU, byte-weighted, TTL-bounded.
* **Inline plugin host.** Trait-based plugin chain runs on every chat
  request. A reference PII-redaction plugin ships in-tree.
* **Inbound TLS.** rustls-backed HTTPS for the proxy listener, with hot cert
  rotation through the same reload path as config.
* **Config hot-reload.** SIGHUP (Unix) or `POST /admin/config/reload` swaps
  the runtime atomically. Validation failures keep the old runtime active.
* **Observability.** OpenTelemetry spans on every request with GenAI
  semantic-convention attributes plus `vaultplane.*` attributes. Prometheus
  metrics at `/admin/metrics`.

## Quick start

VaultPlane Gateway is written in Rust (stable, edition 2024). Build the data
plane and the operator CLI:

```bash
cargo build --release -p vaultplane -p vaultplane-ctl
```

Set provider keys for whichever upstreams you intend to use:

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export VAULTPLANE_ADMIN_TOKEN=$(openssl rand -hex 32)
```

Run with a config file:

```bash
./target/release/vaultplane --config vaultplane.yaml
```

The proxy listens on `0.0.0.0:8080` by default; the admin API listens on
`0.0.0.0:9091`. With no virtual keys configured, the proxy is open (useful
for local development).

Issue a virtual key and call the proxy:

```bash
./target/release/vaultplane-ctl \
  --endpoint http://localhost:9091 \
  key create --team core --app web --env dev --model gpt-4o

curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer vp_..." \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

## Run with Docker

Published images live at `ghcr.io/vaultplane/vaultplane-gateway`. Every push
to `main` produces a new image; tagged releases (`vX.Y.Z`) get semver tags
plus `latest`.

```bash
docker pull ghcr.io/vaultplane/vaultplane-gateway:main

docker run --rm -p 8080:8080 -p 9091:9091 \
  -e OPENAI_API_KEY \
  -e VAULTPLANE_ADMIN_TOKEN \
  -v "$(pwd)/vaultplane.yaml:/etc/vaultplane/vaultplane.yaml:ro" \
  ghcr.io/vaultplane/vaultplane-gateway:main \
  --config /etc/vaultplane/vaultplane.yaml
```

The image is built from `gcr.io/distroless/cc-debian12:nonroot`: no shell,
no package manager, runs as the non-root user (UID 65532). Linux/amd64
today; multi-arch (linux/arm64) is a follow-up.

## Configuration

Configuration is layered: defaults, then an optional YAML file passed with
`--config`, then environment variables prefixed `VAULTPLANE_` (nested keys
split on `__`). A minimal file:

```yaml
listen:
  address: "0.0.0.0:8080"
  admin_address: "0.0.0.0:9091"
  tls:
    cert_path: "/etc/vaultplane/cert.pem"
    key_path: "/etc/vaultplane/key.pem"

providers:
  openai:
    base_url: "https://api.openai.com"
    api_key_env: "OPENAI_API_KEY"

models:
  - name: smart
    primary: { provider: openai, model: gpt-4o }
    fallbacks:
      - { provider: anthropic, model: claude-3-7-sonnet }
    retry_on: [502, 503, 504]
    timeout_ms: 30000

cache:
  enabled: true
  size_mb: 64
  ttl_seconds: 300

plugins:
  - type: pii_redaction
    patterns: [ssn, credit_card, phone_us, email]
    replacement: "[REDACTED]"
```

Validate a file before deploying it:

```bash
vaultplane-ctl config validate vaultplane.yaml
vaultplane-ctl config diff vaultplane.yaml vaultplane.new.yaml
```

## Proxy API (`/v1/*`)

The proxy port speaks OpenAI Chat Completions. Authenticate with a virtual
key in `Authorization: Bearer vp_<token>`. Streaming, non-streaming,
embeddings, and the model list all work. A model that is not in the
configured registry routes by name prefix (`claude` to Anthropic, everything
else to OpenAI). Cache hits return with `x-vaultplane-cache: HIT`.

Provider errors are forwarded with the original status code, rewritten into
the OpenAI error envelope for cross-provider consistency. Connector failures
return 502 (or 504 for upstream timeouts) with `{"error": {"message": ...,
"type": "upstream_error" | "upstream_timeout"}}`.

## Admin API (`/admin/*`)

The admin API binds to a separate port (default `0.0.0.0:9091`), intended
for cluster-internal access. Protected endpoints require the bearer token in
`VAULTPLANE_ADMIN_TOKEN`; health and readiness probes are always open.

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| GET | `/admin/healthz` | open | liveness probe |
| GET | `/admin/readyz` | open | readiness probe |
| GET | `/admin/status` | token | version, uptime, key count |
| GET | `/admin/metrics` | token | Prometheus text format |
| GET | `/admin/keys` | token | list virtual keys (no hashes) |
| POST | `/admin/keys` | token | issue a new key (returns token once) |
| DELETE | `/admin/keys/{id}` | token | revoke a key |
| POST | `/admin/config/reload` | token | reload config and rotate certs |

## `vaultplane-ctl`

The operator CLI talks to the admin API over HTTP and is the recommended way
to manage keys, validate config, and inspect status. With `--endpoint` set
(or `VAULTPLANE_ADMIN_ENDPOINT`), every command targets the running gateway.

```bash
vaultplane-ctl --endpoint http://localhost:9091 status
vaultplane-ctl key list
vaultplane-ctl key create --team core --app web --env prod --model gpt-4o
vaultplane-ctl key revoke vp_AbCdEfGhIjKl
vaultplane-ctl config validate vaultplane.yaml
vaultplane-ctl config diff vaultplane.yaml vaultplane.new.yaml
```

Without `--endpoint`, `key create` falls back to an offline mode: it
generates a key locally and prints a YAML record to paste into `auth.keys`
for bootstrap.

## Observability

Every request is recorded as a tracing span with OpenTelemetry GenAI
semantic-convention attributes (`gen_ai.system`, `gen_ai.request.model`,
`gen_ai.usage.input_tokens`, ...) plus `vaultplane.*` attributes for the
virtual key, team, app, env, cost, and cache hit. Set
`OTEL_EXPORTER_OTLP_ENDPOINT` to forward spans to a collector. The gateway
does not embed the OpenTelemetry Collector; run it as a sidecar.

Prometheus metrics at `/admin/metrics`:

* `vaultplane_requests_total{provider, model, status}`
* `vaultplane_request_duration_seconds{provider, model, cache}`
* `vaultplane_cost_cents_total{provider, model}` (integer cents; divide by
  100 for dollars)
* `vaultplane_rejections_total{reason}` (auth, expired, rate_limit,
  spend_limit, forbidden_model, plugin, upstream_error)

Label cardinality is bounded by design. Virtual key id, team, app, and env
go on the spans, not on the metrics labels, so multi-tenant load does not
explode the registry.

## VaultPlane: two planes

VaultPlane is the control plane for AI infrastructure, split into two
planes:

* **VaultPlane Registry**: the definition plane. Discover, evaluate,
  certify, and govern MCP servers and AI skills. Browse it at
  https://vaultplane.com.
* **VaultPlane Gateway** (this repository): the enforcement plane. Routes
  and governs model and tool traffic on every call.

Policy defined in the Registry is enforced by the Gateway: define trust
once, enforce it everywhere.

## License

[Apache License 2.0](LICENSE).

---

Looking for a managed control plane? See https://vaultplane.com/cloud.
