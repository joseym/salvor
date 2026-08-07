//! A thin wasm-bindgen wrapper over the pure `salvor-replay` crate.
//!
//! The browser inspector (the Bridge's scrubber) needs one operation: fold a
//! run's event log up to a prefix length and read the [`RunState`] it implies,
//! instantly, with no server round trip. This crate exposes exactly that,
//! calling `salvor_replay::derive_state` (the same fold the runtime uses), so
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

use std::time::Duration;

use salvor_replay::{
    Budget, BudgetExtensions, BudgetObservations, Budgets, Effect, EventEnvelope, PendingCall,
    Pricing, RunState, RunStatus, budget_extensions, budget_observations, derive_state,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
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
    Suspended {
        reason: String,
        input_schema: Value,
    },
    Sleeping {
        #[serde(with = "time::serde::rfc3339")]
        wake_at: OffsetDateTime,
    },
    BudgetExceeded {
        budget: Budget,
        observed: f64,
    },
    NeedsReconciliation,
    Completed {
        output: Value,
    },
    Failed {
        error: String,
    },
    Abandoned {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unresolved_write: Option<UnresolvedWriteDto>,
    },
}

/// The write intent an abandonment left unsettled, in the dashboard's shape.
/// Mirrors `salvor_replay::UnresolvedWrite`; present on an `Abandoned` status
/// only when the abandoned run was parked at a dangling write.
#[derive(Serialize)]
struct UnresolvedWriteDto {
    seq: u64,
    tool: String,
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
            RunStatus::Sleeping { wake_at } => RunStatusDto::Sleeping { wake_at: *wake_at },
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
            RunStatus::Abandoned {
                reason,
                unresolved_write,
            } => RunStatusDto::Abandoned {
                reason: reason.clone(),
                unresolved_write: unresolved_write.as_ref().map(|write| UnresolvedWriteDto {
                    seq: write.seq.get(),
                    tool: write.tool.clone(),
                }),
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

/// The declared limits a budget check is run against, in the vocabulary an
/// agent file writes them in.
///
/// The four dimensions are `[budgets]`'s own keys and `pricing` is
/// `[pricing]`'s, so the object `parseAgentToml` returns under those keys is
/// the object this takes, with no re-spelling in between. Every field is
/// optional; an absent dimension is never checked, exactly as
/// [`Budgets`] documents. Unknown keys are rejected, so a `step` typed for
/// `steps` is a loud refusal rather than a budget that silently never fires.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetsRequest {
    #[serde(default)]
    steps: Option<u64>,
    #[serde(default)]
    tokens: Option<u64>,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    wall_time_seconds: Option<f64>,
    #[serde(default)]
    pricing: Option<PricingRequest>,
}

/// Per-token pricing, dollars per million tokens. Required by the cost
/// dimension and ignored by every other one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingRequest {
    input_per_mtok: f64,
    output_per_mtok: f64,
}

/// What a budget check found, in the shape the dashboard consumes.
///
/// `crossed` says whether a limit was reached; `budget` and `observed` are
/// present only when it was, and are byte for byte the pair the runtime would
/// have recorded in its [`salvor_replay::Event::BudgetExceeded`]. The two
/// folded inputs come back alongside the verdict so a caller can show its
/// arithmetic rather than recompute it.
#[derive(Serialize)]
struct BudgetCheckDto {
    crossed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<Budget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed: Option<f64>,
    observations: BudgetObservationsDto,
    extensions: BudgetExtensionsDto,
}

/// The replay-derived quantities the check consumed. Mirrors
/// `salvor_replay::BudgetObservations`.
#[derive(Serialize)]
struct BudgetObservationsDto {
    steps: u64,
    input_tokens: u64,
    output_tokens: u64,
    elapsed_seconds: f64,
}

/// The extensions recorded resumes granted. Mirrors
/// `salvor_replay::BudgetExtensions`.
#[derive(Serialize)]
struct BudgetExtensionsDto {
    steps: u64,
    tokens: u64,
    cost_usd: f64,
    wall_time_seconds: f64,
}

impl From<&BudgetObservations> for BudgetObservationsDto {
    fn from(observations: &BudgetObservations) -> Self {
        BudgetObservationsDto {
            steps: observations.steps,
            input_tokens: observations.input_tokens,
            output_tokens: observations.output_tokens,
            elapsed_seconds: observations.elapsed_seconds,
        }
    }
}

