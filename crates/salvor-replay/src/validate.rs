//! The append-guard validator: a pure decision on whether one candidate
//! envelope is the legal next event for a given log.
//!
//! [`ReplayCursor`](crate::ReplayCursor) answers "given the recorded log and
//! what orchestration wants next, replay or execute". This module answers a
//! different, narrower question the server's append-guard asks: "given the
//! recorded log, is this incoming envelope the one legal next event to append".
//! The cursor is keyed to orchestration requests; the guard is keyed to raw
//! envelopes arriving over the wire, where the server owns the log but not the
//! loop and must re-derive the legal next append from the log it already holds.
//!
//! # What it enforces, and why it mirrors the cursor
//!
//! The rules here are the same well-formedness rules
//! [`ReplayCursor::new`](crate::ReplayCursor::new) and the cursor's step
//! methods already encode, read off the log rather than off an orchestration
//! request:
//!
//! - Contiguous 0-based sequence numbers, one run id, a run head (`RunStarted`
//!   for an agent run or `GraphRunStarted` for a graph run), and nothing after
//!   a terminal event: the [`ReplayCursor::new`] shape rules.
//! - A model or tool intent is followed only by its correlated completion, or
//!   is the log's last event (a dangling intent): the `MalformedLog` branches
//!   in [`ReplayCursor::model_call`] and [`ReplayCursor::tool_call`]. This is
//!   where "one pending call at a time" and "completions correlate to their
//!   intent" come from: because a well-formed log resolves every intent
//!   immediately, a pending intent is always the log's last event, so a second
//!   intent or an out-of-place completion after it is rejected.
//! - A completion with no pending intent to correlate to is refused.
//!
//! # Where it is deliberately lenient
//!
//! It mirrors the cursor's leniency rather than tightening past it. In
//! particular `ReplayCursor::new` does not require a `Resumed` to follow a
//! `Suspended` (that pairing is enforced by orchestration replay, not by log
//! shape), so neither does the guard: a `Resumed`, `Suspended`, or
//! `BudgetExceeded` is a free-standing context event here, subject only to the
//! envelope, head, terminal, and no-dangling-completion rules. Over-tightening
//! would reject a log the cursor itself accepts.
//!
//! The durable-timer pair (`SleepStarted` and `SleepCompleted`) is lenient for
//! the identical reason: nothing requires a `SleepCompleted` to follow a
//! `SleepStarted`, because a run that is still asleep has recorded only the
//! start, and closing the pair is orchestration's job exactly as answering a
//! suspension is. So both are free-standing context events here too.
//! `SleepCompleted` is not a completion in the correlated sense the rules
//! above use: it names no intent and carries no correlation seq, so the
//! no-dangling-completion rule has nothing to say about it.
//!
//! `RunRedriven` is looser still, and has to be. It records that somebody drove
//! a crashed or sleeping run again, and the position it lands at is whatever
//! the log ended at, which for a crashed run is very often a dangling intent.
//! So a mark may follow anything a non-terminal log can end at, and a trailing
//! mark is looked past when the rules ask what the log ends at, leaving the
//! intent underneath it still awaiting its completion. Nothing may follow a
//! terminal, marks included: an abandoned or finished run is not driven again.
//!
//! # Purity
//!
//! Like the rest of this crate, nothing here performs IO, reads a clock, or
//! draws randomness. It reads two in-memory values (the log and the candidate)
//! and returns a decision, so it compiles to wasm32 alongside the cursor and
//! the fold, and the server runs the identical code natively on every append.

use thiserror::Error;

use crate::event::{Event, EventEnvelope, SCHEMA_VERSION};
use crate::id::{RunId, SequenceNumber};

