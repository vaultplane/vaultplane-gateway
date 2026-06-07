# Contributing to VaultPlane Gateway

Thanks for your interest. This document covers the practical bits:
how to build, how to run the tests, what we expect in a PR, and how
the project is structured.

For the architectural contract (what the runtime is supposed to do
and why), see the [README](./README.md) and
[CONFIGURATION.md](./CONFIGURATION.md). For security-sensitive
issues, see [SECURITY.md](./SECURITY.md).

## Quick start for contributors

You will need:

* **Rust stable**, 2024 edition. The repo carries a
  `rust-toolchain.toml` so `rustup` will install the right version
  automatically on first build.
* **Docker** (for the chart-testing and Docker workflows you can run
  locally to mirror CI; not required for everyday development).
* **Helm** (optional, for chart development).

Build and test:

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

CI runs all four; anything red there will block the PR.

Performance work has two extra tools, both run by CI as well:

```bash
# Criterion microbenchmarks for the per-request hot path
# (auth, cache-key derivation, cost accounting).
cargo bench -p vaultplane-core

# End-to-end latency harness: drives the full inbound path against a
# local upstream simulator and prints P50/P99 gateway overhead. It is
# #[ignore]d in the normal run, so invoke it explicitly.
cargo test -p vaultplane --bin vaultplane -- --ignored latency
```

## What we look for in a PR

* **Tests for behavior changes.** Every existing slice landed with
  unit tests, and several with integration tests through the proxy
  router. New behavior follows the same pattern.
* **`cargo fmt --all` clean** and **`cargo clippy --all-targets
  -- -D warnings` clean**. Both run in CI.
* **No new clippy lints suppressed** without a comment explaining
  why. Existing `#[allow(...)]` attributes carry a one-line
  justification; please match the convention.
* **Doc comments on new public items.** The runtime is small; we
  spend the doc budget on the public surface and the load-bearing
  internals (the `Connector` trait, the `Runtime` swap, the plugin
  shape).
* **A short rationale in the PR description.** What problem the
  change solves, what the alternative shapes were, and why this
  one. The commit messages in this repo are intentionally specific
  about the *why*; PR descriptions are usually a short version of
  the eventual commit message.

## Project layout

```
crates/
  vaultplane-core/        shared library: config, auth, cache,
                          provider trait + impls, plugin trait,
                          OTel/cost helpers
  vaultplane/             the gateway data-plane binary
                          (proxy + admin + tls + runtime + prom)
  vaultplane-ctl/         the operator CLI
  vaultplane-plugin-sdk/  WIT contract for the future wasm host

plugins/
  pii-redaction/          reference plugin (today native Rust,
                          eventually a wasm component)

charts/
  vaultplane/             Helm chart for Kubernetes deployment

examples/
  quickstart/             gateway + OpenAI + cache, docker-compose
  observability/          gateway + Jaeger + Prometheus

.github/workflows/
  ci.yml                  fmt + clippy + tests, helm lint + template,
                          chart-testing with kind
  docker.yml              smoke test, multi-arch build, push to GHCR
  chart.yml               package + push chart to GHCR (tags only)
```

## Architectural conventions

A few choices that come up often in reviews; preserve them unless
you have a strong reason not to:

* **Streaming never buffers the full response.** Provider connectors
  return a `BodyStream`; the proxy forwards chunk by chunk. This is
  load-bearing for the N3 memory budget.
* **The connector trait is the seam for providers.** Adding a
  provider means writing one file under
  `crates/vaultplane-core/src/provider/` and registering it in the
  runtime. The rest of the codebase does not change.
* **Hot-swap state lives in the `Runtime` struct.** Anything a
  request handler reads off the wire goes in there, behind the
  `ArcSwap` handle. Anything that holds per-key accumulating state
  (keystore, rate limiter, spend tracker) lives outside the swap.
  See `crates/vaultplane/src/runtime.rs` for the rationale.
* **Plugins decide `Pass | Modify | Reject`.** They run between the
  spend pre-check and the cache lookup. New hooks (response-side
  inspection, streaming inspection) should follow the same
  decision shape.
* **Metric cardinality is bounded by design.** `provider`, `model`,
  `status`, `cache`, and `reason` labels only. Virtual key id,
  team, app, and env go on OTel spans, never on Prometheus labels.

## Style

* **No em dashes in shipped documentation** (README, CHANGELOG,
  CONFIGURATION, doc-comments). Use commas, parentheses, colons,
  or reword.
* **No tabs in YAML or markdown** (Rust handles itself via
  rustfmt).
* **Commit messages**: imperative mood subject line, blank line,
  prose body explaining *why* the change is the way it is. The
  history is the design log; future you will be grateful.

## Reporting bugs or asking questions

* **Bugs**: open a GitHub issue with the gateway version (commit
  SHA or image digest), a minimal config that reproduces, and the
  observed vs expected behavior.
* **Questions about how to use the gateway**: GitHub Discussions
  is the right place. Issues are for things we should fix.
* **Security-sensitive bugs**: do not open a public issue; see
  [SECURITY.md](./SECURITY.md).

## License

By contributing, you agree that your contributions will be licensed
under the [Apache License 2.0](./LICENSE), the same terms as the
rest of the project.

Outside contributions also require a signed Contributor License
Agreement (CLA). When you open your first pull request, the CLA
assistant comments with a link; sign it once and it covers your
future contributions. A PR cannot be merged until the CLA check is
green. Contributions from VaultPlane employees made in the course of
their work are covered by their employment agreement and do not need
a separate signature.
