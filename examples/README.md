# VaultPlane Gateway examples

Runnable examples that show end-to-end how to operate the Gateway. Each
example is a self-contained directory with its own `docker-compose.yml`
and a short README.

| Example | What it shows |
| --- | --- |
| [quickstart](./quickstart) | Gateway against OpenAI with exact-match caching |
| [observability](./observability) | Gateway + Jaeger + Prometheus on one network |

Both expect `OPENAI_API_KEY` in the environment. Set it once before running
either example:

```bash
export OPENAI_API_KEY=sk-...
cd quickstart && docker compose up
```

These examples use no virtual keys and no admin token, so the proxy and
admin ports are open. That is fine on localhost for trying it out, and is
**not** how you run any of this in production. The project README has the
production setup (admin token, virtual keys with rate and spend limits,
TLS).
