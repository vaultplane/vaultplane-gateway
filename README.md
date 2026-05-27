# VaultPlane Gateway

**Every model call, on policy.**

VaultPlane Gateway is an open-source, low-latency proxy for AI model and tool
traffic. It gives platform and security teams one path to route, govern, and
observe every call their AI systems make, across commercial and self-hosted
models.

It is the enforcement plane of [VaultPlane](https://vaultplane.com), the control
plane for AI infrastructure. The VaultPlane Registry defines what is trusted; the
Gateway enforces it on the wire.

> ⚠️ **Early development.** This repository is a scaffold. The proxy runtime is
> being built in the open and is **not yet ready for production use**. Star or
> watch the repo to follow progress.

## Why

Enterprises are putting AI into production faster than they can govern it. The
model traffic an application generates, across providers and with real cost and
real data, multiplies without a single place to control it. A registry can
advise but cannot stop a call; a raw proxy enforces guesses with no source of
truth. The Gateway closes that loop.

## Roadmap

Planned for the first release:

- Unified, provider-agnostic API with streaming pass-through
- Provider failover and routing
- Token and cost accounting, per app and per team
- Exact-match caching (semantic caching in the managed control plane)
- OpenTelemetry-native traces and metrics on every call
- Virtual keys and provider credential configuration
- Wasm and gRPC sidecar plugin SDK
- Self-host via Docker, Helm, or a single binary

Install instructions and benchmarks will land here as the runtime stabilizes.
Performance numbers will always be published with context.

## Build from source

VaultPlane Gateway is written in Rust. Build and run the data plane with:

```bash
cargo run -p vaultplane
```

This starts the proxy listener (default port 8080) and the admin listener
(default port 9091). The admin API serves liveness, readiness, and status at
`/admin/healthz`, `/admin/readyz`, and `/admin/status`.

`POST /v1/chat/completions` accepts the OpenAI Chat Completions schema and routes
by model: `claude` models go to Anthropic, everything else to OpenAI. Set
`OPENAI_API_KEY` and `ANTHROPIC_API_KEY` as needed. OpenAI responses stream
through unchanged (streaming or non-streaming); Anthropic requests and
non-streaming responses are translated to and from the OpenAI schema (Anthropic
streaming is not yet supported). `/v1/embeddings` and `/v1/models` return 501 for
now.

Authentication: set `VAULTPLANE_ADMIN_TOKEN` to protect `/admin/status` (sent as
`Authorization: Bearer <token>`); health and readiness stay open for probes.
Configure virtual keys in the config file to require `Authorization: Bearer vp_<token>`
on the proxy and to scope each key to specific models. With no keys configured the
proxy is open, which is convenient for local development.

Configuration is layered: defaults, then an optional YAML file passed with
`--config`, then environment variables prefixed `VAULTPLANE_`.

## VaultPlane: two planes

VaultPlane is the control plane for AI infrastructure, split into two planes:

- **VaultPlane Registry**: the definition plane. Discover, evaluate, certify,
  and govern MCP servers and AI skills. Browse it at https://vaultplane.com.
- **VaultPlane Gateway** (this repository): the enforcement plane. Routes and
  governs model and tool traffic on every call.

Policy defined in the Registry is enforced by the Gateway: define trust once,
enforce it everywhere.

## License

[Apache License 2.0](LICENSE).

---

Looking for a managed control plane? See https://vaultplane.com/cloud.
