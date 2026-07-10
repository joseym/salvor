//! [`WasmEngine`]: the shared wasmtime machinery behind every [`WasmTool`]
//! (crate::WasmTool): one engine, one WASI linker, one epoch-ticker thread,
//! and a compiled-component cache. Each *call* still gets a fresh `Store` and
//! a fresh instance; only the expensive, stateless pieces are shared.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, Trap};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::Tool;
use crate::error::{LimitExceeded, LimitKind, WasmError};
use crate::grants::DirGrant;
use crate::limits::{EPOCH_TICK_MS, ToolLimits, TrackedLimits};

/// Cap on how much guest stderr one call may accumulate. Stderr is the only
/// stdio a guest gets, it lands in tracing (never the operator's terminal),
/// and a hostile guest must not be able to balloon host memory through it.
const STDERR_CAP_BYTES: usize = 64 * 1024;

/// Everything one call's `Store` carries: the (deny-all plus grants) WASI
/// context, the resource table WASI implementations allocate handles in, and
/// the tracking resource limiter.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: TrackedLimits,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// How a sandboxed call failed, before mapping into the tool layer's error
/// vocabulary. Crate-internal: [`WasmTool`](crate::WasmTool) translates this
/// into [`ToolError`](salvor_tools::ToolError).
pub(crate) enum CallFailure {
    /// The host killed the call at a configured cap.
    Limit(LimitExceeded),
    /// The call could not start: a grant's host directory was missing, or
    /// instantiation failed for a reason that is not a resource cap.
    Setup(String),
    /// The guest crashed on its own (a panic, an `unreachable`, a stack
    /// overflow) with no resource cap involved.
    Trap(String),
}

/// The shared wasmtime host: engine configuration, the WASI p2 linker, the
/// epoch ticker, and a per-path cache of compiled components.
///
/// Construct one per process (or per agent) and share it via `Arc`; every
/// [`WasmTool`](crate::WasmTool) holds a clone. Dropping the last handle
/// stops the ticker thread.
///
/// # The epoch ticker
///
/// Wall-time enforcement is wasmtime's epoch interruption: a monotonically
/// increasing counter on the engine, checked by generated code at loop
/// back-edges and function entries at near-zero cost. One background thread
/// increments it every [`EPOCH_TICK_MS`] milliseconds; each call arms a
/// deadline of `wall_time_ms` worth of ticks. When the counter passes the
/// deadline the guest traps with [`Trap::Interrupt`], whatever it was doing.
pub struct WasmEngine {
    engine: Engine,
    linker: Linker<HostState>,
    /// Compiled components, keyed by canonicalized path. Compilation is the
    /// expensive step (tens of milliseconds for a small Rust guest, ~700 ms
    /// for a componentized-Python one), so it happens once per file per
    /// engine; instantiation stays per-call. The cache assumes a component
    /// file does not change on disk within one engine's lifetime, which for
    /// the CLI is one run.
    components: Mutex<HashMap<PathBuf, Component>>,
    ticker_stop: Arc<AtomicBool>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl WasmEngine {
    /// Builds the engine, its WASI linker, and the epoch ticker thread.
    ///
    /// Fuel accounting and epoch interruption are both switched on at engine
    /// level: epoch interruption backs the always-enforced wall-time cap, and
    /// fuel is armed per call (a call without a fuel budget gets `u64::MAX`,
    /// which never runs out in practice).
    ///
    /// # Errors
    ///
    /// [`WasmError::Engine`] when wasmtime rejects the configuration or the
    /// WASI linker cannot be populated; an OS-level failure to spawn the
    /// ticker thread also folds into [`WasmError::Engine`].
    pub fn new() -> Result<Arc<Self>, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|source| WasmError::Engine { source })?;