/// Why a candidate envelope is not the legal next event for a log.
///
/// One variant per illegal class, each naming the position or values that made
/// the decision, so the server can turn it into a precise `409` body.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// The candidate names a different run than the log it would join.
    #[error("candidate run id {} does not match the log's run id {}", .found.as_uuid(), .expected.as_uuid())]
    RunIdMismatch {
        /// The run id the log carries.
        expected: RunId,
        /// The run id the candidate carried.
        found: RunId,
    },
    /// The candidate's sequence number is not the next contiguous position.
    #[error("candidate seq {found} is not the expected next position {expected}")]
    NonContiguousSeq {
        /// The only position an append may occupy: one past the log's end.
        expected: SequenceNumber,
        /// The position the candidate claimed.
        found: SequenceNumber,
    },
    /// The candidate's schema version is outside the range this build writes.
    #[error("candidate schema_version {version} is out of range 1..={max}")]
    BadSchemaVersion {
        /// The version the candidate carried.
        version: u32,
        /// The highest version this build understands ([`SCHEMA_VERSION`]).
        max: u32,
    },
    /// A fresh log must open with `RunStarted`; this candidate did not.
    #[error("a run log must start with RunStarted, candidate is {found}")]
    ExpectedRunStarted {
        /// The kind the candidate carried instead.
        found: &'static str,
    },
    /// A run head (`RunStarted` or `GraphRunStarted`) may appear only as the
    /// first event; the log already has one.
    #[error("a run head may only be the first event; the log already has history")]
    DuplicateRunStarted,
    /// The log already ended with a terminal event; nothing may follow it.
    #[error("no event may follow the terminal {terminal}")]
    AfterTerminal {
        /// The terminal kind the log ended with.
        terminal: &'static str,
    },
    /// A dangling intent must be followed by its correlated completion, and the
    /// candidate is not it (a second intent, a context event, or the wrong
    /// completion kind).
    #[error(
        "the intent at seq {intent_seq} awaits its completion; candidate {found} cannot follow it"
    )]
    ExpectedCompletion {
        /// The position of the pending intent awaiting completion.
        intent_seq: SequenceNumber,
        /// The kind the candidate carried instead of the awaited completion.
        found: &'static str,
    },
    /// The candidate is a completion of the right kind, but its correlation
    /// sequence does not match the pending intent it would complete.
    #[error("completion correlates to seq {found} but the pending intent is at seq {expected}")]
    MiscorrelatedCompletion {
        /// The pending intent's position, which the completion must name.
        expected: SequenceNumber,
        /// The position the completion actually named.
        found: SequenceNumber,
    },
    /// The candidate is a completion, but no intent is pending for it to
    /// correlate to.
    #[error("candidate {found} is a completion with no pending intent to correlate to")]
    UncorrelatedCompletion {
        /// The completion kind that had nothing to complete.
        found: &'static str,
    },
    /// A fresh intent's own correlation sequence must equal its envelope
    /// position, and this one's does not.
    #[error("intent at envelope seq {envelope_seq} carries a mismatched inner seq {inner_seq}")]
    IntentSeqMismatch {
        /// The position the envelope occupies.
        envelope_seq: SequenceNumber,
        /// The correlation sequence the payload carried.
        inner_seq: SequenceNumber,
    },
}