impl From<&BudgetExtensions> for BudgetExtensionsDto {
    fn from(extensions: &BudgetExtensions) -> Self {
        BudgetExtensionsDto {
            steps: extensions.steps,
            tokens: extensions.tokens,
            cost_usd: extensions.cost_usd,
            wall_time_seconds: extensions.wall_time_seconds,
        }
    }
}

/// The error a budget check can return: bad input from the caller, never a
/// failure of the check itself. A crossing is an answer, not an error.
#[derive(Debug)]
pub enum BudgetCheckError {
    /// The `log_json` string was not a JSON array of event envelopes.
    Log(serde_json::Error),
    /// The `budgets_json` string was not a budget declaration.
    Budgets(serde_json::Error),
    /// The verdict failed to serialize (which cannot happen for the DTO
    /// above, but is surfaced rather than unwrapped).
    Serialize(serde_json::Error),
}

impl core::fmt::Display for BudgetCheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BudgetCheckError::Log(e) => {
                write!(f, "log is not a valid event-envelope array: {e}")
            }
            BudgetCheckError::Budgets(e) => {
                write!(f, "budgets are not a valid budget declaration: {e}")
            }
            BudgetCheckError::Serialize(e) => {
                write!(f, "the budget verdict failed to serialize: {e}")
            }
        }
    }
}

impl std::error::Error for BudgetCheckError {}

