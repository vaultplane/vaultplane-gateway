// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! WebAssembly plugin host.
//!
//! Loads plugins that are WebAssembly components implementing the `inspect`
//! interface from `crates/vaultplane-plugin-sdk/wit/world.wit`, and runs them on
//! the request path through the same [`Plugin`](super::Plugin) trait the native
//! plugins use.
//!
//! Each plugin declares a hard latency budget. The host arms a per-invocation
//! epoch deadline so a plugin that runs past its budget is trapped rather than
//! allowed to stall the request. On a budget overrun, a trap, or a failure to
//! instantiate, the host applies the plugin's circuit-breaker policy: fail open
//! (forward the request unchanged) or fail closed (reject it).
//!
//! Plugins run in a WASI sandbox with no inherited stdio, no preopened
//! directories, and a bounded linear-memory size. A fresh store is created per
//! invocation, so no state leaks between requests.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

use crate::config::FailMode;

use super::{Decision, Plugin, PluginRequest, RejectInfo};

// Host-side bindings generated from the plugin WIT contract. The path is
// relative to this crate's Cargo.toml.
mod bindings {
    wasmtime::component::bindgen!({
        world: "plugin",
        path: "../vaultplane-plugin-sdk/wit",
    });
}

use bindings::PluginPre;
use bindings::exports::vaultplane::plugin::inspect as guest;

/// The epoch ticker resolution. Latency budgets are rounded up to this.
const TICK: Duration = Duration::from_millis(1);

/// Default upper bound on a plugin's linear memory. Bounds runaway allocation
/// without exposing another config knob; revisit if a plugin needs more.
const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// HTTP status returned when a fail-closed plugin rejects a request because it
/// could not be evaluated.
const FAIL_CLOSED_STATUS: u16 = 403;

/// Timeout for downloading a remote (`http(s)://`) plugin component.
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `source` is an `http(s)://` URL (as opposed to a path or `file://`).
fn is_http_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Download a wasm component over HTTP. The blocking HTTP client runs on a
/// dedicated thread so it neither requires nor conflicts with the async runtime
/// this is called from (startup and config reload).
fn fetch_remote(url: &str) -> anyhow::Result<Vec<u8>> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| -> anyhow::Result<Vec<u8>> {
                let client = reqwest::blocking::Client::builder()
                    .timeout(REMOTE_FETCH_TIMEOUT)
                    .build()?;
                let bytes = client.get(url).send()?.error_for_status()?.bytes()?;
                Ok(bytes.to_vec())
            })
            .join()
            .map_err(|_| anyhow::anyhow!("wasm plugin download thread panicked"))?
    })
}

/// Process-wide wasmtime engine shared by every loaded plugin. A single engine
/// means a single epoch-ticker thread regardless of how many plugins load.
static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Return the shared engine, spawning the epoch ticker on first use.
fn shared_engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("wasmtime engine config is valid");

        // One daemon thread advances the engine epoch on a fixed cadence; per-call
        // deadlines are expressed in ticks of this clock.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("vaultplane-wasm-epoch".to_string())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    ticker.increment_epoch();
                }
            })
            .expect("failed to spawn wasm epoch ticker");

        engine
    })
}

/// Per-store host state: a sandboxed WASI context plus memory limits.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl HostState {
    fn new(memory_bytes: usize) -> Self {
        // No inherited stdio, no env, no preopens: a plugin cannot touch the host.
        let wasi = WasiCtxBuilder::new().build();
        let limits = StoreLimitsBuilder::new().memory_size(memory_bytes).build();
        Self {
            wasi,
            table: ResourceTable::new(),
            limits,
        }
    }
}

impl WasiView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

/// A WebAssembly component plugin loaded and ready to run on the request path.
pub struct WasmPlugin {
    name: String,
    pre: PluginPre<HostState>,
    /// Epoch ticks a single invocation may run before the deadline traps it.
    budget_ticks: u64,
    on_timeout: FailMode,
    memory_bytes: usize,
}

