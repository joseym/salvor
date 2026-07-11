//! The reuse layer over `salvor-replay`.
//!
//! This is the whole point of the crate split: the dashboard
//! does not re-implement the event vocabulary or the state machine. It imports
//! the real, IO-free `salvor-replay` crate, compiled to wasm alongside the
//! app, and folds a run's log with the same [`derive_state`] the runtime runs
//! at the IO edge. One implementation serves both.
//!
//! This module re-exports the types the rest of the app needs so nothing else
//! reaches into `salvor_replay` directly, and it defines the one function the
//! scrubber is built on.
//!
//! # The scrubber premise
//!
//! A scrubber is bound to an event index. Dragging it must show the run's
//! derived state as of that prefix of the log. Because the log is immutable
//! once recorded and the fold is pure, that is a single call:
//! [`state_as_of`] takes `&[EventEnvelope]` and an index and returns
//! [`derive_state`] of the prefix. No recording, no server round-trip per
//! drag, and no second state machine that could drift from the runtime,
//! because it *is* the runtime's fold.

// Re-export the real vocabulary and fold. Every other module names these
// through `crate::replay::*`, so the dependency on salvor-replay lives in one
// place. There is deliberately no local copy of any of them. Several variants
// (the event and pending-call payload types) are consumed by the views
// that render them, not yet by this foundation, hence the allow.
#[allow(unused_imports)]
pub use salvor_replay::{
    Budget, BudgetKind, Effect, Event, EventEnvelope, PendingCall, RunId, RunState, RunStatus,
    SequenceNumber, TokenTotals, TokenUsage, derive_state,
};

/// The derived run state as of a prefix of the log.
///
/// `index` is a prefix length: `0` is the empty log ([`RunStatus::NotStarted`]),
/// `log.len()` is the whole log (the head state). Out-of-range indices are
/// clamped to the log length, so a scrubber bound to `0..=log.len()` can never
/// ask for a state that does not exist.
///
/// This folds via the real [`derive_state`]; it is the function criterion 2
/// points at. See [`crate::replay`] for why there is no second implementation.
#[must_use]
pub fn state_as_of(log: &[EventEnvelope], index: usize) -> RunState {
    let prefix = index.min(log.len());
    derive_state(&log[..prefix])
}

/// The derived state of the whole log, i.e. the run's current head state.
///
/// A thin alias for `state_as_of(log, log.len())`, named for the common case a
/// live view wants: the latest state, not a scrubbed-to prefix.
#[must_use]
pub fn head_state(log: &[EventEnvelope]) -> RunState {
    state_as_of(log, log.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build the log the way the SSE client does: deserialize each frame's wire
    // JSON into an EventEnvelope. This exercises both the fold and the exact
    // parse path the stream uses, and needs no time/uuid dependency to name the
    // envelope's internals.
    fn log(frames: &[&str]) -> Vec<EventEnvelope> {
        frames
            .iter()
            .map(|frame| serde_json::from_str(frame).expect("frame is valid envelope JSON"))
            .collect()
    }

    const STARTED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":0,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunStarted","payload":{"agent_def_hash":"sha256:agent","input":{"topic":"otters"}}}}"#;
    const WRITE_REQUESTED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":1,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallRequested","payload":{"seq":1,"tool":"charge_card","input":{"amount":10},"effect":"write","idempotency_key":null}}}"#;
    const WRITE_COMPLETED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":2,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallCompleted","payload":{"seq":1,"output":{"ok":true}}}}"#;

    /// A recorded frame deserializes straight into salvor_replay::EventEnvelope,
    /// the SSE client's premise.
    #[test]
    fn frame_deserializes_into_envelope() {
        let env: EventEnvelope = serde_json::from_str(STARTED).expect("parse");
        assert_eq!(env.seq, SequenceNumber::new(0));
        assert!(matches!(env.event, Event::RunStarted { .. }));
    }

    /// Prefix length 0 is the empty log: not-started, whatever the log holds.
    #[test]
    fn index_zero_is_not_started() {
        let events = log(&[STARTED]);
        assert_eq!(state_as_of(&events, 0).status, RunStatus::NotStarted);
    }

    /// Walking prefixes of growing length walks the run's state history, which
    /// is exactly what a scrubber drag does. This is the premise, proven.
    #[test]
    fn prefixes_walk_the_state_history() {
        let events = log(&[STARTED, WRITE_REQUESTED, WRITE_COMPLETED]);

        // After the write intent with no completion: needs-reconciliation.
        assert_eq!(
            state_as_of(&events, 2).status,
            RunStatus::NeedsReconciliation
        );
        // Once the completion lands: back to running.
        assert_eq!(state_as_of(&events, 3).status, RunStatus::Running);
        // The clamp: an over-long index is the head state, not a panic.
        assert_eq!(state_as_of(&events, 99), head_state(&events));
    }
}
