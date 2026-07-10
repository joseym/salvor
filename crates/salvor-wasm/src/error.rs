//! The two error surfaces of the sandbox: [`WasmError`] for load-time refusals
//! (before any untrusted code runs) and [`LimitExceeded`] for a call the host
//! killed at a resource cap (distinct from a failure the guest itself
//! returned).

use std::path::PathBuf;

use thiserror::Error;

/// A load-time failure: the component never reached instantiation.
///
/// Everything here happens before any untrusted code executes, which is the
/// point of the ordering: a bad hash or an unreadable file is refused while
/// the binary is still inert bytes.
#[derive(Debug, Error)]
pub enum WasmError {
    /// The component file could not be read.
    #[error("reading wasm component `{path}`: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The file's bytes do not hash to the pinned sha256. The component is
    /// refused before compilation, let alone instantiation: an integrity pin
    /// that fails means the operator is not looking at the binary they
    /// approved.
    #[error(
        "wasm component `{path}` failed its sha256 pin: expected {expected}, file hashes to {actual}; refusing to load it"
    )]
    Sha256Mismatch {
        /// The component that failed the check.
        path: PathBuf,
        /// The hash the configuration pinned.
        expected: String,
        /// The hash the file's bytes actually produce.
        actual: String,
    },
    /// The bytes are not a valid WebAssembly component (or failed to
    /// compile).
    #[error("compiling wasm component `{path}`: {source}")]
    Compile {
        /// The component that failed to compile.
        path: PathBuf,
        /// Wasmtime's compilation error.
        #[source]
        source: wasmtime::Error,
    },
    /// The wasmtime engine or its WASI linker could not be constructed.
    #[error("initializing the wasm engine: {source}")]
    Engine {
        /// Wasmtime's construction error.
        #[source]
        source: wasmtime::Error,
    },
}

/// Which per-call resource cap a killed call ran into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    /// The epoch deadline fired: the call exceeded its wall-time budget.
    WallTime,
    /// A `memory.grow` past the byte cap was denied. Most guests' allocators
    /// abort on a failed grow, which surfaces as an opaque `unreachable`
    /// trap; the tracking [`ResourceLimiter`](wasmtime::ResourceLimiter)
    /// records the denial so this error can name the cap instead.
    Memory,
    /// The optional deterministic fuel meter ran out.
    Fuel,
}

/// A call the **host** terminated at a configured resource cap.
///
/// This type exists to keep limit exhaustion distinguishable from a failure
/// the guest returned (the `err` side of the WIT `result`): the guest said
/// "this input is bad", the host said "you consumed too much". Both surface
/// through [`ToolError::Handler`](salvor_tools::ToolError::Handler), but a
/// limit trap carries this typed value in the error's source chain, so a
/// caller can walk `source()` and `downcast_ref::<LimitExceeded>()` to route
/// on it.
///
/// # Why limit exhaustion should not be retried, and what v0.2 does about it
///
/// A limit trap is deterministic: the same input under the same caps traps
/// again, so every retry burns the full wall-time budget to reproduce the
/// same failure. [`RetryPolicy`](salvor_tools::RetryPolicy) cannot see that:
/// it classifies by the *tool's* effect, not by the failure, so a `Read`
/// tool's limit trap is retried like any handler failure (bounded by the
/// loop's attempt cap). Expressing "this particular failure is not
/// retryable" needs a per-error channel in the `salvor-tools` seam, which is a
/// v0.3 conversation; smuggling limit traps through a non-`Handler`
/// [`ToolError`](salvor_tools::ToolError) variant today would record a false
/// failure kind in the durable event log, and honest history outranks saved
/// retries. Until the seam grows that channel, this typed error is the
/// routing hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitExceeded {
    /// Which cap was hit.
    pub kind: LimitKind,
    /// The configured value of that cap, in the cap's own unit (milliseconds
    /// for wall time, bytes for memory, fuel units for fuel).
    pub limit: u64,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            LimitKind::WallTime => write!(
                f,
                "wall-time limit of {} ms exceeded (epoch deadline reached)",
                self.limit
            ),
            LimitKind::Memory => write!(
                f,
                "memory cap of {} bytes exceeded (a memory grow past the cap was denied)",
                self.limit
            ),
            LimitKind::Fuel => write!(f, "fuel limit of {} exhausted", self.limit),
        }
    }
}

// Implemented by hand (not through thiserror, which would want an `#[error]`
// format string) because Display already varies by kind above; the trait impl
// itself needs nothing more. Being a `std::error::Error` is what lets this
// type ride a `HandlerError`'s source chain and be found by `downcast_ref`.
impl std::error::Error for LimitExceeded {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_messages_name_the_cap() {
        let wall = LimitExceeded {
            kind: LimitKind::WallTime,
            limit: 5_000,
        };
        assert_eq!(
            wall.to_string(),
            "wall-time limit of 5000 ms exceeded (epoch deadline reached)"
        );
        let memory = LimitExceeded {
            kind: LimitKind::Memory,
            limit: 33_554_432,
        };
        assert!(memory.to_string().contains("33554432 bytes"));
        let fuel = LimitExceeded {
            kind: LimitKind::Fuel,
            limit: 1_000,
        };
        assert_eq!(fuel.to_string(), "fuel limit of 1000 exhausted");
    }

    #[test]
    fn sha_mismatch_names_both_hashes() {
        let err = WasmError::Sha256Mismatch {
            path: PathBuf::from("tool.wasm"),
            expected: "aaaa".into(),
            actual: "bbbb".into(),
        };
        let text = err.to_string();
        assert!(text.contains("expected aaaa"));
        assert!(text.contains("hashes to bbbb"));
        assert!(text.contains("refusing"));
    }
}
