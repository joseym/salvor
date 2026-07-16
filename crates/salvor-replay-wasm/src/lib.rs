//! A thin wasm-bindgen wrapper over the pure `salvor-replay` crate.
//!
//! The browser inspector (the Bridge's scrubber) needs one operation: fold a
//! run's event log up to a prefix length and read the [`RunState`] it implies,
//! instantly, with no server round trip. This crate exposes exactly that,
//! calling `salvor_replay::derive_state` — the same fold the runtime uses — so
//! the state a scrubbed prefix shows cannot drift from the state the server
//! would derive for that prefix.
//!
//! # The boundary
//!
//! The log crosses in as the exact wire JSON the store already writes (an array
//! of [`EventEnvelope`]s), and the folded state crosses back out as JSON. This
//! matches the store's exact-wire-JSON posture and the dashboard's SSE client,
//! which already deserializes each frame straight into
//! `salvor_replay::EventEnvelope`. The choice is measured, not assumed: see the
//! crate README's latency section. Strings-across-the-boundary stays well under
//! the scrub-latency budget on a 1k-event log, so the heavier
//! serde-wasm-bindgen path is unnecessary and unbuilt.
//!
//! # The state shape
//!
//! `salvor_replay`'s [`RunState`] and its parts derive no `Serialize` (they are
//! the runtime's internal projection), so this crate mirrors them into small
//! serializable DTOs whose wire shape is a stable contract for the dashboard.
//! The hand-written `types/index.d.ts` documents that shape and the
//! `surface_pin` test locks it; change one and the other must follow.
//!
//! # Native and wasm from one source
//!
//! The fold core ([`fold_prefix_to_json`]) is ordinary Rust that builds for any
//! target. `cargo build/test --workspace` compiles and tests it natively with
//! no wasm toolchain; wasm-pack compiles the very same code to
//! `wasm32-unknown-unknown` behind the [`derive_state`] binding. The same-fold
//! proof exists to show those two compilations agree byte for byte.

use salvor_replay::{
    Budget, Effect, EventEnvelope, PendingCall, RunState, RunStatus, derive_state,
};
use serde::Serialize;
use serde_json::Value;
use wasm_bindgen::prelude::*;

/// The run state, in the serializable shape the dashboard consumes.
///
/// A field-for-field mirror of `salvor_replay::RunState`. The wire shape here
/// is a contract pinned by the `surface_pin` test and documented in
/// `types/index.d.ts`.
#[derive(Serialize)]
struct RunStateDto {
    status: RunStatusDto,
    next_seq: u64,
    usage: TokenTotalsDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_call: Option<PendingCallDto>,
}

/// Where the run stands, as a `kind`-tagged discriminated union so the
/// dashboard can switch on `status.kind`.
#[derive(Serialize)]
#[serde(tag = "kind")]
enum RunStatusDto {
    NotStarted,
    Running,
    AwaitingModel,
    AwaitingTool,
    Suspended { reason: String, input_schema: Value },
    BudgetExceeded { budget: Budget, observed: f64 },
    NeedsReconciliation,
    Completed { output: Value },
    Failed { error: String },
}

/// Accumulated token usage across the run.
#[derive(Serialize)]
struct TokenTotalsDto {
    input_tokens: u64,
    output_tokens: u64,
}

