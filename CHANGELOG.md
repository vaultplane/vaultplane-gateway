# Changelog

All notable changes to VaultPlane Gateway are recorded here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
from `v1.0.0` onward; pre-1.0 minor bumps may include breaking changes.

## [Unreleased]

## [1.0.0] - 2026-06-07

### Added

* `control_plane.mode` (`file` | `api`) selects the configuration source. The
  Cloud `api` path is a stub in this release: the gateway logs and serves from
  last-known-good local configuration, so the data plane keeps running whether or
  not a control plane is reachable.
* A default pricing table is bundled with the binary (common OpenAI, Anthropic,
  Azure, and Bedrock models), so cost is reported out of the box; `pricing`
  config entries override or extend it.
* Optional request headers: `X-VaultPlane-Trace-Id` is recorded on the request
  span, and `X-VaultPlane-Idempotency-Key` keys the cache instead of the request
  body.
* `shutdown.drain_timeout_seconds` (default 30) makes the graceful-drain window
  on SIGTERM configurable, with a hard cap that forces exit if draining overruns.
* Wasm plugins can be loaded from `http(s)://` and `file://` URLs, not just local
  paths; remote components are downloaded at load time.
* WebAssembly plugin host (wasmtime, Component Model). Loads components
  implementing the `inspect-request` WIT contract, runs them in a WASI
  sandbox with a fresh store per call, enforces each plugin's latency
  budget with an epoch-deadline trap, and fails open or closed per route
  policy on overrun, trap, or load failure. Configured via the `wasm`
  plugin type, with `latency_budget_ms`, `on_timeout`, and `bind_routes`.
* The reference PII-redaction plugin now ships as a real WebAssembly
  component (built for `wasm32-wasip2`), redacting SSN, US credit card, US
  phone, and email patterns in chat request bodies.
* Plugins can be bound to specific virtual models via `bind_routes`;
  unbound plugins run on every request.
* Structured audit log. Administrative actions and policy decisions
  (`key.create`, `key.revoke`, `config.reload`, `plugin.load`,
  `plugin.reject`, `failover`) are emitted on the `vaultplane::audit` tracing
  target, tagged `vaultplane.audit=true` with canonical `action`, `actor`,
  `subject`, and `outcome` fields plus action-specific metadata.
* OTLP logs export. When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, every tracing
  event (including the audit stream) is exported as OTLP logs to `/v1/logs`,
  alongside the existing span export to `/v1/traces`.
* Optional mutual TLS on the proxy listener via `listen.tls.client_ca_path`.
  When set, clients must present a certificate chaining to the configured CA
  bundle or the connection is refused at the handshake. Toggling mTLS on or off
  is hot-reloadable within an existing `tls:` block.
* `GET /admin/models` lists the configured virtual models and their providers,
  and `vaultplane-ctl model list` is now implemented against it.
* Criterion microbenchmarks for the per-request hot path (virtual-key auth and
  token hashing, cache-key derivation, cost accounting), run as a compile check
  in CI.
* End-to-end latency harness that drives the full inbound path against a local
  upstream simulator and reports P50/P99 gateway overhead (the N1 budget). It is
  `#[ignore]`d in the normal test run and executed explicitly in CI.
* Integrations under `integrations/`: OpenTelemetry Collector presets that take
  the gateway's OTLP traces/logs and scraped Prometheus metrics to Dynatrace,
  Datadog, Splunk Observability, New Relic, Grafana Cloud, Elastic, and
  Honeycomb. The Dynatrace launch partner ships a starter dashboard and a
  ten-minute setup guide; the Rubrik Agent Rewind feed (audit stream plus
  request spans) ships as a stub pending Rubrik's finalized ingest schema.
* Release workflow: on a `v*` tag, a guard verifies the tag matches the crate
  version, then static binaries (Linux amd64/arm64 via musl, macOS arm64) each
  bundling `vaultplane` and `vaultplane-ctl`, plus the reference PII plugin
  `.wasm`, are built, checksummed, and attached to a GitHub Release with notes
  drawn from this changelog. The Docker image and Helm chart continue to publish
  from their own tag-triggered workflows.

### Changed

* Outbound provider TLS now uses rustls (with the OS trust store) instead of
  native-tls/OpenSSL. This removes the OpenSSL build dependency, so the static
  Linux (musl) release binaries cross-compile cleanly, and matches the inbound
  TLS stack.
* `/admin/readyz` now reflects provider reachability. A background prober
  marks the gateway ready once at least one configured provider answers a
  lightweight connectivity check, and not-ready (503) if they all become
  unreachable, so orchestrators pull the pod from rotation. With no models
  configured, readiness is not blocked.
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
