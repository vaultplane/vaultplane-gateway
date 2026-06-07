# Security policy

## Supported versions

VaultPlane Gateway is pre-1.0. Security fixes land on `main`; once `v1.0.0`
ships, this section will list the supported minor versions.

| Version | Supported |
| --- | --- |
| `main` (and the latest tagged release) | yes |
| Earlier tagged releases | no |

If you operate the Gateway in production, pin to a tagged release (or a
specific commit SHA tag of the Docker image and Helm chart) and update
when a security release is announced.

## Reporting a vulnerability

**Please do not open public GitHub issues for security problems.**

Report privately through one of:

* GitHub's private vulnerability reporting at
  https://github.com/vaultplane/vaultplane-gateway/security/advisories/new
  (preferred; tracks the fix and the embargo in one place).
* Email `security@vaultplane.com`.

Please include:

* A description of the issue and the impact you believe it has.
* The affected version (commit SHA, tag, or image digest).
* Steps to reproduce, ideally with a minimal config.
* Any proof-of-concept you are comfortable sharing.

We will acknowledge receipt within three business days, share an initial
assessment within seven, and target a fix within thirty for high-severity
issues. We will credit the reporter in the release notes unless you ask
us not to.

## Scope

In scope:

* The `vaultplane` gateway data plane (this repository).
* The `vaultplane-ctl` CLI (this repository).
* The published container images at
  `ghcr.io/vaultplane/vaultplane-gateway` and the Helm chart at
  `oci://ghcr.io/vaultplane/charts/vaultplane`.

Out of scope:

* The managed control plane at `https://vaultplane.com/cloud` (report
  to `security@vaultplane.com`, but please mark the report as Cloud).
* The VaultPlane Registry product.
* Third-party plugins or integrations.
* Issues that require pre-existing access to a credential the gateway
  has been configured to use (provider API keys, the admin token); the
  gateway protects the proxy, not the secrets management around it.

## Threat model (summary)

The gateway sits between trusted applications (which hold virtual keys)
and untrusted upstream model providers. It assumes:

* The admin port (`:9091`) is reachable only from inside the operator's
  perimeter, or is fronted by an authenticated ingress.
* The admin token, when set, is treated as a high-value secret.
* Provider API keys live in environment variables managed by the
  deployment orchestrator (Kubernetes Secrets, Vault, etc.).
* Virtual keys are bearer tokens; the gateway hashes them at rest with
  SHA-256 (chosen explicitly over argon2id to avoid gating every request
  on a ~10ms verify; high-entropy keys do not benefit from a slow KDF).

The gateway does not assume:

* That upstream providers are honest. Provider response bodies are
  forwarded to clients unchanged for OpenAI and Azure, and rewritten
  into the OpenAI error envelope for Anthropic on non-2xx responses.
* That clients are honest. Plugins (PII redaction, future signed Wasm
  plugins) run before the upstream dispatch.
