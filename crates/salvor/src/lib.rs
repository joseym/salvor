//! Salvor is a durable execution runtime for AI agents, written in Rust.
//!
//! A salvor is the party that recovers a wrecked ship and its cargo. This runtime does the same for
//! a crashed agent run: every run is an append-only event log, so a process killed mid-step resumes
//! exactly where it stopped, with no repeated side effects.
//!
//! This crate is a facade. It holds no logic of its own; it re-exports the `salvor-*` family so one
//! dependency covers the library. Everything here is equally reachable by depending on the
//! individual crates, which is the better choice when you want a narrow build.
//!
//! # What the default features give you
//!
//! The agent loop ([`runtime`]), the tool contract ([`tools`]), the SQLite event store ([`store`]),
//! and the event model and replay engine ([`core`]). That is the set needed to define an agent, run
//! it, kill it, and resume it.
//!
//! # What is optional
//!
//! Each of these is off by default, because each pulls a dependency tree worth opting into
//! deliberately:
//!
//! - `graph`: the declarative graph document format and its validator.
//! - `engine`: the walker that executes a graph document. Implies `graph`.
//! - `server`: the HTTP and server-sent-events control plane.
//! - `llm`: the Messages API client, for talking to a model directly.
//! - `wasm`: sandboxed WebAssembly component tools. Heavy, since it builds wasmtime.
//! - `mcp`: Model Context Protocol tools, forwarded to `salvor-tools`.
//!
//! # Versioning
//!
//! Every re-exported crate is pinned to this crate's exact version, so upgrading `salvor` moves the
//! whole family together and cannot mix versions that were never tested against each other.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// The event model, replay cursor, and state fold: the durability engine's core.
///
/// Re-export of `salvor-core`. Pure and IO-free, which is also why it compiles to wasm for a
/// browser that wants to fold a log itself.
pub mod core {
    pub use salvor_core::*;
}

/// The agent loop, budgets, and the deterministic run context.
///
/// Re-export of `salvor-runtime`. This is the IO edge: the part that calls a model, records what it
/// did, and enforces the ceilings. Requires the `runtime` feature, on by default.
#[cfg(feature = "runtime")]
pub mod runtime {
    pub use salvor_runtime::*;
}

/// The tool contract: effect classification, typed handlers, and the `#[derive(Tool)]` macro.
///
/// Re-export of `salvor-tools`. Requires the `tools` feature, on by default.
#[cfg(feature = "tools")]
pub mod tools {
    pub use salvor_tools::*;
}

/// The event store trait and its SQLite implementation.
///
/// Re-export of `salvor-store`. Requires the `store` feature, on by default.
#[cfg(feature = "store")]
pub mod store {
    pub use salvor_store::*;
}

/// The declarative graph document format and its validator.
///
/// Re-export of `salvor-graph`. Requires the `graph` feature.
#[cfg(feature = "graph")]
pub mod graph {
    pub use salvor_graph::*;
}

/// The walker that executes a graph document: linear chains, gates, branches, maps, and forks.
///
/// Re-export of `salvor-engine`. Requires the `engine` feature.
#[cfg(feature = "engine")]
pub mod engine {
    pub use salvor_engine::*;
}

/// The HTTP and server-sent-events control plane over the durable runtime.
///
/// Re-export of `salvor-server`. Requires the `server` feature.
#[cfg(feature = "server")]
pub mod server {
    pub use salvor_server::*;
}

/// The Messages API client, for hosted and local model endpoints.
///
/// Re-export of `salvor-llm`. Requires the `llm` feature.
#[cfg(feature = "llm")]
pub mod llm {
    pub use salvor_llm::*;
}

/// Sandboxed WebAssembly component tools, run under wasmtime with WASI capabilities denied.
///
/// Re-export of `salvor-wasm`. Requires the `wasm` feature.
#[cfg(feature = "wasm")]
pub mod wasm {
    pub use salvor_wasm::*;
}

/// The handful of names most programs want, in one import.
///
/// Deliberately small: enough to define an agent, give it tools, run it against a store, and read
/// the outcome. Anything more specific is one module path away.
pub mod prelude {
    // Each group follows its own feature: a narrow build gets a smaller prelude rather than a
    // compile error, so `default-features = false` stays usable instead of merely legal.
    pub use crate::core::{Event, EventEnvelope, RunId, RunState, RunStatus, derive_state};
    #[cfg(feature = "runtime")]
    pub use crate::runtime::{Agent, ParkReason, RunOutcome, Runtime, RuntimeError};
    #[cfg(feature = "store")]
    pub use crate::store::{EventStore, SqliteStore, StoreError};
    #[cfg(feature = "tools")]
    pub use crate::tools::{Effect, Tool, ToolCtx, ToolHandler, ToolOutcome};
}
