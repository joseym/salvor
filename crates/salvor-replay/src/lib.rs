//! Salvor replay: the pure, IO-free heart of a durable agent run. The event
//! vocabulary, the replay cursor, and the state fold, with nothing that reads
//! a clock, draws randomness, or performs IO.
//!
//! A run is an append-only sequence of events; nothing else is state. On
//! resume, completed model and tool calls are read from the log, never
//! re-executed, and execution continues live from the first unrecorded step.
//! These layers live here:
//!
//! - **The event vocabulary.** Every event is an [`Event`] payload wrapped in
//!   an [`EventEnvelope`] carrying run identity ([`RunId`]), log position
//!   ([`SequenceNumber`]), the [`SCHEMA_VERSION`], and a recorded timestamp.
//!   The vocabulary includes the deterministic-context observations
//!   ([`Event::NowObserved`], [`Event::RandomObserved`]) that let orchestration
//!   see time and randomness without breaking replay.
//! - **The replay cursor.** [`ReplayCursor`] answers each requested operation
//!   from recorded history while it lasts, then hands off to live mode. The
//!   recorded/live distinction is carried by [`Outcome`] and redeemed through
//!   typed permits, so replayed code cannot accidentally execute. Divergence
//!   from the recorded log fails loudly as a [`ReplayError`].
//! - **State derivation.** [`derive_state`] folds any log prefix into a
//!   [`RunState`]: status, next position, accumulated token usage, and any
//!   dangling call. It backs `replay --dry-run` and every future projection.
//! - **Graph-node projection.** [`derive_graph_projection`] folds a graph run's
//!   log into a [`GraphProjection`]: which nodes the walk reached, which branch
//!   cases fired, and each map node's fan-out. It is separate from
//!   [`derive_state`] on purpose: a graph run keeps the same run-level status
//!   vocabulary as an agent run, and the per-node picture lives here, and, like
//!   the rest of the crate, it never depends on `salvor-graph`.
//! - **Values read back out of a log.** [`ParkReason`] (why a run stopped short
//!   of completing) and [`RunSummary`] (the one-line-per-run projection a store
//!   returns) are plain values over the vocabulary above. They live here so a
//!   consumer that only reads or renders runs can name them without linking the
//!   IO edge that produced them.
//! - **Event rendering.** [`event_kind`] and [`event_detail`] turn one event
//!   into the kind label and the one-line detail every surface prints: the live
//!   progress stream, the `history` command, and a browser inspector. One pure
//!   implementation, so a step reads the same wherever you meet it.
//!
//! # The purity contract
//!
//! Nothing in this crate performs IO, reads a clock, or draws randomness in a
//! code path a run reaches while replaying or folding. Recorded values come
//! from the log; live values are supplied by the caller at the IO edge, which
//! lives one crate up in `salvor-runtime`. The one exception is walled off:
//! [`RunId::new`] mints a fresh random identifier and so lives behind the
//! default-on `rng` feature (see below). A build with `--no-default-features`
//! contains no randomness-drawing code at all.
//!
//! This contract is the reason the crate exists as its own member. The runtime
//! folds these events at the IO edge; the v0.3 browser inspector will fold the
//! same events, from the same code, compiled to wasm. Keeping the fold pure is
//! what lets one implementation serve both.
//!
//! # The v0.3 browser inspector
//!
//! The planned client-side inspector deserializes a run's event log and folds
//! it with [`derive_state`] inside a wasm module, so an operator can scrub a
//! run's history in a browser with no server round trip. That consumer needs
//! everything here: the [`Event`] types (to deserialize the log), the
//! [`derive_state`] fold (to project each prefix to a status), and the
//! [`ReplayCursor`] (to drive replay client-side). All of it compiles to
//! `wasm32-unknown-unknown`, proven in CI by
//! `cargo build -p salvor-replay --target wasm32-unknown-unknown --no-default-features`.
//!
//! # The `rng` feature and wasm32
//!
//! `RunId::new` draws a v4 UUID from the system random source. On
//! `wasm32-unknown-unknown` the `getrandom` crate behind that draw has no
//! default backend and fails to compile, so the draw sits behind the `rng`
//! feature. `rng` is on by default, which keeps every native consumer
//! unchanged. The wasm build turns it off, and a browser inspector never mints
//! run ids anyway: it reads ids that are already recorded in the log.
//!
//! # This crate is re-exported by `salvor-core`
//!
//! `salvor-core` depends on this crate and re-exports its entire public surface,
//! so `salvor_core::Event`, `salvor_core::ReplayCursor`, `salvor_core::derive_state`,
//! and the rest keep working for every existing consumer. New code may depend on
//! either crate; they name the same types.

mod effect;
mod event;
mod graph_state;
mod id;
mod park;
mod render;
mod replay;
mod state;
mod summary;
mod validate;

pub use effect::Effect;
pub use event::{
    Budget, BudgetKind, DedupOrigin, Event, EventEnvelope, ForkOrigin, Performer, SCHEMA_VERSION,
    TokenUsage, UnresolvedWrite,
};
pub use graph_state::{
    GraphProjection, MapIteration, MapProgress, NodeProgress, NodeState, derive_graph_projection,
};
pub use id::{RunId, SequenceNumber};
pub use park::ParkReason;
pub use render::{event_detail, event_kind};
pub use replay::{
    BeginPermit, Emitted, GraphBeginPermit, LoggedStep, ModelCallPermit, ModelReply, NowPermit,
    Outcome, Parked, RandomPermit, ReplayCursor, ReplayError, RequestedStep, ToolCallPermit,
};
pub use state::{PendingCall, RunState, RunStatus, TokenTotals, derive_state};
pub use summary::RunSummary;
pub use validate::{LogValidator, ValidationError, validate_next};
