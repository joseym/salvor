//! Sandboxed WebAssembly tools: run an untrusted binary as a Salvor tool, with
//! every capability and every model-facing word about it declared by the
//! operator, never by the binary.
//!
//! A tool here is a WASI preview 2 **component** implementing the versioned
//! WIT world `salvor:tool@0.1.0` (in this crate's `wit/` directory): one
//! export, `call: func(input: string) -> result<string, string>`, with JSON
//! payloads on both sides. That boundary is deliberately identical to
//! [`DynTool::call_json`](salvor_tools::DynTool) (JSON in, JSON out), so a
//! [`WasmTool`] slots behind the existing dispatch seam beside native and MCP
//! tools with no new type machinery. Guests are polyglot: Rust targets
//! `wasm32-wasip2` with `wit-bindgen`, Python goes through `componentize-py`,
//! JavaScript through `jco componentize`; the same host runs all three
//! unmodified.
//!
//! # The guarantee, stated precisely
//!
//! A guest gets **no capability the operator did not grant**. The WASI context
//! is empty by default: no filesystem, no environment, no arguments, no
//! sockets, no stdio except stderr captured into tracing. The only v0.2 grant
//! is directory preopens ([`DirGrant`]). On top of that sit hard per-call
//! caps: a wall-time deadline (epoch interruption), a memory byte cap
//! (enforced by a [`ResourceLimiter`](wasmtime::ResourceLimiter) that records
//! denials so the error can name the cap), and optional deterministic fuel
//! metering. Each call runs in a fresh `Store` with a fresh instance, so no
//! state leaks between calls.
//!
//! This is a strong isolation boundary, not a hypervisor. Wasmtime has had
//! CVEs; the workspace pins its version and bumps it deliberately and
//! routinely. Do not describe untrusted code run here as "safe"; describe it
//! as capability-confined and hard-capped.
//!
//! # The operator declares everything
//!
//! The guest contract has no channel for self-description, on purpose. Name,
//! description, input schema, and [`Effect`](salvor_core::Effect) all come from
//! operator configuration ([`WasmToolSpec`]); the effect is *required* there,
//! with no default. This is one notch stricter than MCP, where servers
//! legitimately self-describe and unannotated tools default to `Write`: a
//! sandboxed binary gets no voice, so a missing effect is a missing operator
//! decision, not an input to guess from. A hostile description is a
//! prompt-injection surface, and rendering config must never require
//! instantiating untrusted code.
//!
//! # Deliberately out of scope for v0.2 (the fence)
//!
//! Each of these is a v0.3-or-later conversation with its own risk, not a
//! backlog item to sneak in:
//!
//! - **Network access of any kind.** No `wasi:http`, no sockets. Outbound
//!   HTTP grants a socket-shaped capability whose blast radius (SSRF against
//!   the operator's network) deserves its own design pass, likely as a
//!   host-mediated fetch with an allowlist. Tools that need the network use
//!   MCP today.
//! - **Guest self-description.** A later world version can add a describe
//!   export; v0.2 asks the guest nothing.
//! - **Rich WIT types.** The world is versioned precisely so records
//!   mirroring the tool schema can arrive without breaking v0.2 guests.
//! - **Tool-initiated suspension.** A [`WasmTool`] never returns
//!   [`ToolOutcome::Suspend`](salvor_tools::ToolOutcome::Suspend);
//!   human-in-the-loop lives on native tools.
//! - **OCI / registry distribution.** A component is a file path plus an
//!   optional sha256 pin.
//! - **Streaming output.** One call, one JSON string back.
//!
//! # Blocking posture
//!
//! Wasmtime is driven synchronously here; [`WasmTool`]'s async
//! [`call_json`](salvor_tools::DynTool::call_json) wraps the call in
//! [`tokio::task::spawn_blocking`]. The per-call epoch deadline already
//! bounds how long that blocking call can live, which is what makes the sync
//! posture sound without adopting wasmtime's own async support.

mod engine;
mod error;
mod grants;
mod limits;
mod tool;

/// Host-side bindings generated from `wit/tool.wit` by
/// [`wasmtime::component::bindgen!`]. The macro reads the WIT world at compile
/// time and emits a typed `Tool` wrapper whose `instantiate` links a component
/// against a `Linker` and whose `call_call` invokes the guest's one export
/// with Rust `String`s, translating the component-model canonical ABI (string
/// lifting/lowering, the `result` discriminant) so no hand-written pointer
/// arithmetic exists anywhere in this crate.
mod bindings {
    wasmtime::component::bindgen!({
        world: "tool",
        path: "wit",
    });
}

pub use engine::WasmEngine;
pub use error::{LimitExceeded, LimitKind, WasmError};
pub use grants::{DirGrant, GrantPerms};
pub use limits::{DEFAULT_MEMORY_BYTES, DEFAULT_WALL_TIME_MS, ToolLimits};
pub use tool::{WasmTool, WasmToolSpec};