/// The dangling call intent, when one exists, `kind`-tagged like the status.
#[derive(Serialize)]
#[serde(tag = "kind")]
enum PendingCallDto {
    Model {
        seq: u64,
        request_hash: String,
    },
    Tool {
        seq: u64,
        tool: String,
        input: Value,
        effect: Effect,
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
}

impl From<&RunState> for RunStateDto {
    fn from(state: &RunState) -> Self {
        RunStateDto {
            status: (&state.status).into(),
            next_seq: state.next_seq.get(),
            usage: TokenTotalsDto {
                input_tokens: state.usage.input_tokens,
                output_tokens: state.usage.output_tokens,
            },
            pending_call: state.pending_call.as_ref().map(Into::into),
        }
    }
}

impl From<&RunStatus> for RunStatusDto {
    fn from(status: &RunStatus) -> Self {
        match status {
            RunStatus::NotStarted => RunStatusDto::NotStarted,
            RunStatus::Running => RunStatusDto::Running,
            RunStatus::AwaitingModel => RunStatusDto::AwaitingModel,
            RunStatus::AwaitingTool => RunStatusDto::AwaitingTool,
            RunStatus::Suspended {
                reason,
                input_schema,
            } => RunStatusDto::Suspended {
                reason: reason.clone(),
                input_schema: input_schema.clone(),
            },
            RunStatus::BudgetExceeded { budget, observed } => RunStatusDto::BudgetExceeded {
                budget: *budget,
                observed: *observed,
            },
            RunStatus::NeedsReconciliation => RunStatusDto::NeedsReconciliation,
            RunStatus::Completed { output } => RunStatusDto::Completed {
                output: output.clone(),
            },
            RunStatus::Failed { error } => RunStatusDto::Failed {
                error: error.clone(),
            },
        }
    }
}

impl From<&PendingCall> for PendingCallDto {
    fn from(call: &PendingCall) -> Self {
        match call {
            PendingCall::Model { seq, request_hash } => PendingCallDto::Model {
                seq: seq.get(),
                request_hash: request_hash.clone(),
            },
            PendingCall::Tool {
                seq,
                tool,
                input,
                effect,
                idempotency_key,
            } => PendingCallDto::Tool {
                seq: seq.get(),
                tool: tool.clone(),
                input: input.clone(),
                effect: *effect,
                idempotency_key: idempotency_key.clone(),
            },
        }
    }
}

/// The error a fold can return: the log did not parse, the requested prefix ran
/// past the log, or the derived state failed to serialize (which cannot happen
/// for the DTO above, but is surfaced rather than unwrapped).
#[derive(Debug)]
pub enum FoldError {
    /// The `log_json` string was not a JSON array of event envelopes.
    Parse(serde_json::Error),
    /// `prefix_len` exceeded the number of events in the log. Valid lengths are
    /// `0..=log.len()` (the upper bound folds the whole log).
    PrefixOutOfRange { prefix_len: usize, log_len: usize },
    /// The derived state failed to serialize.
    Serialize(serde_json::Error),
}

impl core::fmt::Display for FoldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FoldError::Parse(e) => write!(f, "log is not a valid event-envelope array: {e}"),
            FoldError::PrefixOutOfRange {
                prefix_len,
                log_len,
            } => write!(
                f,
                "prefix length {prefix_len} exceeds log length {log_len} (valid range 0..={log_len})"
            ),
            FoldError::Serialize(e) => write!(f, "derived state failed to serialize: {e}"),
        }
    }
}

impl std::error::Error for FoldError {}

/// Folds the first `prefix_len` events of a wire-JSON event log into the run
/// state they imply, returned as canonical JSON.
///
/// This is the fold core, callable from any target. `log_json` is the exact
/// wire form the store writes: a JSON array of `salvor_replay::EventEnvelope`.
/// `prefix_len` is a prefix length in `0..=log.len()`; folding length 0 yields
/// the not-started state, and folding `log.len()` yields the head state.
///
/// # Errors
///
/// Returns [`FoldError`] if the log does not parse or `prefix_len` runs past the
/// log's length.
pub fn fold_prefix_to_json(log_json: &str, prefix_len: usize) -> Result<String, FoldError> {
    let log: Vec<EventEnvelope> = serde_json::from_str(log_json).map_err(FoldError::Parse)?;
    if prefix_len > log.len() {
        return Err(FoldError::PrefixOutOfRange {
            prefix_len,
            log_len: log.len(),
        });
    }
    let state = derive_state(&log[..prefix_len]);
    let dto = RunStateDto::from(&state);
    serde_json::to_string(&dto).map_err(FoldError::Serialize)
}

/// Counts the events in a wire-JSON event log, so a caller can enumerate the
/// prefix lengths a scrubber steps through without parsing the log itself.
///
/// # Errors
///
/// Returns [`FoldError::Parse`] if the log does not parse.
pub fn count_events(log_json: &str) -> Result<usize, FoldError> {
    let log: Vec<EventEnvelope> = serde_json::from_str(log_json).map_err(FoldError::Parse)?;
    Ok(log.len())
}

/// Folds a run's event log up to `prefix_len` events and returns the derived
/// [`RunState`] as JSON.
///
/// The scrubber's one operation. `log_json` is the run's log as the wire JSON
/// the store writes; `prefix_len` is how many leading events to fold (`0` for
/// the empty prefix, the event count for the head). The returned string is JSON
/// matching the `RunStateJson` type in `types/index.d.ts`.
///
/// Throws if the log does not parse or `prefix_len` exceeds the event count.
#[wasm_bindgen(js_name = deriveState)]
pub fn derive_state_js(log_json: &str, prefix_len: usize) -> Result<String, JsError> {
    fold_prefix_to_json(log_json, prefix_len).map_err(|e| JsError::new(&e.to_string()))
}