        let mut linker: Linker<HostState> = Linker::new(&engine);
        // Satisfies every WASI 0.2 interface a guest's standard library wires
        // in (clocks, stdio, filesystem, random) against whatever WasiCtx the
        // store carries. The imports always exist; the capabilities behind
        // them are only what each call's context grants.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|source| WasmError::Engine { source })?;

        let ticker_stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let stop = Arc::clone(&ticker_stop);
            std::thread::Builder::new()
                .name("salvor-wasm-epoch".to_owned())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                        engine.increment_epoch();
                    }
                })
                .map_err(|source| WasmError::Engine {
                    source: wasmtime::Error::new(source),
                })?
        };

        Ok(Arc::new(Self {
            engine,
            linker,
            components: Mutex::new(HashMap::new()),
            ticker_stop,
            ticker: Some(ticker),
        }))
    }

    /// Reads, integrity-checks, and compiles a component, caching the
    /// compilation per path.
    ///
    /// The sha256 pin, when present, is verified against the freshly read
    /// bytes on **every** load, before compilation and long before
    /// instantiation, so a tampered file is refused while it is still inert
    /// bytes. Hex comparison is case-insensitive.
    ///
    /// # Errors
    ///
    /// [`WasmError::Io`] when the file cannot be read,
    /// [`WasmError::Sha256Mismatch`] when a pin does not match, and
    /// [`WasmError::Compile`] when the bytes are not a valid component.
    pub fn load_component(
        &self,
        path: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<Component, WasmError> {
        let bytes = std::fs::read(path).map_err(|source| WasmError::Io {
            path: path.to_owned(),
            source,
        })?;

        if let Some(expected) = expected_sha256 {
            let actual = hex_digest(&bytes);
            let expected = expected.trim().to_ascii_lowercase();
            if actual != expected {
                return Err(WasmError::Sha256Mismatch {
                    path: path.to_owned(),
                    expected,
                    actual,
                });
            }
        }

        let key = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let mut cache = self.components.lock().expect("component cache poisoned");
        if let Some(component) = cache.get(&key) {
            return Ok(component.clone());
        }
        let component =
            Component::new(&self.engine, &bytes).map_err(|source| WasmError::Compile {
                path: path.to_owned(),
                source,
            })?;
        cache.insert(key, component.clone());
        Ok(component)
    }

    /// Runs one call: fresh WASI context, fresh store, fresh instance, one
    /// invocation of the guest's `call` export.
    ///
    /// Synchronous and CPU-bound by design; the caller wraps it in
    /// `spawn_blocking`. The outer `Result` is the host's verdict (limits,
    /// setup, crashes); the inner `Result<String, String>` is the guest's own
    /// answer over the WIT boundary.
    pub(crate) fn execute(
        &self,
        component: &Component,
        limits: ToolLimits,
        grants: &[DirGrant],
        input: &str,
    ) -> Result<Result<String, String>, CallFailure> {
        // Deny-all WASI: no env, no args, no filesystem, no sockets, no
        // clock *grants* beyond what wasi:clocks exposes read-only. Stderr
        // goes to a capped in-memory pipe that lands in tracing below, so a
        // hostile guest cannot write to the operator's terminal.
        let stderr = MemoryOutputPipe::new(STDERR_CAP_BYTES);
        let mut wasi = WasiCtxBuilder::new();
        wasi.stderr(stderr.clone());
        for grant in grants {
            let (dir_perms, file_perms) = grant.perms.wasi_perms();
            wasi.preopened_dir(&grant.host, &grant.guest, dir_perms, file_perms)
                .map_err(|err| {
                    CallFailure::Setup(format!(
                        "preopening host directory `{}` at guest path `{}`: {err}",
                        grant.host.display(),
                        grant.guest
                    ))
                })?;
        }

        let host = HostState {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits: TrackedLimits::new(limits.memory_bytes),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store
            .set_fuel(limits.fuel.unwrap_or(u64::MAX))
            .map_err(|err| CallFailure::Setup(format!("arming fuel meter: {err}")))?;
        store.set_epoch_deadline(limits.epoch_deadline_ticks());

        let outcome = Tool::instantiate(&mut store, component, &self.linker)
            .and_then(|tool| tool.call_call(&mut store, input));

        // Whatever happened, surface the guest's stderr through tracing so a
        // failing tool is debuggable without ever inheriting the host's
        // stderr into the sandbox.
        let stderr_bytes = stderr.contents();
        if !stderr_bytes.is_empty() {
            tracing::debug!(
                target: "salvor_wasm::guest",
                stderr = %String::from_utf8_lossy(&stderr_bytes),
                "guest wrote to stderr"
            );
        }

        outcome.map_err(|err| attribute_failure(&err, &store, limits))
    }
}

impl Drop for WasmEngine {
    fn drop(&mut self) {
        self.ticker_stop.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            // The ticker sleeps at most one tick, so this join is bounded.
            let _ = ticker.join();
        }
    }
}

