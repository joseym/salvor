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
//! Like the replay cursor, this fold lives in the pure `salvor-replay` crate,
//! which builds for wasm32 so the v0.3 browser inspector can derive a run's
//! state from its log in-browser, from this same code.
//!
//! # The write rule
//!
//! A log whose last recorded tool intent has [`Effect::Write`] and no
//! completion derives to [`RunStatus::NeedsReconciliation`], never to
//! anything retryable. The write may or may not have reached the provider;
//! the fold refuses to guess, and so must everything built on it.

use serde_json::Value;
use time::OffsetDateTime;

use crate::effect::Effect;
use crate::event::{Budget, Event, EventEnvelope, Performer, SuspensionKind, UnresolvedWrite};
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
    /// parked, the schema the input must satisfy, and who owes the answer.
    Suspended {
        /// The recorded suspension reason.
        reason: String,
        /// The JSON Schema the resume input is validated against.
        input_schema: Value,
        /// What the run is waiting on, read straight off the recorded
        /// [`Event::Suspended`].
        ///
        /// `None` is the human gate, which is what every suspension recorded
        /// before the discriminator existed meant, so an old log folds to the
        /// status it always folded to. A [`SuspensionKind`] narrows it to a
        /// wait no operator can answer, which is the difference an inbox has
        /// to see.
        kind: Option<SuspensionKind>,
    },
    /// The run parked on a durable timer and may continue at `wake_at`.
    ///
    /// Its own status rather than a reuse of [`RunStatus::Suspended`], because
    /// every surface reads `Suspended` as awaiting a human: it lands in the
    /// approval inbox and is resumable by hand with an arbitrary payload. A
    /// sleeping run is waiting for a moment to arrive, not for anyone to
    /// decide anything, and the two must not be confused for each other in a
    /// listing, an inbox, or a resume.
    Sleeping {
        /// The recorded instant the run may continue at, read straight off
        /// [`Event::SleepStarted`]. Nothing here compares it against the
        /// current time; that comparison needs a clock and belongs at the IO
        /// edge.
        wake_at: OffsetDateTime,
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
    /// The run was abandoned by an operator: a terminal resting state in its
    /// own right, distinct from [`RunStatus::Failed`]. Abandonment is a
    /// deliberate retirement, not a failure, so it reads as its own muted
    /// terminal everywhere downstream rather than borrowing the failure ink.
    Abandoned {
        /// The operator's optional note for why the run was abandoned.
        reason: Option<String>,
        /// The write intent left unsettled when a needs-reconciliation run was
        /// abandoned, when there was one. Present only for a run abandoned from
        /// [`RunStatus::NeedsReconciliation`]; the abandonment records it so the
        /// terminal state never claims the write question was answered.
        unresolved_write: Option<UnresolvedWrite>,
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
            Event::ModelCallRequested {
                seq, request_hash, ..
            } => {
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
                ..
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
            // The recorded `kind` is carried onto the status rather than
            // dropped. It does not change what state the run is in: a run
            // waiting on a signal is parked, not running, and resumable only
            // by input the recorded schema accepts, exactly as a gate is. It
            // changes who owes the answer, and every surface that lists work
            // reads the run through this fold and nowhere else, so a
            // discriminator the fold drops is a discriminator no listing can
            // act on. Absent stays absent: an old log has no `kind` and folds
            // to `None`, which is the human gate it always meant.
            Event::Suspended {
                reason,
                input_schema,
                kind,
            } => {
                state.status = RunStatus::Suspended {
                    reason: reason.clone(),
                    input_schema: input_schema.clone(),
                    kind: *kind,
                };
            }
            Event::Resumed { .. } => {
                state.status = RunStatus::Running;
            }
            // The durable-timer pair. A sleep parks the run under its own
            // status carrying the recorded instant, never `Suspended`: nothing
            // is waiting on a human here. The completion returns the run to
            // `Running`, exactly as a resume does for a suspension.
            Event::SleepStarted { wake_at } => {
                state.status = RunStatus::Sleeping { wake_at: *wake_at };
            }
            Event::SleepCompleted {} => {
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
            // An operator-appended terminal. Abandonment gets its own resting
            // status, never `Failed`: the two are different facts and read
            // differently downstream. The `pending_call` is left as the log's
            // last dangling intent implied (kept through terminal events, like
            // `Failed`), so an abandoned needs-reconciliation run still surfaces
            // the write; `unresolved_write` is the durable, recorded copy of
            // that evidence carried on the status itself.
            Event::RunAbandoned {
                reason,
                unresolved_write,
            } => {
                state.status = RunStatus::Abandoned {
                    reason: reason.clone(),
                    unresolved_write: unresolved_write.clone(),
                };
            }
            // A graph run's head. It stands where an agent run's `RunStarted`
            // does: the run is now under way, so the status becomes `Running`.
            // No new `RunStatus` variant is minted for graph runs: the whole
            // point of this fold's graph handling is that a graph run reads
            // through the same agent-run status vocabulary. Between its
            // recorded steps it is `Running`; a dangling model or tool call
            // inside one of its agent or tool nodes still arrives as an
            // ordinary `ModelCallRequested`/`ToolCallRequested`, so the arms
            // above already carry it to `AwaitingModel`, `AwaitingTool`, or
            // `NeedsReconciliation` with no graph-specific code. `usage`
            // accumulates across every node's model calls; `pending_call`
            // surfaces whichever node's call is dangling. The per-node picture
            // (which node is current, which branch fired, map fan-out) is a
            // separate projection, `crate::graph_state`, deliberately kept out
            // of this run-level status.
            Event::GraphRunStarted { .. } => {
                state.status = RunStatus::Running;
            }
            // The graph node/branch/map/fold markers narrate the walk for the
            // per-node projection; at the run level they are structural notes
            // that change no status, exactly like the context observations
            // above. A graph run sits at `Running` across all of them (the
            // real call boundaries are the model/tool intents they bracket).
            Event::NodeEntered { .. }
            | Event::NodeExited { .. }
            | Event::NodeSkipped { .. }
            | Event::BranchTaken { .. }
            | Event::MapFannedOut { .. }
            | Event::MapIterationStarted { .. }
            | Event::MapIterationJoined { .. }
            | Event::FoldIterationStarted { .. }
            | Event::FoldIterationJoined { .. }
            | Event::FoldConverged { .. } => {}
        }
    }
    state
}

/// Whether `log` is a client-driven run's own log, on the log's own evidence:
/// the `RunStarted` at its head carries `driven_by: client`.
///
/// This is the durable half of the answer to "who drives this run". A process
/// that opened the run and holds an in-memory lease on it knows the other
/// half, but that registry is authoritative only for runs the process itself
/// opened, and after a restart (or in a process, such as `salvor wake`, that
/// never opened the run at all) it knows nothing. Only the head is read,
/// because only the head can carry the marker: a run head appears at seq 0
/// and nowhere else (the append-guard enforces that), and the field is
/// stamped by the surface that opens a client-driven run, so a later event
/// cannot forge it onto a run that is not one.
///
/// Every surface that must not become a second writer against a client's
/// drive lease calls this on the log before acting: `salvor-server`'s
/// client-run surface (to adopt rather than refuse a run reopened after a
/// restart), its resume endpoint (to keep refusing one), its wake sweeper
/// (to keep leaving a due timer to its client), and `salvor-cli`'s `wake`
/// sweep (to leave the same run alone rather than re-driving it as a second
/// writer).
#[must_use]
pub fn log_is_client_driven(log: &[EventEnvelope]) -> bool {
    matches!(
        log.first().map(|envelope| &envelope.event),
        Some(Event::RunStarted {
            driven_by: Some(Performer::Client),
            ..
        })
    )
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
            labels: None,
            driven_by: None,
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
                request_body: None,
                performed_by: None,
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
                performed_by: None,
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
                performed_by: None,
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
                performed_by: None,
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
                performed_by: None,
            },
            Event::ToolCallCompleted {
                seq: SequenceNumber::new(1),
                output: serde_json::json!({"id": "TICKET-1"}),
                deduplicated_from: None,
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
                kind: None,
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::Suspended {
                reason: "awaiting approval".into(),
                input_schema: schema,
                kind: None,
            }
        );
    }

    /// The discriminator survives the fold. A gate suspension (no recorded
    /// `kind`, which is also every suspension written before the field
    /// existed) derives to `None`, and a signal suspension derives to
    /// `Some(Signal)`, so a surface reading the status can route a webhook
    /// wait away from an approval inbox without going back to the log.
    #[test]
    fn a_suspension_carries_what_it_waits_on() {
        let schema = serde_json::json!({"type": "object"});
        let gate = derive_state(&log(vec![
            started(),
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: schema.clone(),
                kind: None,
            },
        ]));
        assert_eq!(
            gate.status,
            RunStatus::Suspended {
                reason: "awaiting approval".into(),
                input_schema: schema.clone(),
                kind: None,
            }
        );

        let signal = derive_state(&log(vec![
            started(),
            Event::Suspended {
                reason: "awaiting the payment webhook".into(),
                input_schema: schema.clone(),
                kind: Some(SuspensionKind::Signal),
            },
        ]));
        assert_eq!(
            signal.status,
            RunStatus::Suspended {
                reason: "awaiting the payment webhook".into(),
                input_schema: schema,
                kind: Some(SuspensionKind::Signal),
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
                kind: None,
            },
            Event::Resumed {
                input: serde_json::json!({"approved": true}),
            },
        ]));
        assert_eq!(state.status, RunStatus::Running);
    }

    /// A log ending at a started sleep derives to sleeping, carrying the
    /// recorded wake instant a caller needs in order to know when the run may
    /// be driven again.
    #[test]
    fn sleep_without_completion_derives_sleeping() {
        let wake_at = datetime!(2026-07-16 12:00:00.123456789 UTC);
        let state = derive_state(&log(vec![started(), Event::SleepStarted { wake_at }]));
        assert_eq!(state.status, RunStatus::Sleeping { wake_at });
    }

    /// A recorded sleep completion puts the run back in running, exactly as a
    /// resume does for a suspension.
    #[test]
    fn sleep_completion_derives_running() {
        let state = derive_state(&log(vec![
            started(),
            Event::SleepStarted {
                wake_at: datetime!(2026-07-16 12:00:00 UTC),
            },
            Event::SleepCompleted {},
        ]));
        assert_eq!(state.status, RunStatus::Running);
    }

    /// A terminal after a sleep wins, like every other terminal: the sleeping
    /// status is a resting point on the way somewhere, not a sticky one.
    #[test]
    fn a_terminal_after_a_sleep_derives_terminal() {
        let state = derive_state(&log(vec![
            started(),
            Event::SleepStarted {
                wake_at: datetime!(2026-07-16 12:00:00 UTC),
            },
            Event::SleepCompleted {},
            Event::RunCompleted {
                output: serde_json::json!({"summary": "slept on it"}),
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::Completed {
                output: serde_json::json!({"summary": "slept on it"}),
            }
        );
    }

    /// Sleeping is never suspended. A surface that reads `Suspended` as
    /// "a human owes this run an answer" must not meet a sleeping run there,
    /// so the fold keeps the two statuses apart at every position of a log
    /// that holds both kinds of parking.
    #[test]
    fn sleeping_is_never_suspended() {
        let events = vec![
            started(),
            Event::SleepStarted {
                wake_at: datetime!(2026-07-16 12:00:00 UTC),
            },
            Event::SleepCompleted {},
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: None,
            },
        ];
        let full = log(events);
        for cut in 0..=full.len() {
            let status = derive_state(&full[..cut]).status;
            let is_sleeping = matches!(status, RunStatus::Sleeping { .. });
            let is_suspended = matches!(status, RunStatus::Suspended { .. });
            assert!(
                !(is_sleeping && is_suspended),
                "cut at {cut}: a status is one or the other, never both"
            );
            assert_eq!(is_sleeping, cut == 2, "cut at {cut}: only the sleep sleeps");
            assert_eq!(
                is_suspended,
                cut == 4,
                "cut at {cut}: only the suspension suspends"
            );
        }
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

    /// An abandonment derives to the abandoned terminal, carrying its recorded
    /// reason. A bare abandonment (no dangling write) carries no
    /// unresolved-write evidence.
    #[test]
    fn abandonment_derives_abandoned() {
        let state = derive_state(&log(vec![
            started(),
            Event::RunAbandoned {
                reason: Some("husk is dead forever".into()),
                unresolved_write: None,
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::Abandoned {
                reason: Some("husk is dead forever".into()),
                unresolved_write: None,
            }
        );
    }

    /// Abandoning a needs-reconciliation run derives to the abandoned terminal
    /// carrying the unresolved-write evidence, and the dangling write is still
    /// surfaced through `pending_call` (kept through the terminal event), so the
    /// abandoned state never claims the write question was answered.
    #[test]
    fn abandonment_of_needs_reconciliation_records_unresolved_write() {
        let state = derive_state(&log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: serde_json::json!({"title": "bug"}),
                effect: Effect::Write,
                idempotency_key: None,
                performed_by: None,
            },
            Event::RunAbandoned {
                reason: None,
                unresolved_write: Some(UnresolvedWrite {
                    seq: SequenceNumber::new(1),
                    tool: "create_ticket".into(),
                }),
            },
        ]));
        assert_eq!(
            state.status,
            RunStatus::Abandoned {
                reason: None,
                unresolved_write: Some(UnresolvedWrite {
                    seq: SequenceNumber::new(1),
                    tool: "create_ticket".into(),
                }),
            }
        );
        assert!(matches!(
            state.pending_call,
            Some(PendingCall::Tool {
                effect: Effect::Write,
                ..
            })
        ));
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

    fn graph_started() -> Event {
        Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            forked_from: None,
        }
    }

    /// A graph run's head derives to running, exactly as an agent run's does:
    /// no new status is minted for graph runs.
    #[test]
    fn graph_run_started_derives_running() {
        let state = derive_state(&log(vec![graph_started()]));
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.next_seq, SequenceNumber::new(1));
    }

    /// The graph node/branch/map markers change no run status: a graph run
    /// reads as running across them, and `next_seq` still advances.
    #[test]
    fn graph_markers_do_not_change_status() {
        let state = derive_state(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "research".into(),
            },
            Event::BranchTaken {
                node: "gate".into(),
                case: "approved".into(),
            },
            Event::MapFannedOut {
                node: "fanout".into(),
                items: serde_json::json!([1, 2]),
            },
            Event::MapIterationStarted {
                node: "fanout".into(),
                index: 0,
                child_run: "sha256:child".into(),
            },
            Event::MapIterationJoined {
                node: "fanout".into(),
                index: 0,
            },
            Event::NodeExited {
                node: "research".into(),
            },
            Event::NodeSkipped {
                node: "publish".into(),
                reason: "unreached".into(),
            },
        ]));
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.next_seq, SequenceNumber::new(8));
        assert_eq!(state.pending_call, None);
    }

    /// A dangling model call inside a graph run's agent node surfaces through
    /// the same `AwaitingModel` status as an agent run: the graph markers add
    /// no new call-boundary status, they only bracket the real intents.
    #[test]
    fn dangling_model_call_inside_a_node_derives_awaiting_model() {
        let state = derive_state(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "research".into(),
            },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(2),
                request_hash: "sha256:req".into(),
                request_body: None,
                performed_by: None,
            },
        ]));
        assert_eq!(state.status, RunStatus::AwaitingModel);
        assert_eq!(
            state.pending_call,
            Some(PendingCall::Model {
                seq: SequenceNumber::new(2),
                request_hash: "sha256:req".into(),
            })
        );
    }

    /// Usage accumulates across every completed model call, widened to u64.
    #[test]
    fn usage_accumulates_across_model_calls() {
        let state = derive_state(&log(vec![
            started(),
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:a".into(),
                request_body: None,
                performed_by: None,
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
                request_body: None,
                performed_by: None,
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
