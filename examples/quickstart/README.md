# Quickstart

Gateway against OpenAI, with the exact-match cache turned on. No virtual
keys, no admin token: the proxy and admin ports are open. Fine on
localhost, never deploy this way.

## Run

```bash
export OPENAI_API_KEY=sk-...
docker compose up
```

## Call the gateway

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "smart",
    "messages": [{"role": "user", "content": "say hi in five words"}]
  }'
```

Run the same `curl` a second time and the response comes back from cache
(check the response headers: `x-vaultplane-cache: HIT`).

## What this shows

* The virtual model name `smart` routes through the configured registry
  to OpenAI's `gpt-4o`. Change the `models:` block to point `smart` at a
  different upstream and apps don't have to know.
* The exact-match cache serves identical request bodies from memory.
* The admin port is reachable: `curl http://localhost:9091/admin/status`.

## What's next

For anything beyond localhost, set `VAULTPLANE_ADMIN_TOKEN`, issue virtual
keys with `vaultplane-ctl key create`, and turn on TLS. See the project
[README](../../README.md) for the production shape.