/// Decides what a failed call actually died of, in precedence order:
///
/// 1. A downcast [`Trap::Interrupt`] is the epoch deadline: the wall-time cap.
/// 2. [`Trap::OutOfFuel`] is the fuel cap.
/// 3. Any other failure where the limiter recorded a denied memory (or
///    table) grow is the memory cap. This ordering matters: a denied grow
///    usually surfaces as an opaque `unreachable` trap from the guest's
///    aborting allocator, so the recorded denial is the only honest witness.
///    But a guest that *handles* allocation failure can trip the limiter and
///    still die later of the deadline, which is why the trap downcasts win.
/// 4. Everything else is the guest crashing on its own, or instantiation
///    failing.
fn attribute_failure(
    err: &wasmtime::Error,
    store: &Store<HostState>,
    limits: ToolLimits,
) -> CallFailure {
    match err.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => CallFailure::Limit(LimitExceeded {
            kind: LimitKind::WallTime,
            limit: limits.wall_time_ms,
        }),
        Some(Trap::OutOfFuel) => CallFailure::Limit(LimitExceeded {
            kind: LimitKind::Fuel,
            limit: limits.fuel.unwrap_or(u64::MAX),
        }),
        _ if store.data().limits.memory_denied() || store.data().limits.table_denied() => {
            CallFailure::Limit(LimitExceeded {
                kind: LimitKind::Memory,
                limit: limits.memory_bytes,
            })
        }
        Some(trap) => CallFailure::Trap(format!("guest trapped: {trap}")),
        None => CallFailure::Setup(format!("instantiating or calling the component: {err:#}")),
    }
}

/// Lowercase hex of the sha256 of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digest_matches_known_vector() {
        // sha256("abc"), the FIPS 180-2 test vector.
        assert_eq!(
            hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn missing_component_file_is_io_not_panic() {
        let engine = WasmEngine::new().expect("engine builds");
        // `Component` has no Debug impl, so `expect_err` is unavailable; a
        // match asserts the same thing.
        match engine.load_component(Path::new("/nonexistent/tool.wasm"), None) {
            Err(WasmError::Io { .. }) => {}
            Err(other) => panic!("expected Io error, got {other}"),
            Ok(_) => panic!("missing file must fail"),
        }
    }

    #[test]
    fn sha_mismatch_refuses_before_compilation() {
        // The file exists but is not even wasm; the pin check must reject it
        // before compilation ever sees the bytes, so the error is a mismatch,
        // not a compile failure.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-wasm.wasm");
        std::fs::write(&path, b"not wasm at all").expect("write");
        let engine = WasmEngine::new().expect("engine builds");
        match engine.load_component(&path, Some("00".repeat(32).as_str())) {
            Err(WasmError::Sha256Mismatch { .. }) => {}
            Err(other) => panic!("expected Sha256Mismatch, got {other}"),
            Ok(_) => panic!("bad pin must fail"),
        }
    }
}