/// Decides whether `candidate` is the one legal next event to append to `log`.
///
/// `log` is assumed well formed (the guard's own invariant: it is either empty
/// or a prefix built by appending only validated events). The check reads the
/// log's tail, not its whole history, so it is cheap to run on every append.
///
/// # Errors
///
/// A [`ValidationError`] naming the first rule the candidate breaks. See the
/// variants for the full list; the module docs explain how each maps to a
/// cursor rule.
pub fn validate_next(
    log: &[EventEnvelope],
    candidate: &EventEnvelope,
) -> Result<(), ValidationError> {
    // Envelope-level checks, independent of the log's contents.
    if candidate.schema_version == 0 || candidate.schema_version > SCHEMA_VERSION {
        return Err(ValidationError::BadSchemaVersion {
            version: candidate.schema_version,
            max: SCHEMA_VERSION,
        });
    }
    let expected_seq = log
        .last()
        .map_or(SequenceNumber::new(0), |last| last.seq.next());
    if candidate.seq != expected_seq {
        return Err(ValidationError::NonContiguousSeq {
            expected: expected_seq,
            found: candidate.seq,
        });
    }

    // The log's last event that the rules below turn on. Trailing
    // `RunRedriven` marks are looked past: a redrive records that somebody
    // drove the run again, which resolves nothing and awaits nothing, so a run
    // redriven at a dangling intent is still awaiting that intent's completion
    // and the next append is still that completion. The cursor drops the same
    // marks before replaying (see `ReplayCursor::new`); this is the guard's
    // half of one rule. A log is never all marks, because its head is a run
    // head, so a non-empty log always has one of these.
    let last = log
        .iter()
        .rev()
        .find(|envelope| !matches!(envelope.event, Event::RunRedriven { .. }));
    let Some(last) = last else {
        // Empty log: the candidate opens the run. It must be a run head
        // (`RunStarted` for an agent run or `GraphRunStarted` for a graph run)
        // and its position (already checked to be `expected_seq` == 0) stands
        // in for any run id, since there is no prior event to match against.
        return match &candidate.event {
            Event::RunStarted { .. } | Event::GraphRunStarted { .. } => Ok(()),
            other => Err(ValidationError::ExpectedRunStarted {
                found: kind_name(other),
            }),
        };
    };

    // Non-empty log: the candidate must name the same run.
    if candidate.run_id != last.run_id {
        return Err(ValidationError::RunIdMismatch {
            expected: last.run_id,
            found: candidate.run_id,
        });
    }

    // A run head is a head-only event: neither `RunStarted` nor
    // `GraphRunStarted` may appear once the log already has history.
    if matches!(
        candidate.event,
        Event::RunStarted { .. } | Event::GraphRunStarted { .. }
    ) {
        return Err(ValidationError::DuplicateRunStarted);
    }

    // Nothing may follow a terminal event. `RunAbandoned` joins the terminal
    // family here: once an operator abandons a run, its log is closed exactly
    // as a completed or failed run's is.
    if matches!(
        last.event,
        Event::RunCompleted { .. } | Event::RunFailed { .. } | Event::RunAbandoned { .. }
    ) {
        return Err(ValidationError::AfterTerminal {
            terminal: kind_name(&last.event),
        });
    }

    // The correlation rules turn on whether the log ends at a dangling intent.
    // A well-formed log resolves every intent with the immediately following
    // completion, so an intent that is still the last event is the only pending
    // call, which is where "one pending call at a time" falls out for free.
    // A redrive mark may follow anything the log can end at, a dangling intent
    // included, because that is exactly where it lands: a crashed run is
    // redriven at the position after the intent it is about to re-issue. It
    // resolves nothing and claims nothing, so the intent below is still the
    // pending one, and the cursor drops the mark before replaying (see
    // `ReplayCursor::new`). Checked before the correlation rules so it is not
    // read as a candidate completion that got the kind wrong.
    if matches!(candidate.event, Event::RunRedriven { .. }) {
        return Ok(());
    }

    match &last.event {
        Event::ModelCallRequested {
            seq: intent_seq, ..
        } => check_completes(*intent_seq, &candidate.event, CompletionKind::Model),
        Event::ToolCallRequested {
            seq: intent_seq, ..
        } => check_completes(*intent_seq, &candidate.event, CompletionKind::Tool),
        // No pending intent: the candidate may be a fresh intent or a context
        // or control event, but never a stray completion.
        _ => match &candidate.event {
            Event::ModelCallCompleted { .. } | Event::ToolCallCompleted { .. } => {
                Err(ValidationError::UncorrelatedCompletion {
                    found: kind_name(&candidate.event),
                })
            }
            Event::ModelCallRequested { seq, .. } | Event::ToolCallRequested { seq, .. } => {
                // A fresh intent's inner correlation seq must equal its own
                // envelope position, exactly as the cursor reserves it.
                if *seq == candidate.seq {
                    Ok(())
                } else {
                    Err(ValidationError::IntentSeqMismatch {
                        envelope_seq: candidate.seq,
                        inner_seq: *seq,
                    })
                }
            }
            _ => Ok(()),
        },
    }
}

