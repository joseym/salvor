//! Salvor is a durable execution runtime for AI agents, written in Rust.
//!
//! A salvor is the party that recovers a wrecked ship and its cargo. This
//! runtime does the same for a crashed agent run: every run is an
//! append-only event log, so a process killed mid-step resumes exactly where
//! it stopped, with no repeated side effects.
//!
//! This crate is the umbrella name. The runtime currently ships as a family
//! of crates: `salvor-runtime` (the agent loop and budgets), `salvor-core`
//! and `salvor-replay` (the event model and replay engine), `salvor-store`
//! (the SQLite event store), `salvor-llm` (the Messages API client),
//! `salvor-tools` (typed and MCP tools), `salvor-wasm` (sandboxed tools), and
//! the `salvor` binary from `salvor-cli`. A future release re-exports the
//! public surface from here so one dependency covers the library.
