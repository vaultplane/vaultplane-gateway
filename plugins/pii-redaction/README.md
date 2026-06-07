# pii-redaction

Reference inline plugin for VaultPlane Gateway. It redacts common PII patterns (US
Social Security numbers, US-format credit card numbers, US phone numbers, and email
addresses) from the `messages[].content` strings of an OpenAI Chat Completions
request body, replacing each match with `[REDACTED]`.

It is a WebAssembly component implementing the `inspect-request` contract from the
plugin SDK (`crates/vaultplane-plugin-sdk/wit/world.wit`), and proves that contract
end-to-end. The gateway loads it through its wasmtime host; see the `wasm` plugin
type in `CONFIGURATION.md`.

## Building

This crate is intentionally excluded from the workspace build (which targets the
host). Build it for the WebAssembly component target instead:

```bash
rustup target add wasm32-wasip2
cargo build --release --manifest-path plugins/pii-redaction/Cargo.toml --target wasm32-wasip2
```

The component is written to:

```
plugins/pii-redaction/target/wasm32-wasip2/release/pii_redaction.wasm
```

Point a gateway `wasm` plugin entry at that file, or attach it to a release.

## Notes

The pattern set and replacement string are fixed in this reference build. Passing
per-deployment plugin configuration into a component needs an additional WIT entry
point and is left for a follow-up; operators who need configurable patterns today
can use the gateway's built-in native `pii_redaction` plugin.
