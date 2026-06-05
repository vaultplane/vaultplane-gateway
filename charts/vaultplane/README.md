# VaultPlane Gateway Helm chart

Deploys the VaultPlane Gateway data plane as a Kubernetes Deployment with
a two-port Service (proxy and admin), a ConfigMap holding the YAML
config, a Secret for provider API keys and the admin token, an optional
ServiceMonitor for kube-prometheus-stack, and an optional
PodDisruptionBudget.

## Install

From the OCI registry on GHCR (published on each `v*` tag):

```bash
helm install vaultplane oci://ghcr.io/vaultplane/charts/vaultplane \
  --version 0.0.1 \
  --create-namespace \
  --namespace vaultplane \
  --set "secret.data.OPENAI_API_KEY=sk-..." \
  --set "secret.data.VAULTPLANE_ADMIN_TOKEN=$(openssl rand -hex 32)"
```

From a local checkout (useful while developing or pinning to a commit):

```bash
helm install vaultplane ./charts/vaultplane \
  --create-namespace \
  --namespace vaultplane \
  --set "secret.data.OPENAI_API_KEY=sk-..." \
  --set "secret.data.VAULTPLANE_ADMIN_TOKEN=$(openssl rand -hex 32)"
```

For anything beyond a trial, drive it from a values file:

```yaml
# values.production.yaml
image:
  tag: "sha-abc1234"

replicaCount: 2

resources:
  requests: { cpu: 200m, memory: 256Mi }
  limits:   { cpu: "2",   memory: 1Gi }

config: |
  listen:
    address: "0.0.0.0:8080"
    admin_address: "0.0.0.0:9091"
  providers:
    openai:    { base_url: "https://api.openai.com", api_key_env: "OPENAI_API_KEY" }
    anthropic: { base_url: "https://api.anthropic.com", api_key_env: "ANTHROPIC_API_KEY" }
  models:
    - name: smart
      primary: { provider: openai, model: gpt-4o }
      fallbacks:
        - { provider: anthropic, model: claude-3-7-sonnet }
      retry_on: [502, 503, 504]
      timeout_ms: 30000
  cache:
    enabled: true
    size_mb: 128
    ttl_seconds: 300

secret:
  create: false
  name: "vaultplane-secrets"      # pre-created out of band

env:
  OTEL_EXPORTER_OTLP_ENDPOINT: "http://otel-collector.observability:4318"
  OTEL_SERVICE_NAME: "vaultplane-gateway"

serviceMonitor:
  enabled: true
  bearerTokenSecret:
    name: "vaultplane-secrets"
    key: VAULTPLANE_ADMIN_TOKEN

podDisruptionBudget:
  enabled: true
  minAvailable: 1
```

```bash
helm install vaultplane ./charts/vaultplane \
  --namespace vaultplane --create-namespace \
  --values values.production.yaml
```

## Values reference

| Key | Default | Purpose |
| --- | --- | --- |
| `image.repository` | `ghcr.io/vaultplane/vaultplane-gateway` | Image repo. |
| `image.tag` | `main` | Image tag. Empty falls back to `.Chart.AppVersion`. |
| `replicaCount` | `1` | Pod count. The gateway is per-replica stateful for rate limits and spend; multi-replica needs sticky routing per virtual key today. |
| `service.proxyPort` | `8080` | OpenAI-compatible API port. |
| `service.adminPort` | `9091` | Status, metrics, key management, reload. |
| `config` | minimal | Full YAML config block, mounted as a ConfigMap. |
| `secret.create` | `true` | If true, the chart manages a Secret. If false, set `secret.name` to an existing one. |
| `secret.data` | `{}` | Map of env vars baked into the managed Secret. Set provider API key vars and `VAULTPLANE_ADMIN_TOKEN`. |
| `env` | `{}` | Extra env vars (OTel endpoint, RUST_LOG, etc.). |
| `serviceMonitor.enabled` | `false` | Render a ServiceMonitor scraping `/admin/metrics`. |
| `podDisruptionBudget.enabled` | `false` | Render a PDB with `minAvailable`. |
| `resources` | unset | Container resource requests and limits. |

Full list: see [`values.yaml`](./values.yaml).

## Production checklist

* `secret.create=false` and reference an external Secret managed by your
  secret-management tool (ExternalSecrets, Vault, sealed-secrets, ...).
* `VAULTPLANE_ADMIN_TOKEN` set in the Secret. Without it, the admin port
  is open and anyone who can reach it can issue keys.
* Pin `image.tag` to a specific commit SHA tag, not `main`.
* TLS: configure `listen.tls.{cert_path, key_path}` in the `config:`
  block and mount the cert/key files (use a second Secret + extra
  volumeMounts; the chart does not abstract this yet).
* Resource requests and limits set per your observed load.
* `podDisruptionBudget.enabled=true` for HA setups.

## Uninstall

```bash
helm uninstall vaultplane --namespace vaultplane
```
