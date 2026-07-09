//! Salvor core: the event model, replay engine, and deterministic context for
//! durable agent runs.
//!
//! A run is an append-only sequence of events; nothing else is state. On
//! resume, completed model and tool calls are read from the log, never
//! re-executed, and execution continues live from the first unrecorded step.
//!
//! Three layers live here today:
//!
//! - **The event vocabulary.** Every event is an [`Event`] payload wrapped in
//!   an [`EventEnvelope`] carrying run identity, log position
//!   ([`SequenceNumber`]), the [`SCHEMA_VERSION`], and a recorded timestamp.
//!   The vocabulary includes the deterministic-context observations
//!   ([`Event::NowObserved`], [`Event::RandomObserved`]) that let
//!   orchestration see time and randomness without breaking replay.
//! - **The replay cursor.** [`ReplayCursor`] answers each requested operation
//!   from recorded history while it lasts, then hands off to live mode. The
//!   recorded/live distinction is carried by [`Outcome`] and redeemed through
//!   typed permits, so replayed code cannot accidentally execute. Divergence
//!   from the recorded log fails loudly as a [`ReplayError`].
//! - **State derivation.** [`derive_state`] folds any log prefix into a
//!   [`RunState`]: status, next position, accumulated token usage, and any
//!   dangling call. It backs `replay --dry-run` and every future projection.
//!
//! Everything is pure: no type here reads a clock, draws randomness, or
//! performs IO. Recorded values come from the log; live values are supplied
//! by the caller at the IO edge. That purity is what lets the replay path
//! move into an IO-free `salvor-replay` crate (with a wasm32 target) in v0.2.

mod effect;
mod event;
mod id;
mod replay;
mod state;

pub use effect::Effect;
pub use event::{Budget, BudgetKind, Event, EventEnvelope, SCHEMA_VERSION, TokenUsage};
pub use id::{RunId, SequenceNumber};
pub use replay::{
    BeginPermit, Emitted, LoggedStep, ModelCallPermit, ModelReply, NowPermit, Outcome, Parked,
    RandomPermit, ReplayCursor, ReplayError, RequestedStep, ToolCallPermit,
};
pub use state::{PendingCall, RunState, RunStatus, TokenTotals, derive_state};
