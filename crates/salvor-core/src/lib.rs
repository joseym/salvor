//! Salvor core: the event model, replay engine, budget enforcement, and
//! deterministic context (`ctx.now()`, `ctx.random()`) for durable agent runs.
//!
//! A run is an append-only sequence of events; nothing else is state. On
//! resume, completed model and tool calls are read from the log, never
//! re-executed.
//!
//! This crate lands the event vocabulary. Every event is an [`Event`]
//! payload wrapped in an [`EventEnvelope`] that carries run identity, the
//! event's position in the log ([`SequenceNumber`]), the [`SCHEMA_VERSION`],
//! and when it was recorded. The vocabulary is pure data: no type here reads
//! the clock, draws randomness, or performs IO, so the replay path can later
//! move into an IO-free crate.

mod effect;
mod event;
mod id;

pub use effect::Effect;
pub use event::{Budget, BudgetKind, Event, EventEnvelope, SCHEMA_VERSION, TokenUsage};
pub use id::{RunId, SequenceNumber};
