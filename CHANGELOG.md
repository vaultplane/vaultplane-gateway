# Changelog

All notable changes to VaultPlane Gateway are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
from `v1.0.0` onward; pre-1.0 minor bumps may include breaking changes.

## [Unreleased]

### Added

* `SECURITY.md`, `CONTRIBUTING.md`, and `CHANGELOG.md` (this file) for
  pre-1.0 project hygiene.

## Pre-1.0 history

The project moved from empty scaffold to a working data plane in a
series of focused slices on `main`. The full commit log is the design
log; this section groups the slices by category for readers landing
fresh on the repo.

### Core proxy and provider surface

* OpenAI-compatible `POST /v1/chat/completions` with streaming and
  non-streaming responses.
* `POST /v1/embeddings` (OpenAI and Azure today; Anthropic and Bedrock
  return a clean "not supported" error inherited from a default trait
  impl).
* `GET /v1/models` lists the configured virtual models.
* OpenAI connector with model-name rewriting.
* Anthropic connector with full schema translation (request, response,
  streaming SSE, error envelope) to and from the OpenAI Chat
  Completions shape.
* Azure OpenAI connector mapping virtual model names to Azure
  deployment names via URL path.
* AWS Bedrock connector with hand-rolled SigV4 signing for the
  Anthropic Bedrock models.

### Routing and reliability

* Virtual model registry: each name maps to a primary provider plus
  ordered fallbacks, with retryable status codes, connector errors,
  and timeouts triggering automatic failover.
* Per-attempt timeout enforced at the registry layer (returns
  `Error::UpstreamTimeout`, surfaced as 504 by the proxy).
* OpenAI-shaped error envelope for upstream non-2xx responses
  (Anthropic's native error JSON is rewritten so clients see a
  consistent shape).
* Connector errors mapped to category-aware status: 504 for timeouts,
  502 for everything else, both with the underlying detail in the
  body.

### Auth, accounting, and policy

* Virtual keys (`vp_` prefix), 32 bytes of system entropy, SHA-256
  hashed at rest.
* Per-key model scope (allowlist with `*` wildcard).
* Per-key requests-per-second rate limit (token bucket).
* Per-key spend limit (USD per day, week, or month) with
  pre-check before upstream dispatch and post-call accounting.
* Per-key expiration (RFC3339 `expires_at`).
* Inline plugin chain on every chat and embeddings request, deciding
  `Pass | Modify | Reject`. Reference PII-redaction plugin ships
  in-tree (SSN, US credit card, US phone, email).

### Performance and caching

* Exact-match in-process response cache (`moka`, LRU,
  byte-weighted, TTL-bounded). Used by chat and embeddings.
* Streaming responses pass through chunk-by-chunk; the proxy never
  buffers a full upstream response.

### Admin API and CLI

* Admin API on a separate port: `/admin/healthz`, `/admin/readyz`,
  `/admin/status`, `/admin/metrics`, virtual key CRUD,
  `/admin/keys/{id}/spend` for per-key spend reports, and
  `/admin/config/reload` for hot reload.
* Static admin-token middleware (open when no token configured).
* `vaultplane-ctl`: online mode talks to the admin API
  (`key create | list | revoke`, `status`); offline mode generates
  keys locally and prints a YAML record for bootstrap.
  `config validate` and `config diff` reuse the gateway's own
  `Config::load`.

### Operability

* Layered configuration (defaults, YAML, environment variables with
  `VAULTPLANE_` prefix and `__` for nesting).
* Atomic hot reload: SIGHUP or `POST /admin/config/reload` swaps the
  whole `Runtime` bundle behind an `ArcSwap` handle. In-flight
  requests keep the snapshot they loaded; the next request sees the
  new one. Validation failures keep the old runtime active.
* Inbound TLS for the proxy listener via `axum-server` + rustls,
  with hot cert rotation through the same reload path.

### Observability

* OpenTelemetry spans on every request with GenAI semantic-convention
  attributes (`gen_ai.system`, `gen_ai.request.model`,
  `gen_ai.usage.input_tokens`, ...) plus `vaultplane.*` attributes
  for the virtual key, team, app, env, cost, and cache outcome.
  OTLP exporter is conditional on `OTEL_EXPORTER_OTLP_ENDPOINT`.
* Prometheus metrics on the admin port:
  `vaultplane_requests_total{provider, model, status}`,
  `vaultplane_request_duration_seconds{provider, model, cache}`,
  `vaultplane_cost_cents_total{provider, model}`,
  `vaultplane_rejections_total{reason}`. Cardinality bounded by
  design; per-key dimensions live on the spans, not the metric
  labels.

### Packaging and release

* Multi-arch (linux/amd64, linux/arm64) Docker image published to
  `ghcr.io/vaultplane/vaultplane-gateway` on every push to `main`
  and on `v*` tags. Built on `gcr.io/distroless/cc-debian12:nonroot`.
* Smoke test in CI: the workflow boots the just-built image and
  verifies `/admin/healthz` and `/admin/readyz` before the
  multi-arch push.
* Helm chart at `charts/vaultplane/`. Published to
  `oci://ghcr.io/vaultplane/charts/vaultplane` on `v*` tags.
* `helm-testing` in CI: full `ct install` against a `kind`
  cluster on every push (chart actually has to come ready).

### Documentation

* `README.md` opens for platform and security teams, scrolls down to
  operators, ends with an engineering deep dive.
* `CONFIGURATION.md` enumerates every YAML field with type, default,
  hot-reload behavior, and a one-line note.
* `examples/quickstart/` and `examples/observability/`: runnable
  docker-compose stacks for first-touch and a Jaeger + Prometheus
  setup.

[Unreleased]: https://github.com/vaultplane/vaultplane-gateway/compare/main...HEAD
