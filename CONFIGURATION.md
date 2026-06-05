# Configuration reference

This document enumerates every field in the gateway's YAML configuration
with its type, default value, hot-reload behavior, and a short note. For
the high-level shape, see the [README](./README.md). For runnable
examples, see [`examples/`](./examples).

## Layered loading

Configuration is layered. Later layers override earlier ones:

1. **Built-in defaults.** What you get if you start with no `--config` and
   no environment variables.
2. **YAML file** passed with `--config <path>` (or the `VAULTPLANE_CONFIG`
   env var). Any field you set overrides the default.
3. **Environment variables** prefixed `VAULTPLANE_`, with `__` separating
   nested keys. So `listen.address` is overridden by
   `VAULTPLANE_LISTEN__ADDRESS`; `providers.openai.base_url` by
   `VAULTPLANE_PROVIDERS__OPENAI__BASE_URL`.

Validate a config file with:

```bash
vaultplane-ctl config validate vaultplane.yaml
```

Preview a change before reloading with:

```bash
vaultplane-ctl config diff vaultplane.yaml vaultplane.new.yaml
```

## Hot-reload behavior

Fields are marked **hot-reload: yes** if a `SIGHUP` (or `POST
/admin/config/reload`) picks up the change without restarting the
process. **No** means the field is read once at startup and a restart is
required.

The rule of thumb: anything that lives inside the swappable `Runtime`
bundle (provider connectors, model registry, pricing, cache, plugins) is
hot-reloadable. Listener bindings, the admin token, and the file-loaded
virtual key list are read once. The TLS cert and key paths are a special
case: the paths themselves are bound at startup, but the cert and key
bytes those paths point at can change between reloads and the rustls
config rotates in place.

## `listen`

Proxy and admin listener configuration.

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `listen.address` | string | `"0.0.0.0:8080"` | no | Address the OpenAI-compatible proxy API binds to. |
| `listen.admin_address` | string | `"0.0.0.0:9091"` | no | Address the admin API binds to. |
| `listen.tls` | object \| null | null | mixed | TLS material for the proxy listener (see below). Adding or removing the `tls` block requires a restart; the cert and key files it points at are hot-rotated. |

### `listen.tls`

When set, the proxy listener serves HTTPS via rustls. The admin listener
stays plain HTTP.

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `listen.tls.cert_path` | string | required | yes (rotates in place) | Path to a PEM-encoded certificate chain (server cert first, then any intermediates). |
| `listen.tls.key_path` | string | required | yes (rotates in place) | Path to the PEM-encoded private key for the certificate. |

## `providers`

Per-provider connection settings. The keys, base URLs, and API versions
are static across reloads of `providers.*` (a reload rebuilds the
connectors); the actual API key values are read from the named
environment variables, so rotating a provider key is an environment
update plus a process or pod restart.

### `providers.openai`

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `providers.openai.base_url` | string | `"https://api.openai.com"` | yes | Base URL of the OpenAI API or any OpenAI-compatible server. |
| `providers.openai.api_key_env` | string | `"OPENAI_API_KEY"` | yes | Name of the env var that holds the API key. |

### `providers.anthropic`

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `providers.anthropic.base_url` | string | `"https://api.anthropic.com"` | yes | Base URL of the Anthropic Messages API. |
| `providers.anthropic.api_key_env` | string | `"ANTHROPIC_API_KEY"` | yes | Name of the env var that holds the API key. |

### `providers.azure`

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `providers.azure.base_url` | string | `""` | yes | Resource base URL, for example `https://my-resource.openai.azure.com`. Empty means Azure routing is unconfigured. |
| `providers.azure.api_key_env` | string | `"AZURE_OPENAI_API_KEY"` | yes | Name of the env var that holds the API key. |
| `providers.azure.api_version` | string | `"2024-10-21"` | yes | Azure OpenAI API version, sent as a query parameter on every request. |

### `providers.bedrock`

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `providers.bedrock.region` | string | `"us-east-1"` | yes | AWS region. |
| `providers.bedrock.access_key_env` | string | `"AWS_ACCESS_KEY_ID"` | yes | Env var holding the AWS access key id. |
| `providers.bedrock.secret_key_env` | string | `"AWS_SECRET_ACCESS_KEY"` | yes | Env var holding the AWS secret access key. |
| `providers.bedrock.session_token_env` | string | `"AWS_SESSION_TOKEN"` | yes | Env var holding the AWS session token. Optional for permanent credentials. |

## `auth`

Authentication for the proxy and the admin API.

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `auth.admin_token_env` | string | `"VAULTPLANE_ADMIN_TOKEN"` | no | Env var the admin API reads its bearer token from. Unset / empty means the privileged admin endpoints are open. |
| `auth.keys` | array of virtual keys | `[]` | no | Bootstrap virtual keys loaded from the config file. The admin API (`POST /admin/keys`) is the runtime channel; in production, leave this empty and issue keys through the API. |

