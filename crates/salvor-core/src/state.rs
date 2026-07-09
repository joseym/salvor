//! State derivation: a pure fold from an event log to the run state it
//! implies.
//!
//! This is the projection behind `replay --dry-run` and, later, the
//! dashboard: events in, state out, nothing executed. It is total over every
//! prefix of a valid log, because a crash can happen at any event boundary
//! and every prefix is therefore a state something might resume from.
//!
//! # Purity
//!
//! No IO, no clock, no randomness, no dependency on storage or executors.
//! Like the replay cursor, this module is written to lift unchanged into the
//! pure `salvor-replay` crate planned for v0.2 (and from there to a wasm32
//! build for in-browser inspection).
//!
//! # The write rule
//!
//! A log whose last recorded tool intent has [`Effect::Write`] and no
//! completion derives to [`RunStatus::NeedsReconciliation`], never to
//! anything retryable. The write may or may not have reached the provider;
//! the fold refuses to guess, and so must everything built on it.

use serde_json::Value;

use crate::effect::Effect;
use crate::event::{Budget, Event, EventEnvelope};
use crate::id::SequenceNumber;

/// Token usage accumulated across every completed model call in a log.
///
/// Wider than the per-call [`crate::TokenUsage`] counters on purpose: a long
/// run sums many calls, and the fold must stay total, so accumulation uses
/// `u64` and saturating arithmetic instead of trusting the sum to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenTotals {
    /// Total input (prompt) tokens across the run.
    pub input_tokens: u64,
    /// Total output (completion) tokens across the run.
    pub output_tokens: u64,
}

/// A recorded call intent with no recorded completion.
///
/// Carries everything needed to act on the gap: re-issue a model call,
/// retry an idempotent call under its recorded key, or show a human the
/// evidence for reconciling a write.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingCall {
    /// A model call was requested and never completed. Safe to re-issue.
    Model {
        /// The log position of the intent.
        seq: SequenceNumber,
        /// The recorded hash of the request.
        request_hash: String,
    },
    /// A tool call was requested and never completed. What may be done next
    /// depends on `effect`; see the [`Effect`] docs for the table.
    Tool {
        /// The log position of the intent.
        seq: SequenceNumber,
        /// The tool that was called.
        tool: String,
        /// The recorded input.
        input: Value,
        /// The declared effect class.
        effect: Effect,
        /// The recorded idempotency key. For an idempotent retry this exact
        /// key must be reused, so the provider collapses the attempts.
        idempotency_key: Option<String>,
    },
}

/// Where a run stands, as derived from its log alone.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    /// The log is empty: nothing has been recorded yet.
    NotStarted,
    /// The run is between recorded steps and can continue.
    Running,
    /// A model call intent is recorded with no completion. The call can be
    /// re-issued.
    AwaitingModel,
    /// A read or idempotent tool intent is recorded with no completion. The
    /// call can be re-executed (reads freely, idempotent calls under their
    /// recorded key).
    AwaitingTool,
    /// The run parked awaiting input. Carries what a resume needs: why it
    /// parked and the schema the input must satisfy.
    Suspended {
        /// The recorded suspension reason.
        reason: String,
        /// The JSON Schema the resume input is validated against.
        input_schema: Value,
    },
    /// A declared budget was crossed and the run parked. A human can raise
    /// the limit and resume.
    BudgetExceeded {
        /// The budget that was crossed.
        budget: Budget,
        /// The observed value, in the units of the budget's kind.
        observed: f64,
    },
    /// A write intent is recorded with no completion. The write may or may
    /// not have happened; only a human may decide what happens next. Never
    /// derived as retryable, by design.
    NeedsReconciliation,
    /// The run finished with this output.
    Completed {
        /// The recorded final output.
        output: Value,
    },
    /// The run failed with this error.
    Failed {
        /// The recorded failure description.
        error: String,
    },
}

/// Everything a log prefix implies about a run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunState {
    /// Where the run stands.
    pub status: RunStatus,
    /// The position the next appended event will occupy.
    pub next_seq: SequenceNumber,
    /// Token usage accumulated over every completed model call.
    pub usage: TokenTotals,
    /// The dangling call intent, when one exists. `Some` whenever `status`
    /// is [`RunStatus::AwaitingModel`], [`RunStatus::AwaitingTool`], or
    /// [`RunStatus::NeedsReconciliation`]. Kept through a terminal event
    /// too, so an explicitly failed run still shows the unresolved call.
    pub pending_call: Option<PendingCall>,
}

