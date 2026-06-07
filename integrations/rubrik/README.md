# Rubrik Agent Rewind

VaultPlane and Rubrik cover complementary halves of the agentic AI lifecycle:
VaultPlane defines and enforces what is allowed on every model call, and Rubrik
Agent Rewind records and selectively rolls back what happened. The Gateway is a
clean feed for Agent Rewind because it already sees every call on the wire.

> Status: this is a stub pending Rubrik's finalized ingest schema. The endpoint
> and auth below are placeholders, and the feed is wired through a standard OTLP
> exporter rather than a bespoke connector. It will be completed once the schema
> lands. Acceptance for MVP allows a functional stub here.

## What the OSS feed provides

The Gateway emits two things Agent Rewind can consume today, both over OTLP:

- **The audit stream** (`logs`, filtered to `vaultplane.audit = true`): every
  policy decision and administrative action, with `action`, `actor`, `subject`,
  `outcome`, and action-specific metadata. This is the "what was allowed or
  blocked, and why" record.
- **Request spans** (`traces`): one span per call carrying the GenAI semantic
  conventions (model, token usage) plus `vaultplane.*` attributes (team, app,
  env, virtual key id, cost, cache hit, provider attempts) and the outcome
  (HTTP status). This is the "who called what, and how it resolved" record.

## What is not in the OSS feed

- **Full prompt and completion bodies.** By default the Gateway never logs prompt
  or completion content (it is opt-in per route at DEBUG, for privacy). A
  complete Agent Rewind feed that can reconstruct and roll back conversations
  needs that body-capture path enabled, which is a deliberate, scoped decision
  rather than a default.
- **The policy-context pull API** (which policies were active for a given call).
  That is a control-plane (Cloud) capability, not part of the open-source data
  plane.

## Wiring the stub

```bash
export RUBRIK_OTLP_ENDPOINT=https://<rubrik-ingest-endpoint>/otlp   # placeholder
export RUBRIK_API_TOKEN=...                                         # placeholder
otelcol-contrib --config integrations/rubrik/collector.yaml
```

Point the gateway at this Collector with
`OTEL_EXPORTER_OTLP_ENDPOINT=http://<collector-host>:4318`. The Collector
forwards request spans and the audit log stream to Rubrik; everything else is
dropped.
