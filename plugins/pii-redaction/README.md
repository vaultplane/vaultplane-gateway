# pii-redaction

Reference inline plugin for VaultPlane Gateway. It redacts common PII patterns (US
Social Security numbers, US-format credit card numbers, US phone numbers, and email
addresses) from inbound requests.

This crate is a WebAssembly component and is intentionally excluded from the
workspace build, which targets the host. It is built for a wasm target instead:

```bash
cargo build --release -p pii-redaction --target wasm32-wasip2
```

The redaction logic is not yet implemented.