### Each entry in `auth.keys`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `id` | string | required | Non-secret identifier (`vp_...`) for logs and admin operations. |
| `hash` | string | required | SHA-256 hex digest of the plaintext token. Generate with `vaultplane-ctl key create` (offline mode). |
| `team` | string | `""` | Team the key is attributed to. Surfaces on OTel spans. |
| `app` | string | `""` | Application the key is attributed to. |
| `env` | string | `""` | Environment (for example `prod`). |
| `models` | array of strings | `[]` | Allowed model names. Empty or containing `*` allows any model. |
| `rate_limit_rps` | integer or null | null | Per-second rate limit. Null means no limit. |
| `spend_limit` | object or null | null | Per-period USD spend limit, see below. |
| `expires_at` | RFC3339 string or null | null | Reject the key after this instant. |

`spend_limit` has shape:

| Field | Type | Notes |
| --- | --- | --- |
| `amount_usd` | float | Maximum spend per period in dollars. |
| `period` | `"day"` \| `"week"` \| `"month"` | Period the budget resets over. |

## `models`

The virtual model registry. Each entry maps a virtual name to a primary
provider route plus ordered fallbacks. A model that is not in the registry
routes by name prefix (`claude*` to Anthropic, everything else to OpenAI).

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `name` | string | required | The virtual model name clients send in the `model` field. |
| `primary` | route | required | The primary provider route, see below. |
| `fallbacks` | array of routes | `[]` | Ordered fallback routes, tried in turn when the primary fails. |
| `retry_on` | array of integers | `[429, 500, 502, 503, 504]` | HTTP status codes that trigger failover to the next route. |
| `timeout_ms` | integer | `30000` | Per-attempt timeout in milliseconds. |

A route is:

| Field | Type | Notes |
| --- | --- | --- |
| `provider` | string | One of `openai`, `anthropic`, `azure`, `bedrock`. |
| `model` | string | The upstream model name to send to that provider. |

Hot-reload: yes. A reload rebuilds the registry from scratch.

## `pricing`

USD pricing per 1,000 input and output tokens, keyed by provider then
model. Empty means cost is not reported (the OTel `vaultplane.cost_usd`
attribute and the `vaultplane_cost_cents_total` metric stay at zero).

```yaml
pricing:
  providers:
    openai:
      gpt-4o:
        input_per_1k_tokens_usd: 0.0025
        output_per_1k_tokens_usd: 0.01
    anthropic:
      claude-3-7-sonnet:
        input_per_1k_tokens_usd: 0.003
        output_per_1k_tokens_usd: 0.015
```

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `pricing.providers.<provider>.<model>.input_per_1k_tokens_usd` | float | 0 | Cost per 1,000 input (prompt) tokens. |
| `pricing.providers.<provider>.<model>.output_per_1k_tokens_usd` | float | 0 | Cost per 1,000 output (completion) tokens. |

Hot-reload: yes.

## `cache`

The exact-match response cache. Deterministic responses (same scope, same
model, identical request body) are served from memory.

| Field | Type | Default | Hot-reload | Notes |
| --- | --- | --- | --- | --- |
| `cache.enabled` | bool | `true` | yes | Whether the cache is active. |
| `cache.size_mb` | integer | `256` | yes | Maximum cache size in megabytes (byte-weighted by response body length). |
| `cache.ttl_seconds` | integer | `3600` | yes | Time-to-live for cached entries, in seconds. |

A reload that changes any field rebuilds the cache; in-flight cached
entries are discarded.

## `plugins`

Inline request-inspection plugins. The array order is the chain order.
Today the gateway ships one built-in plugin (`pii_redaction`); the
trait shape mirrors the WIT contract a future WebAssembly host slots into.

### `pii_redaction`

```yaml
plugins:
  - type: pii_redaction
    patterns: [ssn, credit_card, phone_us, email]
    replacement: "[REDACTED]"
```

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `type` | `"pii_redaction"` | required | Plugin identifier. |
| `patterns` | array | all four | Which patterns to redact. Valid values: `ssn`, `credit_card`, `phone_us`, `email`. |
| `replacement` | string | `"[REDACTED]"` | String substituted in place of each match. |

Hot-reload: yes. A reload rebuilds the plugin chain.

## Environment variable convention

Every field above can be overridden by an environment variable using the
`VAULTPLANE_` prefix with `__` separating nested keys. A few examples:

| YAML path | Env var |
| --- | --- |
| `listen.address` | `VAULTPLANE_LISTEN__ADDRESS` |
| `listen.admin_address` | `VAULTPLANE_LISTEN__ADMIN_ADDRESS` |
| `providers.openai.base_url` | `VAULTPLANE_PROVIDERS__OPENAI__BASE_URL` |
| `providers.azure.api_version` | `VAULTPLANE_PROVIDERS__AZURE__API_VERSION` |
| `cache.enabled` | `VAULTPLANE_CACHE__ENABLED` |
| `cache.ttl_seconds` | `VAULTPLANE_CACHE__TTL_SECONDS` |

Lists (`models`, `auth.keys`, `plugins`) are awkward to express in env
vars; configure them in the YAML file.