/// Evaluates a run's declared budgets against its recorded log, returned as
/// canonical JSON.
///
/// This is the check core, callable from any target, and it is the runtime's
/// own rule rather than a copy of it: `salvor_replay::budget_observations` and
/// `budget_extensions` fold the log into the numbers the loop accumulates, and
/// [`Budgets::first_crossing`] is the same function the driver calls before
/// every model call, in the same fixed order (steps, tokens, cost, wall time),
/// firing on `observed >= limit`.
///
/// `log_json` is the exact wire form the store writes: a JSON array of
/// `salvor_replay::EventEnvelope`. `budgets_json` is the declaration in the
/// agent file's own vocabulary, `{"steps":24,"tokens":400000}`, optionally
/// with `{"pricing":{"input_per_mtok":3,"output_per_mtok":15}}`.
///
/// Pass the prefix the check would have seen: the loop checks *before* each
/// model call, so the verdict behind a recorded `BudgetExceeded` at position
/// `n` is this function over `log[..n]`.
///
/// # Errors
///
/// Returns [`BudgetCheckError`] if either input does not parse.
pub fn check_budgets_to_json(
    log_json: &str,
    budgets_json: &str,
) -> Result<String, BudgetCheckError> {
    let log: Vec<EventEnvelope> = serde_json::from_str(log_json).map_err(BudgetCheckError::Log)?;
    let request: BudgetsRequest =
        serde_json::from_str(budgets_json).map_err(BudgetCheckError::Budgets)?;

    let budgets = Budgets {
        max_steps: request.steps,
        max_tokens: request.tokens,
        max_cost_usd: request.cost_usd,
        max_wall_time: request.wall_time_seconds.map(Duration::from_secs_f64),
    };
    let pricing = request.pricing.map(|pricing| Pricing {
        input_per_mtok: pricing.input_per_mtok,
        output_per_mtok: pricing.output_per_mtok,
    });

    let observations = budget_observations(&log);
    let extensions = budget_extensions(&log);
    let crossing = budgets.first_crossing(&extensions, pricing.as_ref(), &observations);

    let dto = BudgetCheckDto {
        crossed: crossing.is_some(),
        budget: crossing.map(|(budget, _)| budget),
        observed: crossing.map(|(_, observed)| observed),
        observations: (&observations).into(),
        extensions: (&extensions).into(),
    };
    serde_json::to_string(&dto).map_err(BudgetCheckError::Serialize)
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

/// Evaluates declared budgets against a run's recorded log and returns the
/// verdict as JSON.
///
/// `logJson` is the run's log as the wire JSON the store writes; `budgetsJson`
/// is the declaration in the agent file's own vocabulary
/// (`{"steps":24,"tokens":400000}`, optionally with a `pricing` object). The
/// returned string is JSON matching the `BudgetCheckJson` type in
/// `types/index.d.ts`.
///
/// Nothing is estimated: the observed steps, tokens, and elapsed time are
/// folded out of the log, the extensions come from the resumes that answered
/// earlier crossings, and the comparison is the runtime's own
/// `Budgets::first_crossing`. Pass a prefix to ask what the check saw at that
/// point; the loop checks before each model call, so the verdict behind a
/// recorded `BudgetExceeded` at position `n` is this over the first `n` events.
///
/// Throws if either input does not parse. A crossing is a verdict, not a throw.
#[wasm_bindgen(js_name = checkBudgets)]
pub fn check_budgets_js(log_json: &str, budgets_json: &str) -> Result<String, JsError> {
    check_budgets_to_json(log_json, budgets_json).map_err(|e| JsError::new(&e.to_string()))
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

    /// Budget-exceeded pins the f64 fields (`limit`, `observed`): the
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

    /// A run parked on a durable timer pins the sleeping status and its
    /// `wake_at`, which crosses as the RFC 3339 string the event recorded,
    /// nanoseconds included. Its own `kind`, never `Suspended`: the dashboard
    /// must be able to tell a timer from a run awaiting a human.
    #[test]
    fn surface_pin_sleeping() {
        let log = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"SleepStarted","payload":{"wake_at":"2025-07-22T08:00:00.123456789Z"}}"#,
            ),
        ]);
        let out = fold_prefix_to_json(&log, 2).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"Sleeping","wake_at":"2025-07-22T08:00:00.123456789Z"},"next_seq":2,"usage":{"input_tokens":0,"output_tokens":0}}"#
        );
    }

    /// A bare abandonment pins the abandoned status with neither optional key:
    /// `reason` and `unresolved_write` are skipped when absent, so the status is
    /// just its `kind`.
    #[test]
    fn surface_pin_abandoned_bare() {
        let log = wire_log(&[
            started(0),
            env(1, r#"{"kind":"RunAbandoned","payload":{}}"#),
        ]);
        let out = fold_prefix_to_json(&log, 2).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"Abandoned"},"next_seq":2,"usage":{"input_tokens":0,"output_tokens":0}}"#
        );
    }

    /// Abandoning a needs-reconciliation run pins the honesty node: the status
    /// carries the operator reason AND the `unresolved_write` (`seq`, `tool`),
    /// and the dangling write is still surfaced through `pending_call`.
    #[test]
    fn surface_pin_abandoned_with_unresolved_write() {
        let log = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"ToolCallRequested","payload":{"seq":1,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":null}}"#,
            ),
            env(
                2,
                r#"{"kind":"RunAbandoned","payload":{"reason":"husk is dead forever","unresolved_write":{"seq":1,"tool":"create_ticket"}}}"#,
            ),
        ]);
        let out = fold_prefix_to_json(&log, 3).unwrap();
        assert_eq!(
            out,
            r#"{"status":{"kind":"Abandoned","reason":"husk is dead forever","unresolved_write":{"seq":1,"tool":"create_ticket"}},"next_seq":3,"usage":{"input_tokens":0,"output_tokens":0},"pending_call":{"kind":"Tool","seq":1,"tool":"create_ticket","input":{"title":"bug"},"effect":"write"}}"#
        );
    }

    /// A log the loop has counted one model call in, with a clock reading
    /// either side of it: the shape every budget assertion below reads.
    fn one_step_log() -> String {
        wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"NowObserved","payload":{"now":"2025-07-15T08:00:00Z"}}"#,
            ),
            env(
                2,
                r#"{"kind":"ModelCallRequested","payload":{"seq":2,"request_hash":"h"}}"#,
            ),
            env(
                3,
                r#"{"kind":"ModelCallCompleted","payload":{"seq":2,"response":{"text":"hi"},"usage":{"input_tokens":30,"output_tokens":3}}}"#,
            ),
            env(
                4,
                r#"{"kind":"NowObserved","payload":{"now":"2025-07-15T08:00:09Z"}}"#,
            ),
        ])
    }

    /// An uncrossed check pins the negative shape: `crossed` false, neither
    /// optional key present, and both folded inputs still reported so a caller
    /// can show the arithmetic.
    #[test]
    fn surface_pin_budget_check_not_crossed() {
        let out = check_budgets_to_json(&one_step_log(), r#"{"steps":24}"#).unwrap();
        assert_eq!(
            out,
            r#"{"crossed":false,"observations":{"steps":1,"input_tokens":30,"output_tokens":3,"elapsed_seconds":9.0},"extensions":{"steps":0,"tokens":0,"cost_usd":0.0,"wall_time_seconds":0.0}}"#
        );
    }

    /// A crossing pins the positive shape, including the `Budget` payload the
    /// runtime would have recorded: the same `kind` spelling and the same f64
    /// `limit`.
    #[test]
    fn surface_pin_budget_check_crossed() {
        let out = check_budgets_to_json(&one_step_log(), r#"{"steps":1}"#).unwrap();
        assert_eq!(
            out,
            r#"{"crossed":true,"budget":{"kind":"steps","limit":1.0},"observed":1.0,"observations":{"steps":1,"input_tokens":30,"output_tokens":3,"elapsed_seconds":9.0},"extensions":{"steps":0,"tokens":0,"cost_usd":0.0,"wall_time_seconds":0.0}}"#
        );
    }

    /// The cost dimension needs pricing, and its arithmetic is the runtime's:
    /// 30 input and 3 output tokens at $3/$15 per million is $0.000135.
    #[test]
    fn the_cost_dimension_uses_the_declared_pricing() {
        let out = check_budgets_to_json(
            &one_step_log(),
            r#"{"cost_usd":0.0001,"pricing":{"input_per_mtok":3.0,"output_per_mtok":15.0}}"#,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["crossed"], true);
        assert_eq!(value["budget"]["kind"], "cost_usd");
        assert!((value["observed"].as_f64().unwrap() - 0.000_135).abs() < 1e-12);
    }

    /// A cost budget with no pricing is never checked, the same silence the
    /// runtime's own `first_crossing` keeps (the agent builder is what refuses
    /// the combination, one layer up).
    #[test]
    fn a_cost_budget_without_pricing_is_not_checked() {
        let out = check_budgets_to_json(&one_step_log(), r#"{"cost_usd":0.0000001}"#).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["crossed"],
            false
        );
    }

    /// A resume that answered a crossing raises the effective limit, and the
    /// raise is read out of the log rather than passed in.
    #[test]
    fn a_recorded_extension_raises_the_limit() {
        let crossed = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"ModelCallCompleted","payload":{"seq":0,"response":{"text":"hi"},"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ),
            env(
                2,
                r#"{"kind":"BudgetExceeded","payload":{"budget":{"kind":"steps","limit":1.0},"observed":1.0}}"#,
            ),
        ]);
        let before: Value =
            serde_json::from_str(&check_budgets_to_json(&crossed, r#"{"steps":1}"#).unwrap())
                .unwrap();
        assert_eq!(before["crossed"], true);

        let resumed = format!(
            "{},{}]",
            crossed.trim_end_matches(']'),
            env(
                3,
                r#"{"kind":"Resumed","payload":{"input":{"extend":{"steps":2}}}}"#
            )
        );
        let after: Value =
            serde_json::from_str(&check_budgets_to_json(&resumed, r#"{"steps":1}"#).unwrap())
                .unwrap();
        assert_eq!(after["crossed"], false, "1 step against a limit of 1 + 2");
        assert_eq!(after["extensions"]["steps"], 2);
    }

    /// A resume that answered a SUSPENSION is a different conversation, so an
    /// `extend` key in its input is not a budget extension. The adjacency rule
    /// is what tells the two apart, and it is asserted rather than assumed.
    #[test]
    fn a_suspension_resume_does_not_extend_a_budget() {
        let log = wire_log(&[
            started(0),
            env(
                1,
                r#"{"kind":"Suspended","payload":{"reason":"approve","input_schema":{}}}"#,
            ),
            env(
                2,
                r#"{"kind":"Resumed","payload":{"input":{"extend":{"steps":99}}}}"#,
            ),
        ]);
        let value: Value =
            serde_json::from_str(&check_budgets_to_json(&log, r#"{"steps":1}"#).unwrap()).unwrap();
        assert_eq!(value["extensions"]["steps"], 0);
    }

    /// A misspelled dimension is refused, not ignored: a budget that silently
    /// never fires is the failure mode `deny_unknown_fields` exists to stop.
    #[test]
    fn a_misspelled_budget_key_is_refused() {
        assert!(matches!(
            check_budgets_to_json("[]", r#"{"step":1}"#).unwrap_err(),
            BudgetCheckError::Budgets(_)
        ));
        assert!(matches!(
            check_budgets_to_json("not json", "{}").unwrap_err(),
            BudgetCheckError::Log(_)
        ));
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