/// Which completion kind a dangling intent awaits.
#[derive(Clone, Copy)]
enum CompletionKind {
    Model,
    Tool,
}

/// Checks that `candidate` is the completion the dangling intent at
/// `intent_seq` awaits, correlated to that position.
fn check_completes(
    intent_seq: SequenceNumber,
    candidate: &Event,
    awaited: CompletionKind,
) -> Result<(), ValidationError> {
    let found = kind_name(candidate);
    match (awaited, candidate) {
        (CompletionKind::Model, Event::ModelCallCompleted { seq, .. })
        | (CompletionKind::Tool, Event::ToolCallCompleted { seq, .. }) => {
            if *seq == intent_seq {
                Ok(())
            } else {
                Err(ValidationError::MiscorrelatedCompletion {
                    expected: intent_seq,
                    found: *seq,
                })
            }
        }
        _ => Err(ValidationError::ExpectedCompletion { intent_seq, found }),
    }
}

/// A growing, always-well-formed log you fold candidates into one at a time.
///
/// Each [`push`](Self::push) validates the candidate against everything folded
/// so far and, on success, extends the working log, so the validator's own
/// invariant (the log is well formed) is preserved by construction. This is the
/// shape the server holds for a batch append: build one over the recorded log,
/// push each incoming envelope, and append the accepted ones to the store.
#[derive(Debug, Clone)]
pub struct LogValidator {
    log: Vec<EventEnvelope>,
}

impl LogValidator {
    /// Builds a validator over an existing (well-formed) log prefix.
    #[must_use]
    pub fn new(log: Vec<EventEnvelope>) -> Self {
        Self { log }
    }

    /// The position the next accepted append will occupy.
    #[must_use]
    pub fn next_seq(&self) -> SequenceNumber {
        self.log
            .last()
            .map_or(SequenceNumber::new(0), |last| last.seq.next())
    }

    /// The working log folded so far.
    #[must_use]
    pub fn log(&self) -> &[EventEnvelope] {
        &self.log
    }

    /// Decides whether `candidate` is the legal next event, without folding it.
    ///
    /// # Errors
    ///
    /// The [`ValidationError`] from [`validate_next`].
    pub fn validate(&self, candidate: &EventEnvelope) -> Result<(), ValidationError> {
        validate_next(&self.log, candidate)
    }

    /// Validates `candidate` and, on success, extends the working log with it.
    ///
    /// # Errors
    ///
    /// The [`ValidationError`] from [`validate_next`]; the log is left
    /// unchanged when the candidate is rejected.
    pub fn push(&mut self, candidate: EventEnvelope) -> Result<(), ValidationError> {
        self.validate(&candidate)?;
        self.log.push(candidate);
        Ok(())
    }
}

