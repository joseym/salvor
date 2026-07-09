//! Salvor core: the event model, replay engine, budget enforcement, and
//! deterministic context (`ctx.now()`, `ctx.random()`) for durable agent runs.
//!
//! A run is an append-only sequence of events; nothing else is state. On
//! resume, completed model and tool calls are read from the log, never
//! re-executed.