/// Folds an event log into the [`RunState`] it implies.
///
/// Total over every prefix of a valid log: it never panics and never
/// executes anything, whatever boundary the log was cut at. Later events
/// simply overwrite what earlier ones implied, so feeding it prefixes of
/// growing length walks the run's whole state history (which is exactly what
/// a dashboard scrubber will do).
#[must_use]
pub fn derive_state(log: &[EventEnvelope]) -> RunState {
    let mut state = RunState {
        status: RunStatus::NotStarted,
        next_seq: SequenceNumber::new(0),
        usage: TokenTotals::default(),
        pending_call: None,
    };
    for envelope in log {
        state.next_seq = envelope.seq.next();
        match &envelope.event {
            Event::RunStarted { .. } => {
                state.status = RunStatus::Running;
            }
            Event::ModelCallRequested { seq, request_hash } => {
                state.pending_call = Some(PendingCall::Model {
                    seq: *seq,
                    request_hash: request_hash.clone(),
                });
                state.status = RunStatus::AwaitingModel;
            }
            Event::ModelCallCompleted { usage, .. } => {
                state.usage.input_tokens = state
                    .usage
                    .input_tokens
                    .saturating_add(u64::from(usage.input_tokens));
                state.usage.output_tokens = state
                    .usage
                    .output_tokens
                    .saturating_add(u64::from(usage.output_tokens));
                state.pending_call = None;
                state.status = RunStatus::Running;
            }
            Event::ToolCallRequested {
                seq,
                tool,
                input,
                effect,
                idempotency_key,
            } => {
                state.pending_call = Some(PendingCall::Tool {
                    seq: *seq,
                    tool: tool.clone(),
                    input: input.clone(),
                    effect: *effect,
                    idempotency_key: idempotency_key.clone(),
                });
                // The write rule: an uncompleted write intent is
                // needs-reconciliation the moment it is the log's last word,
                // and every prefix ending here is exactly that log.
                state.status = match effect {
                    Effect::Write => RunStatus::NeedsReconciliation,
                    Effect::Read | Effect::Idempotent => RunStatus::AwaitingTool,
                };
            }
            Event::ToolCallCompleted { .. } => {
                state.pending_call = None;
                state.status = RunStatus::Running;
            }
            // Deterministic-context observations change no run status; they
            // only exist so replay can hand the same values back.
            Event::NowObserved { .. } | Event::RandomObserved { .. } => {}
            Event::Suspended {
                reason,
                input_schema,
            } => {
                state.status = RunStatus::Suspended {
                    reason: reason.clone(),
                    input_schema: input_schema.clone(),
                };
            }
            Event::Resumed { .. } => {
                state.status = RunStatus::Running;
            }
            Event::BudgetExceeded { budget, observed } => {
                state.status = RunStatus::BudgetExceeded {
                    budget: *budget,
                    observed: *observed,
                };
            }
            Event::RunCompleted { output } => {
                state.status = RunStatus::Completed {
                    output: output.clone(),
                };
            }
            Event::RunFailed { error } => {
                state.status = RunStatus::Failed {
                    error: error.clone(),
                };
            }
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TokenUsage;
    use crate::id::RunId;
    use time::macros::datetime;
    use uuid::Uuid;

    fn log(events: Vec<Event>) -> Vec<EventEnvelope> {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap());
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| {
                EventEnvelope::new(
                    run_id,
                    SequenceNumber::new(i as u64),
                    datetime!(2026-07-09 12:00:00 UTC),
                    event,
                )
            })
            .collect()
    }

    fn started() -> Event {
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: serde_json::json!({"topic": "otters"}),
        }
    }

    /// The empty prefix is a state too: nothing recorded, next position 0.
    #[test]
    fn empty_log_derives_not_started() {
        let state = derive_state(&[]);
        assert_eq!(state.status, RunStatus::NotStarted);
        assert_eq!(state.next_seq, SequenceNumber::new(0));
        assert_eq!(state.usage, TokenTotals::default());
        assert_eq!(state.pending_call, None);
    }

    /// A log ending between steps derives to running.
    #[test]
    fn run_started_derives_running() {
        let state = derive_state(&log(vec![started()]));
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.next_seq, SequenceNumber::new(1));
    }

    /// A dangling model intent derives to awaiting-model with the pending
    /// call carrying what a re-issue needs.
    #[test]
    fn dangling_model_intent_derives_awaiting_model() {
        let state = derive_state(&log(vec![
            started(),
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:req".into(),
            },
        ]));
        assert_eq!(state.status, RunStatus::AwaitingModel);
        assert_eq!(
            state.pending_call,
            Some(PendingCall::Model {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:req".into(),
            })
        );
    }

    /// A dangling read intent derives to awaiting-tool: reads re-execute
    /// freely, so the state is retryable.
    #[test]
    fn dangling_read_intent_derives_awaiting_tool() {
        let state = derive_state(&log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "search".into(),
                input: serde_json::json!({"q": "otters"}),
                effect: Effect::Read,
                idempotency_key: None,
            },
        ]));
        assert_eq!(state.status, RunStatus::AwaitingTool);
    }

    /// A dangling idempotent intent is retryable and surfaces its recorded
    /// idempotency key, so the retry collapses at the provider.
    #[test]
    fn dangling_idempotent_intent_surfaces_recorded_key() {
        let state = derive_state(&log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "store".into(),
                input: serde_json::json!({"doc": 1}),
                effect: Effect::Idempotent,
                idempotency_key: Some("key-7".into()),
            },
        ]));
        assert_eq!(state.status, RunStatus::AwaitingTool);
        match state.pending_call {
            Some(PendingCall::Tool {
                idempotency_key, ..
            }) => assert_eq!(idempotency_key.as_deref(), Some("key-7")),
            other => panic!("expected pending tool call, got {other:?}"),
        }
    }

    /// The write rule: a dangling write intent derives to
    /// needs-reconciliation, never to anything retryable.
    #[test]
    fn dangling_write_intent_derives_needs_reconciliation() {
        let state = derive_state(&log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: serde_json::json!({"title": "bug"}),
                effect: Effect::Write,
                idempotency_key: None,
            },
        ]));
        assert_eq!(state.status, RunStatus::NeedsReconciliation);
        assert!(matches!(
            state.pending_call,
            Some(PendingCall::Tool {
                effect: Effect::Write,
                ..
            })
        ));
    }

    /// A completed write intent is history, not a hazard.
    #[test]
    fn completed_write_derives_running() {
        let state = derive_state(&log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: serde_json::json!({"title": "bug"}),
                effect: Effect::Write,
                idempotency_key: None,
            },
            Event::ToolCallCompleted {
                seq: SequenceNumber::new(1),
                output: serde_json::json!({"id": "TICKET-1"}),
            },
        ]));
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.pending_call, None);
    }

    /// A log ending at a suspension derives to suspended, carrying what a
    /// resume needs.
    #[test]
    fn suspension_without_resume_derives_suspended() {
        let schema = serde_json::json!({"type": "object"});
        let state = derive_state(&log(vec![
            started(),
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: schema.clone(),
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::Suspended {
                reason: "awaiting approval".into(),
                input_schema: schema,
            }
        );
    }

    /// A recorded resume puts the run back in running.
    #[test]
    fn resume_derives_running() {
        let state = derive_state(&log(vec![
            started(),
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            Event::Resumed {
                input: serde_json::json!({"approved": true}),
            },
        ]));
        assert_eq!(state.status, RunStatus::Running);
    }

    /// A log ending at a budget crossing derives to budget-exceeded, parked
    /// rather than dead.
    #[test]
    fn budget_crossing_derives_budget_exceeded() {
        let budget = Budget {
            kind: crate::event::BudgetKind::CostUsd,
            limit: 2.0,
        };
        let state = derive_state(&log(vec![
            started(),
            Event::BudgetExceeded {
                budget,
                observed: 2.5,
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::BudgetExceeded {
                budget,
                observed: 2.5,
            }
        );
    }

    /// Terminal events derive to completed and failed, carrying their
    /// recorded payloads.
    #[test]
    fn terminal_events_derive_terminal_statuses() {
        let completed = derive_state(&log(vec![
            started(),
            Event::RunCompleted {
                output: serde_json::json!({"summary": "done"}),
            },
        ]));
        assert_eq!(
            completed.status,
            RunStatus::Completed {
                output: serde_json::json!({"summary": "done"}),
            }
        );

        let failed = derive_state(&log(vec![
            started(),
            Event::RunFailed {
                error: "provider timeout".into(),
            },
        ]));
        assert_eq!(
            failed.status,
            RunStatus::Failed {
                error: "provider timeout".into(),
            }
        );
    }

    /// Context observations leave the status untouched.
    #[test]
    fn context_events_do_not_change_status() {
        let state = derive_state(&log(vec![
            started(),
            Event::NowObserved {
                now: datetime!(2026-07-09 12:00:00.123456789 UTC),
            },
            Event::RandomObserved { value: u64::MAX },
        ]));
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.next_seq, SequenceNumber::new(3));
    }

    /// Usage accumulates across every completed model call, widened to u64.
    #[test]
    fn usage_accumulates_across_model_calls() {
        let state = derive_state(&log(vec![
            started(),
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:a".into(),
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(1),
                response: serde_json::json!({"text": "one"}),
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 40,
                },
            },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(3),
                request_hash: "sha256:b".into(),
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(3),
                response: serde_json::json!({"text": "two"}),
                usage: TokenUsage {
                    input_tokens: u32::MAX,
                    output_tokens: 2,
                },
            },
        ]));
        assert_eq!(
            state.usage,
            TokenTotals {
                input_tokens: 100 + u64::from(u32::MAX),
                output_tokens: 42,
            }
        );
    }
}