/// The stable name of an event's kind, for error messages. Kept local so the
/// validator carries no dependency on `replay.rs`'s private helper.
fn kind_name(event: &Event) -> &'static str {
    match event {
        Event::RunStarted { .. } => "RunStarted",
        Event::ModelCallRequested { .. } => "ModelCallRequested",
        Event::ModelCallCompleted { .. } => "ModelCallCompleted",
        Event::ToolCallRequested { .. } => "ToolCallRequested",
        Event::ToolCallCompleted { .. } => "ToolCallCompleted",
        Event::NowObserved { .. } => "NowObserved",
        Event::RandomObserved { .. } => "RandomObserved",
        Event::Suspended { .. } => "Suspended",
        Event::Resumed { .. } => "Resumed",
        Event::RunRedriven { .. } => "RunRedriven",
        Event::SleepStarted { .. } => "SleepStarted",
        Event::SleepCompleted {} => "SleepCompleted",
        Event::BudgetExceeded { .. } => "BudgetExceeded",
        Event::RunCompleted { .. } => "RunCompleted",
        Event::RunFailed { .. } => "RunFailed",
        Event::RunAbandoned { .. } => "RunAbandoned",
        Event::GraphRunStarted { .. } => "GraphRunStarted",
        Event::NodeEntered { .. } => "NodeEntered",
        Event::NodeExited { .. } => "NodeExited",
        Event::NodeSkipped { .. } => "NodeSkipped",
        Event::BranchTaken { .. } => "BranchTaken",
        Event::MapFannedOut { .. } => "MapFannedOut",
        Event::MapIterationStarted { .. } => "MapIterationStarted",
        Event::MapIterationJoined { .. } => "MapIterationJoined",
        Event::FoldIterationStarted { .. } => "FoldIterationStarted",
        Event::FoldIterationJoined { .. } => "FoldIterationJoined",
        Event::FoldConverged { .. } => "FoldConverged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::Effect;
    use crate::event::{Budget, BudgetKind, TokenUsage};
    use time::OffsetDateTime;
    use time::macros::datetime;
    use uuid::Uuid;

    fn run_a() -> RunId {
        RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-00000000000a").unwrap())
    }

    fn run_b() -> RunId {
        RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-00000000000b").unwrap())
    }

    fn ts() -> OffsetDateTime {
        datetime!(2026-07-11 12:00:00 UTC)
    }

    /// Wraps an event for run A at a given position.
    fn env(seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope::new(run_a(), SequenceNumber::new(seq), ts(), event)
    }

    fn started() -> Event {
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            driven_by: None,
            caller: None,
        }
    }

    fn model_intent(seq: u64) -> Event {
        Event::ModelCallRequested {
            seq: SequenceNumber::new(seq),
            request_hash: "sha256:req".into(),
            request_body: None,
            performed_by: None,
        }
    }

    fn model_done(seq: u64) -> Event {
        Event::ModelCallCompleted {
            seq: SequenceNumber::new(seq),
            response: serde_json::json!({"text": "hi"}),
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
            },
        }
    }

    fn tool_intent(seq: u64, effect: Effect) -> Event {
        Event::ToolCallRequested {
            seq: SequenceNumber::new(seq),
            tool: "render".into(),
            input: serde_json::json!({"src": "x"}),
            effect,
            idempotency_key: None,
            performed_by: None,
        }
    }

    fn tool_done(seq: u64) -> Event {
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(seq),
            output: serde_json::json!({"ok": true}),
            deduplicated_from: None,
            settled_by: None,
            settled_caller: None,
        }
    }

    /// A legal control-and-context sequence validates event by event, folded
    /// through the incremental validator.
    #[test]
    fn a_legal_sequence_validates_event_by_event() {
        let sequence = vec![
            started(),
            Event::NowObserved { now: ts() },
            Event::RandomObserved { value: 7 },
            Event::Suspended {
                reason: "approval".into(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: None,
            },
            Event::Resumed {
                input: serde_json::json!({"approved": true}),
                caller: None,
            },
            Event::BudgetExceeded {
                budget: Budget {
                    kind: BudgetKind::Tokens,
                    limit: 100.0,
                },
                observed: 101.0,
            },
            Event::RunCompleted {
                output: serde_json::json!({"done": true}),
            },
        ];
        let mut validator = LogValidator::new(vec![]);
        for (seq, event) in sequence.into_iter().enumerate() {
            validator
                .push(env(seq as u64, event))
                .expect("each event is the legal next one");
        }
    }

    /// A redrive mark may land after a dangling intent, which is where a
    /// crashed run's redrive puts it, and the intent underneath it still
    /// awaits its completion afterward. Both halves matter: refusing the mark
    /// would refuse the ordinary redrive, and letting the mark hide the intent
    /// would refuse the completion the re-issued call goes on to record.
    #[test]
    fn a_redrive_mark_rides_over_a_dangling_intent() {
        let mut v = LogValidator::new(vec![]);
        v.push(env(0, started())).unwrap();
        v.push(env(1, tool_intent(1, Effect::Idempotent))).unwrap();
        v.push(env(
            2,
            Event::RunRedriven {
                caller: Some("ops".into()),
            },
        ))
        .expect("a crashed run is redriven at the position after its dangling intent");
        v.push(env(3, tool_done(1)))
            .expect("the re-issued call still completes the intent the mark rode over");
    }

    /// Nothing follows a terminal, a redrive mark included: an abandoned or
    /// finished run is not driven again, and the guard says so in the same
    /// words it uses for every other kind.
    #[test]
    fn a_redrive_mark_after_a_terminal_is_refused() {
        let closed = vec![
            env(0, started()),
            env(
                1,
                Event::RunCompleted {
                    output: serde_json::json!({"ok": true}),
                },
            ),
        ];
        let error = validate_next(&closed, &env(2, Event::RunRedriven { caller: None }))
            .expect_err("a finished run is not driven again");
        assert_eq!(
            error,
            ValidationError::AfterTerminal {
                terminal: "RunCompleted"
            }
        );
    }

    /// The durable-timer pair validates as free-standing context events, in
    /// either arrangement a real log can hold: a sleep that ends and a run that
    /// continues, and a sleep that is still open when the log stops. The guard
    /// mirrors the cursor's leniency about the pairing rather than inventing a
    /// shape rule the cursor does not enforce.
    #[test]
    fn the_sleep_pair_validates_as_free_standing_events() {
        let mut v = LogValidator::new(vec![]);
        v.push(env(0, started())).unwrap();
        v.push(env(1, Event::NowObserved { now: ts() }))
            .expect("an observation precedes the sleep, as a live run records it");
        v.push(env(
            2,
            Event::SleepStarted {
                wake_at: datetime!(2026-07-18 12:00:00 UTC),
            },
        ))
        .expect("a sleep start is a legal free-standing event");
        v.push(env(3, Event::SleepCompleted {}))
            .expect("a sleep completion is a legal free-standing event");
        v.push(env(4, model_intent(4)))
            .expect("the woken run continues with an ordinary intent");

        // And the open case: a log that stops at the sleep start is a
        // well-formed log, so a candidate after it is judged on its own merits.
        let parked = vec![
            env(0, started()),
            env(
                1,
                Event::SleepStarted {
                    wake_at: datetime!(2026-07-18 12:00:00 UTC),
                },
            ),
        ];
        validate_next(&parked, &env(2, Event::SleepCompleted {}))
            .expect("closing an open sleep is legal");
        validate_next(&parked, &env(2, Event::NowObserved { now: ts() }))
            .expect("the guard never demands the completion, exactly as for a suspension");
    }

    /// Nothing about the sleep events loosens the correlation rules: a
    /// completion still needs a pending intent, and a sleep event is not one.
    #[test]
    fn a_completion_after_a_sleep_is_still_uncorrelated() {
        let log = vec![
            env(0, started()),
            env(
                1,
                Event::SleepStarted {
                    wake_at: datetime!(2026-07-18 12:00:00 UTC),
                },
            ),
            env(2, Event::SleepCompleted {}),
        ];
        let err = validate_next(&log, &env(3, model_done(1))).unwrap_err();
        assert_eq!(
            err,
            ValidationError::UncorrelatedCompletion {
                found: "ModelCallCompleted"
            }
        );
    }

    /// A sleep event cannot step past a dangling intent either: the completion
    /// comes first, exactly as for any other context event.
    #[test]
    fn sleep_after_intent_is_rejected() {
        let log = vec![env(0, started()), env(1, model_intent(1))];
        let err = validate_next(
            &log,
            &env(
                2,
                Event::SleepStarted {
                    wake_at: datetime!(2026-07-18 12:00:00 UTC),
                },
            ),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ValidationError::ExpectedCompletion {
                intent_seq: SequenceNumber::new(1),
                found: "SleepStarted",
            }
        );
    }

    /// A dangling model intent is completed by its correlated completion, and
    /// the run then continues.
    #[test]
    fn model_intent_then_correlated_completion_is_legal() {
        let mut v = LogValidator::new(vec![]);
        v.push(env(0, started())).unwrap();
        v.push(env(1, model_intent(1))).unwrap();
        v.push(env(2, model_done(1)))
            .expect("the completion correlates to the intent at seq 1");
    }

    /// A write intent's completion is a well-formed next event at the log
    /// level: the reconciliation policy lives in the server, not the guard, so
    /// the validator matches the cursor's leniency here.
    #[test]
    fn write_intent_completion_is_well_formed() {
        let mut v = LogValidator::new(vec![]);
        v.push(env(0, started())).unwrap();
        v.push(env(1, tool_intent(1, Effect::Write))).unwrap();
        v.push(env(2, tool_done(1)))
            .expect("a completion after a write intent is well formed");
    }

    fn graph_started() -> Event {
        Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            forked_from: None,
            caller: None,
        }
    }

    /// A graph run's `GraphRunStarted` is a legal fresh-log head, exactly like
    /// `RunStarted`, and its node markers validate as free-standing events
    /// afterward (they are context/control events, not intents or completions).
    #[test]
    fn graph_run_head_and_markers_validate() {
        let mut v = LogValidator::new(vec![]);
        v.push(env(0, graph_started()))
            .expect("a graph run head opens a fresh log");
        v.push(env(
            1,
            Event::NodeEntered {
                node: "research".into(),
            },
        ))
        .expect("a node marker is a legal free-standing event");
        v.push(env(
            2,
            Event::BranchTaken {
                node: "gate".into(),
                case: "approved".into(),
            },
        ))
        .expect("a branch marker is a legal free-standing event");
        v.push(env(
            3,
            Event::NodeExited {
                node: "research".into(),
            },
        ))
        .expect("a node marker is a legal free-standing event");
    }

    /// A second run head, of either kind, once the log has history is a
    /// duplicate-head rejection.
    #[test]
    fn duplicate_graph_run_head_is_rejected() {
        let log = vec![env(0, graph_started())];
        let err = validate_next(&log, &env(1, graph_started())).unwrap_err();
        assert_eq!(err, ValidationError::DuplicateRunStarted);
    }

    /// A graph marker cannot step past a dangling intent inside a node: the
    /// completion must come first, exactly as for a context event.
    #[test]
    fn graph_marker_after_intent_is_rejected() {
        let log = vec![
            env(0, graph_started()),
            env(1, Event::NodeEntered { node: "n".into() }),
            env(2, model_intent(2)),
        ];
        let err = validate_next(&log, &env(3, Event::NodeExited { node: "n".into() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::ExpectedCompletion {
                intent_seq: SequenceNumber::new(2),
                found: "NodeExited",
            }
        );
    }

    /// The first event of a fresh log must be RunStarted.
    #[test]
    fn empty_log_rejects_non_run_started() {
        let err = validate_next(&[], &env(0, Event::NowObserved { now: ts() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::ExpectedRunStarted {
                found: "NowObserved"
            }
        );
    }

    /// A second RunStarted is rejected.
    #[test]
    fn duplicate_run_started_is_rejected() {
        let log = vec![env(0, started())];
        let err = validate_next(&log, &env(1, started())).unwrap_err();
        assert_eq!(err, ValidationError::DuplicateRunStarted);
    }

    /// A non-contiguous position is rejected before anything else about the
    /// candidate is considered.
    #[test]
    fn non_contiguous_seq_is_rejected() {
        let log = vec![env(0, started())];
        let err = validate_next(&log, &env(5, Event::NowObserved { now: ts() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::NonContiguousSeq {
                expected: SequenceNumber::new(1),
                found: SequenceNumber::new(5),
            }
        );
    }

    /// A candidate naming a different run than the log is rejected.
    #[test]
    fn wrong_run_id_is_rejected() {
        let log = vec![env(0, started())];
        let foreign = EventEnvelope::new(
            run_b(),
            SequenceNumber::new(1),
            ts(),
            Event::NowObserved { now: ts() },
        );
        let err = validate_next(&log, &foreign).unwrap_err();
        assert_eq!(
            err,
            ValidationError::RunIdMismatch {
                expected: run_a(),
                found: run_b(),
            }
        );
    }

    /// A completion with no pending intent to correlate to is rejected.
    #[test]
    fn uncorrelated_completion_is_rejected() {
        let log = vec![env(0, started())];
        let err = validate_next(&log, &env(1, model_done(1))).unwrap_err();
        assert_eq!(
            err,
            ValidationError::UncorrelatedCompletion {
                found: "ModelCallCompleted"
            }
        );
    }

    /// A second intent opened while one is already pending is rejected: only
    /// one pending call at a time.
    #[test]
    fn two_pending_intents_are_rejected() {
        let log = vec![env(0, started()), env(1, model_intent(1))];
        let err = validate_next(&log, &env(2, model_intent(2))).unwrap_err();
        assert_eq!(
            err,
            ValidationError::ExpectedCompletion {
                intent_seq: SequenceNumber::new(1),
                found: "ModelCallRequested",
            }
        );
    }

    /// A completion of the right kind but the wrong correlation seq is
    /// rejected as miscorrelated.
    #[test]
    fn bad_correlation_completion_is_rejected() {
        let log = vec![env(0, started()), env(1, model_intent(1))];
        // Envelope position 2 is contiguous, but the payload correlates to seq
        // 9, which is not the pending intent.
        let err = validate_next(&log, &env(2, model_done(9))).unwrap_err();
        assert_eq!(
            err,
            ValidationError::MiscorrelatedCompletion {
                expected: SequenceNumber::new(1),
                found: SequenceNumber::new(9),
            }
        );
    }

    /// A context event cannot step past a dangling intent: the completion must
    /// come first.
    #[test]
    fn context_event_after_intent_is_rejected() {
        let log = vec![env(0, started()), env(1, model_intent(1))];
        let err = validate_next(&log, &env(2, Event::NowObserved { now: ts() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::ExpectedCompletion {
                intent_seq: SequenceNumber::new(1),
                found: "NowObserved",
            }
        );
    }

    /// No event may follow a terminal event.
    #[test]
    fn event_after_terminal_is_rejected() {
        let log = vec![
            env(0, started()),
            env(
                1,
                Event::RunCompleted {
                    output: serde_json::json!({"done": true}),
                },
            ),
        ];
        let err = validate_next(&log, &env(2, Event::NowObserved { now: ts() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::AfterTerminal {
                terminal: "RunCompleted"
            }
        );
    }

    /// No event may follow the abandoned terminal, exactly as none may follow
    /// completed or failed: `RunAbandoned` is a full member of the terminal
    /// family the guard closes the log on.
    #[test]
    fn event_after_abandoned_is_rejected() {
        let log = vec![
            env(0, started()),
            env(
                1,
                Event::RunAbandoned {
                    reason: Some("husk is dead forever".into()),
                    unresolved_write: None,
                    caller: None,
                },
            ),
        ];
        let err = validate_next(&log, &env(2, Event::NowObserved { now: ts() })).unwrap_err();
        assert_eq!(
            err,
            ValidationError::AfterTerminal {
                terminal: "RunAbandoned"
            }
        );
    }

    /// A fresh intent whose inner correlation seq does not equal its envelope
    /// position is rejected.
    #[test]
    fn intent_inner_seq_must_match_envelope() {
        let log = vec![env(0, started())];
        // Envelope at position 1, but the payload claims correlation seq 4.
        let err = validate_next(&log, &env(1, model_intent(4))).unwrap_err();
        assert_eq!(
            err,
            ValidationError::IntentSeqMismatch {
                envelope_seq: SequenceNumber::new(1),
                inner_seq: SequenceNumber::new(4),
            }
        );
    }

    /// A zero or future schema version is rejected.
    #[test]
    fn bad_schema_version_is_rejected() {
        let mut e = env(0, started());
        e.schema_version = 0;
        assert_eq!(
            validate_next(&[], &e).unwrap_err(),
            ValidationError::BadSchemaVersion {
                version: 0,
                max: SCHEMA_VERSION,
            }
        );
        let mut future = env(0, started());
        future.schema_version = SCHEMA_VERSION + 1;
        assert_eq!(
            validate_next(&[], &future).unwrap_err(),
            ValidationError::BadSchemaVersion {
                version: SCHEMA_VERSION + 1,
                max: SCHEMA_VERSION,
            }
        );
    }
}
