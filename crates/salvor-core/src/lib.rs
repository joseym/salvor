//! Salvor core: the stable public surface for the event model, replay engine,
//! and deterministic context of durable agent runs.
//!
//! A run is an append-only sequence of events; nothing else is state. On
//! resume, completed model and tool calls are read from the log, never
//! re-executed, and execution continues live from the first unrecorded step.
//!
//! # What lives where
//!
//! The types themselves live in [`salvor_replay`], a pure, IO-free crate that
//! also builds for `wasm32-unknown-unknown` so the v0.3 browser inspector can
//! fold event logs client-side. That crate holds the event vocabulary
//! ([`Event`], [`EventEnvelope`], [`RunId`], [`SequenceNumber`],
//! [`SCHEMA_VERSION`]), the replay cursor ([`ReplayCursor`] and its typed
//! [`Outcome`] permits), the state fold ([`derive_state`] into [`RunState`]),
//! the values read back out of a log ([`ParkReason`], [`RunSummary`]), the
//! budget declaration and its crossing check ([`Budgets`], [`Pricing`],
//! [`BudgetObservations`], [`budget_observations`]), and the event renderers
//! ([`event_kind`], [`event_detail`]).
//!
//! `salvor-core` re-exports that surface unchanged. Every `salvor_core::` path
//! that existed before the extraction keeps resolving, so the store, the
//! runtime, the tools layer, and the CLI compile against the same names they
//! always used. New code may depend on either crate; the names are identical.
//! The split exists so the runtime and the browser inspector fold events with
//! literally the same code, without the runtime's IO edge leaking into the
//! pure path.

pub use salvor_replay::{
    BeginPermit, Budget, BudgetExtensions, BudgetKind, BudgetObservations, Budgets, DedupOrigin,
    Effect, Emitted, Event, EventEnvelope, ForkOrigin, GraphBeginPermit, LogValidator, LoggedStep,
    ModelCallPermit, ModelReply, NowPermit, Outcome, ParkReason, Parked, PendingCall, Performer,
    Pricing, RandomPermit, ReplayCursor, ReplayError, RequestedStep, RunId, RunState, RunStatus,
    RunSummary, SCHEMA_VERSION, SequenceNumber, SettledBy, SuspensionKind, TokenTotals, TokenUsage,
    ToolCallPermit, UnresolvedWrite, ValidationError, budget_extensions, budget_observations,
    derive_state, event_detail, event_kind, log_is_client_driven, validate_extension_input,
    validate_next,
};
