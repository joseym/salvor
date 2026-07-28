//! Live progress: turning each persisted event into a one-line `tracing`
//! record, emitted the instant the event becomes durable.
//!
//! Live emission replaces walking the log *after* a drive returned: the
//! runtime is the only layer that knows a step
//! happened as it happens, so it is the layer that emits the step. Every
//! [`crate::RunCtx`] persist calls [`emit_step`] right after the store append
//! succeeds; a subscriber installed by the binary (the CLI writes it to
//! stderr) renders the record. Replayed events are answered from the log and
//! never re-persisted, so they never re-emit: a resumed or recovered run
//! streams only its genuinely new activity.
//!
//! # What is emitted, and what is withheld
//!
//! Each record carries the correlation fields an operator needs (`run_id` and
//! `seq`) and a scannable message of the form `"<kind> <detail>"`. The detail
//! is the same one-line summary the `history` command prints, produced by the
//! shared [`event_kind`] / [`event_detail`] functions so a step reads the
//! same whether you watch it live or inspect it later.
//!
//! Those two functions are pure, so they live in the `salvor-replay` crate
//! that `salvor-core` re-exports, alongside the event vocabulary they render.
//! They are re-exported from here so every `salvor_runtime::event_kind` and
//! `salvor_runtime::event_detail` path keeps resolving. Only [`emit_step`],
//! which reaches for the `tracing` subscriber, stays at this IO edge.
//!
//! The detail deliberately does **not** carry full payloads. Inputs, outputs,
//! resume values, and error messages are truncated so a model's raw output or
//! a tool's raw arguments never land in the progress stream in full. The
//! untruncated form lives only in the event log, reachable through
//! `salvor history --json`.

use salvor_core::{Event, RunId, SequenceNumber};

pub use salvor_core::{event_detail, event_kind};

/// Emits one info-level progress record for a freshly persisted event.
///
/// Called from the [`crate::RunCtx`] IO edge the moment an append returns
/// `Ok`, so the record reaches the subscriber while the run is still driving.
/// The `run_id` and `seq` fields correlate the line to its place in the log;
/// the message is `"<kind> <detail>"` with the payload truncated.
pub(crate) fn emit_step(run_id: RunId, seq: SequenceNumber, event: &Event) {
    tracing::info!(
        run_id = %run_id.as_uuid(),
        seq = seq.get(),
        "{} {}",
        event_kind(event),
        event_detail(event),
    );
}