/// Returns the number of events in a run's log, for enumerating scrub positions.
///
/// Throws if the log does not parse.
#[wasm_bindgen(js_name = eventCount)]
pub fn event_count_js(log_json: &str) -> Result<usize, JsError> {
    count_events(log_json).map_err(|e| JsError::new(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single wire-JSON envelope, with a fixed run id and timestamp so the
    /// pinned assertions below stay byte-stable. `seq` and `event` vary.
    fn env(seq: u64, event_json: &str) -> String {
        format!(
            r#"{{"run_id":"00000000-0000-4000-8000-0000000000aa","seq":{seq},"schema_version":1,"recorded_at":"2025-07-15T08:00:00Z","event":{event_json}}}"#
        )
    }

    fn started(seq: u64) -> String {
        env(
            seq,
            r#"{"kind":"RunStarted","payload":{"agent_def_hash":"h","input":{}}}"#,
        )
    }

    fn wire_log(envs: &[String]) -> String {
        format!("[{}]", envs.join(","))
    }

    /// The empty prefix pins the not-started shape and the `next_seq`/`usage`
    /// fields, with `pending_call` absent (skipped when None).
    #[test]
    fn surface_pin_not_started() {
        let out = fold_prefix_to_json("[]", 0).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"NotStarted"},"next_seq":0,"usage":{"input_tokens":0,"output_tokens":0}}"#
        );
    }

    /// Running pins the tagged-status shape after one event.
    #[test]
    fn surface_pin_running() {
        let log = wire_log(&[started(0)]);
        let out = fold_prefix_to_json(&log, 1).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"Running"},"next_seq":1,"usage":{"input_tokens":0,"output_tokens":0}}"#
        );
    }

    /// A dangling write pins the needs-reconciliation status AND the full
    /// pending-call shape (the `kind`-tagged Tool variant with its effect and
    /// idempotency key). This is the richest node of the surface.
    #[test]
    fn surface_pin_needs_reconciliation_with_pending_tool() {
        let log = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"ToolCallRequested","payload":{"seq":1,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":"idem-1"}}"#,
            ),
        ]);
        let out = fold_prefix_to_json(&log, 2).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"NeedsReconciliation"},"next_seq":2,"usage":{"input_tokens":0,"output_tokens":0},"pending_call":{"kind":"Tool","seq":1,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":"idem-1"}}"#
        );
    }

    /// Budget-exceeded pins the f64 fields (`limit`, `observed`) — the
    /// cross-target number-formatting path the same-fold proof guards.
    #[test]
    fn surface_pin_budget_exceeded_floats() {
        let log = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"BudgetExceeded","payload":{"budget":{"kind":"cost_usd","limit":2.5},"observed":2.500001}}"#,
            ),
        ]);
        let out = fold_prefix_to_json(&log, 2).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"BudgetExceeded","budget":{"kind":"cost_usd","limit":2.5},"observed":2.500001},"next_seq":2,"usage":{"input_tokens":0,"output_tokens":0}}"#
        );
    }

    /// Folding length 0 of a non-empty log is the not-started prefix; folding
    /// the full length is the head. Both are valid prefixes.
    #[test]
    fn prefix_zero_and_full_are_valid() {
        let log = wire_log(&[started(0)]);
        assert!(fold_prefix_to_json(&log, 0).is_ok());
        assert!(fold_prefix_to_json(&log, 1).is_ok());
    }

    /// A prefix past the log's end is a caller error, not a panic.
    #[test]
    fn prefix_past_end_errors() {
        let log = wire_log(&[started(0)]);
        let err = fold_prefix_to_json(&log, 2).unwrap_err();
        assert!(matches!(err, FoldError::PrefixOutOfRange { .. }));
    }

    /// A log that is not a JSON envelope array is a parse error, not a panic.
    #[test]
    fn bad_log_errors() {
        assert!(matches!(
            fold_prefix_to_json("not json", 0).unwrap_err(),
            FoldError::Parse(_)
        ));
        assert!(matches!(
            fold_prefix_to_json(r#"[{"nope":true}]"#, 1).unwrap_err(),
            FoldError::Parse(_)
        ));
    }

    /// `count_events` counts the log so a caller can enumerate scrub positions.
    #[test]
    fn count_events_counts() {
        let log = wire_log(&[started(0), started(1)]);
        assert_eq!(count_events(&log).unwrap(), 2);
        assert_eq!(count_events("[]").unwrap(), 0);
    }
}