impl WasmPlugin {
    /// Load a plugin component from `source`.
    ///
    /// `source` is a local path, a `file://` URL, or an `http(s)://` URL; remote
    /// components are fetched over HTTP. Validates that the component implements
    /// the `inspect` contract (via the generated pre-instantiation). Returns an
    /// error on a missing/unreachable source, an invalid component, or a contract
    /// mismatch, so the caller can refuse to swap in a broken configuration.
    pub fn load(
        name: impl Into<String>,
        source: impl AsRef<str>,
        latency_budget_ms: u32,
        on_timeout: FailMode,
    ) -> anyhow::Result<Self> {
        let name = name.into();
        let source = source.as_ref();
        let engine = shared_engine();

        let component = if is_http_url(source) {
            let bytes = fetch_remote(source).with_context(|| {
                format!("failed to download wasm plugin '{name}' from {source}")
            })?;
            Component::from_binary(engine, &bytes).with_context(|| {
                format!("wasm plugin '{name}' downloaded from {source} is invalid")
            })?
        } else {
            let path = source.strip_prefix("file://").unwrap_or(source);
            Component::from_file(engine, path)
                .with_context(|| format!("failed to load wasm plugin '{name}' from {path}"))?
        };

        let mut linker = Linker::<HostState>::new(engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .context("failed to add WASI to the plugin linker")?;

        let instance_pre = linker
            .instantiate_pre(&component)
            .with_context(|| format!("wasm plugin '{name}' failed to link"))?;
        let pre = PluginPre::new(instance_pre).with_context(|| {
            format!("wasm plugin '{name}' does not implement the inspect contract")
        })?;

        // One tick is one millisecond; a sub-millisecond budget still gets at
        // least one tick of grace.
        let budget_ticks = u64::from(latency_budget_ms).max(1);

        Ok(Self {
            name,
            pre,
            budget_ticks,
            on_timeout,
            memory_bytes: DEFAULT_MEMORY_BYTES,
        })
    }

    /// Apply the circuit-breaker policy after a plugin failed to produce a
    /// decision, logging the cause.
    fn circuit_break(&self, error: &anyhow::Error) -> Decision {
        let interrupted = error
            .downcast_ref::<wasmtime::Trap>()
            .is_some_and(|t| *t == wasmtime::Trap::Interrupt);

        match self.on_timeout {
            FailMode::FailOpen => {
                tracing::warn!(
                    plugin = %self.name,
                    interrupted,
                    error = %error,
                    "wasm plugin failed; failing open (request forwarded)"
                );
                Decision::Pass
            }
            FailMode::FailClosed => {
                tracing::warn!(
                    plugin = %self.name,
                    interrupted,
                    error = %error,
                    "wasm plugin failed; failing closed (request rejected)"
                );
                Decision::Reject(RejectInfo {
                    reason: format!(
                        "request blocked: plugin '{}' could not evaluate it",
                        self.name
                    ),
                    status_code: FAIL_CLOSED_STATUS,
                })
            }
        }
    }

    /// Run the component once and translate its decision, mapping any failure
    /// through the circuit breaker.
    fn run(&self, request: &PluginRequest) -> Decision {
        let mut store = Store::new(shared_engine(), HostState::new(self.memory_bytes));
        store.limiter(|state| &mut state.limits);
        // Arm the deadline before instantiation so a plugin cannot stall in its
        // own initializer either.
        store.set_epoch_deadline(self.budget_ticks);

        let guest_request = guest::Request {
            method: request.method.clone(),
            path: request.path.clone(),
            headers: request.headers.clone(),
            body: request.body.to_vec(),
        };

        let instance = match self.pre.instantiate(&mut store) {
            Ok(instance) => instance,
            Err(error) => return self.circuit_break(&error),
        };

        match instance
            .vaultplane_plugin_inspect()
            .call_inspect_request(&mut store, &guest_request)
        {
            Ok(action) => map_action(action),
            Err(error) => self.circuit_break(&error),
        }
    }
}

/// Translate a guest decision into the host [`Decision`].
fn map_action(action: guest::Action) -> Decision {
    match action {
        guest::Action::Pass => Decision::Pass,
        guest::Action::Modify(req) => Decision::Modify(PluginRequest {
            method: req.method,
            path: req.path,
            headers: req.headers,
            body: Bytes::from(req.body),
        }),
        guest::Action::Reject(info) => Decision::Reject(RejectInfo {
            reason: info.reason,
            status_code: info.status_code,
        }),
    }
}

impl Plugin for WasmPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn inspect_request(&self, request: &PluginRequest) -> Decision {
        self.run(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reports_a_missing_file() {
        let err = match WasmPlugin::load("missing", "does-not-exist.wasm", 5, FailMode::FailOpen) {
            Ok(_) => panic!("loading a missing component must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("missing"),
            "error should name the plugin: {err}"
        );
    }

    #[test]
    fn load_reports_a_failed_download() {
        // An unreachable URL exercises the http branch and surfaces a download
        // error naming the plugin.
        let err = match WasmPlugin::load(
            "remote",
            "http://127.0.0.1:1/plugin.wasm",
            5,
            FailMode::FailOpen,
        ) {
            Ok(_) => panic!("downloading from an unreachable URL must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("remote"),
            "error should name the plugin: {err}"
        );
    }
}
