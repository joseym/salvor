//! The kill-at-every-boundary property, exercised at the replay-engine
//! level: for every prefix of a reference log, the state fold yields a
//! coherent state, and replaying then continuing from that prefix reaches
//! the identical final state (in fact, the identical final log) as the
//! uninterrupted run.
//!
//! Three reference runs cover the log shapes a crash can leave behind: a
//! completed run touching every event kind, a run that dies between a write
//! intent and its completion, and a run that ends in failure. The one
//! deliberate exception to "continuing reaches the same final state" is a
//! prefix ending in a dangling write intent: continuing from there must
//! refuse with needs-reconciliation rather than re-execute the write, and
//! the test asserts exactly that.

use salvor_replay::{
    Budget, BudgetKind, Effect, Emitted, Event, EventEnvelope, Outcome, PendingCall, ReplayCursor,
    ReplayError, RunId, RunStatus, SequenceNumber, TokenTotals, TokenUsage, derive_state,
};
use serde_json::{Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// One step of a scripted run. The script is the single source of both the
/// live values (used when the drive goes past recorded history) and the
/// expected replayed values (asserted when history covers the step), so a
/// replay that returned anything but the recorded value fails immediately.
enum Step {
    Begin {
        hash: &'static str,
        input: Value,
    },
    Now {
        value: OffsetDateTime,
    },
    Random {
        value: u64,
    },
    Model {
        request_hash: &'static str,
        response: Value,
        usage: TokenUsage,
    },
    Tool {
        tool: &'static str,
        input: Value,
        effect: Effect,
        key: Option<&'static str>,
        /// `None` scripts a crash after the intent is persisted but before
        /// the tool completes.
        output: Option<Value>,
    },
    Suspend {
        reason: &'static str,
        schema: Value,
        resume: Value,
    },
    /// A budget crossing interposed by the runtime between steps, followed
    /// by a human extending the budget and resuming.
    Budget {
        budget: Budget,
        observed: f64,
        resume: Value,
    },
    Complete {
        output: Value,
    },
    Fail {
        error: &'static str,
    },
}

fn run_id() -> RunId {
    RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000004").unwrap())
}

/// A deterministic timestamp per position, so re-driven runs produce
/// byte-identical envelopes and whole-log equality is assertable.
fn ts(seq: SequenceNumber) -> OffsetDateTime {
    datetime!(2026-07-09 12:00:00 UTC) + Duration::seconds(i64::try_from(seq.get()).unwrap())
}

/// What one drive produced: the full log (prefix plus everything appended)
/// and how many times anything was executed live.
#[derive(Debug)]
struct Drive {
    log: Vec<EventEnvelope>,
    executions: u32,
}

/// Drives the scripted run over a recorded prefix. Recorded steps are
/// asserted to replay the scripted values exactly; unrecorded steps execute
/// "live" (using the scripted values) and append to the log.
fn drive(script: &[Step], prefix: &[EventEnvelope]) -> Result<Drive, ReplayError> {
    let mut cursor = ReplayCursor::new(prefix.to_vec())?;
    let mut log = prefix.to_vec();
    let mut executions = 0;

    macro_rules! push {
        ($emitted:expr) => {{
            let emitted: Emitted = $emitted;
            log.push(EventEnvelope::new(
                run_id(),
                emitted.seq,
                ts(emitted.seq),
                emitted.event,
            ));
        }};
    }

    for step in script {
        match step {
            Step::Begin { hash, input } => match cursor.begin(hash)? {
                Outcome::Replayed(recorded) => assert_eq!(&recorded, input),
                Outcome::Live(permit) => push!(permit.record(input.clone())),
            },
            Step::Now { value } => match cursor.now()? {
                Outcome::Replayed(recorded) => assert_eq!(recorded, *value),
                Outcome::Live(permit) => {
                    executions += 1;
                    push!(permit.record(*value));
                }
            },
            Step::Random { value } => match cursor.random()? {
                Outcome::Replayed(recorded) => assert_eq!(recorded, *value),
                Outcome::Live(permit) => {
                    executions += 1;
                    push!(permit.record(*value));
                }
            },
            Step::Model {
                request_hash,
                response,
                usage,
            } => match cursor.model_call(request_hash)? {
                Outcome::Replayed(reply) => {
                    assert_eq!(&reply.response, response);
                    assert_eq!(&reply.usage, usage);
                }
                Outcome::Live(permit) => {
                    if let Some(intent) = permit.intent() {
                        push!(intent.clone());
                    }
                    executions += 1;
                    push!(permit.record(response.clone(), *usage));
                }
            },
            Step::Tool {
                tool,
                input,
                effect,
                key,
                output,
            } => match output {
                Some(output) => match cursor.tool_call(tool, input, *effect, *key)? {
                    Outcome::Replayed(recorded) => assert_eq!(&recorded, output),
                    Outcome::Live(permit) => {
                        if let Some(intent) = permit.intent() {
                            push!(intent.clone());
                        }
                        executions += 1;
                        push!(permit.record(output.clone()));
                    }
                },
                // The scripted crash: persist the intent, then die before
                // the tool runs. When the prefix already ends at this
                // intent, the cursor itself refuses with the reconciliation
                // error, which ends the run in the same place.
                None => match cursor.tool_call(tool, input, *effect, *key) {
                    Ok(Outcome::Live(permit)) => {
                        if let Some(intent) = permit.intent() {
                            push!(intent.clone());
                        }
                        return Ok(Drive { log, executions });
                    }
                    Err(ReplayError::NeedsReconciliation { .. }) => {
                        return Ok(Drive { log, executions });
                    }
                    Ok(Outcome::Replayed(_)) => {
                        panic!("a crashed call can never have a recorded completion")
                    }
                    Err(other) => return Err(other),
                },
            },
            Step::Suspend {
                reason,
                schema,
                resume,
            } => {
                match cursor.suspend(reason, schema)? {
                    Outcome::Replayed(()) => {}
                    Outcome::Live(emitted) => push!(emitted),
                }
                match cursor.await_resume()? {
                    Outcome::Replayed(recorded) => assert_eq!(&recorded, resume),
                    Outcome::Live(parked) => push!(parked.resume(resume.clone())),
                }
            }
            Step::Budget {
                budget,
                observed,
                resume,
            } => {
                match cursor.budget_exceeded(*budget, *observed)? {
                    Outcome::Replayed(()) => {}
                    Outcome::Live(emitted) => push!(emitted),
                }
                match cursor.await_resume()? {
                    Outcome::Replayed(recorded) => assert_eq!(&recorded, resume),
                    Outcome::Live(parked) => push!(parked.resume(resume.clone())),
                }
            }
            Step::Complete { output } => match cursor.complete_run(output)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
            Step::Fail { error } => match cursor.fail_run(error)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
        }
    }
    Ok(Drive { log, executions })
}

/// True when the log's last event is an uncompleted write intent, the one
/// prefix shape a continuation must refuse rather than complete.
fn ends_in_dangling_write(log: &[EventEnvelope]) -> bool {
    matches!(
        log.last().map(|env| &env.event),
        Some(Event::ToolCallRequested {
            effect: Effect::Write,
            ..
        })
    )
}

/// The status shape each prefix must derive to, recomputed here from the
/// prefix's last event alone so a regression in the fold's state machine
/// cannot hide behind the fold itself.
fn expected_status(prefix: &[EventEnvelope]) -> RunStatus {
    let Some(last) = prefix.last() else {
        return RunStatus::NotStarted;
    };
    match &last.event {
        Event::RunStarted { .. }
        | Event::ModelCallCompleted { .. }
        | Event::ToolCallCompleted { .. }
        | Event::NowObserved { .. }
        | Event::RandomObserved { .. }
        | Event::Resumed { .. } => RunStatus::Running,
        Event::ModelCallRequested { .. } => RunStatus::AwaitingModel,
        Event::ToolCallRequested { effect, .. } => match effect {
            Effect::Write => RunStatus::NeedsReconciliation,
            Effect::Read | Effect::Idempotent => RunStatus::AwaitingTool,
        },
        Event::Suspended {
            reason,
            input_schema,
        } => RunStatus::Suspended {
            reason: reason.clone(),
            input_schema: input_schema.clone(),
        },
        Event::BudgetExceeded { budget, observed } => RunStatus::BudgetExceeded {
            budget: *budget,
            observed: *observed,
        },
        Event::RunCompleted { output } => RunStatus::Completed {
            output: output.clone(),
        },
        Event::RunFailed { error } => RunStatus::Failed {
            error: error.clone(),
        },
    }
}

/// Token totals recomputed independently of the fold.
fn expected_usage(prefix: &[EventEnvelope]) -> TokenTotals {
    let mut totals = TokenTotals::default();
    for env in prefix {
        if let Event::ModelCallCompleted { usage, .. } = &env.event {
            totals.input_tokens += u64::from(usage.input_tokens);
            totals.output_tokens += u64::from(usage.output_tokens);
        }
    }
    totals
}

/// The every-boundary property for one script: record the reference log,
/// then for every prefix length 0..=N check the fold and the continuation.
fn assert_every_boundary(script: &[Step]) {
    let reference = drive(script, &[]).expect("reference run succeeds");
    for cut in 0..=reference.log.len() {
        let prefix = &reference.log[..cut];

        // The fold is total and coherent over this prefix.
        let state = derive_state(prefix);
        assert_eq!(state.status, expected_status(prefix), "cut at {cut}");
        assert_eq!(state.next_seq, SequenceNumber::new(cut as u64));
        assert_eq!(state.usage, expected_usage(prefix), "cut at {cut}");

        // Replay-then-continue from this prefix.
        if ends_in_dangling_write(prefix) && cut < reference.log.len() {
            // The reference run completed this write, but a continuation
            // cannot know that: it must refuse to guess.
            let err = drive(script, prefix).expect_err("must need reconciliation");
            assert!(
                matches!(err, ReplayError::NeedsReconciliation { .. }),
                "cut at {cut}: expected reconciliation, got {err:?}"
            );
            assert_eq!(state.status, RunStatus::NeedsReconciliation);
            assert!(matches!(
                state.pending_call,
                Some(PendingCall::Tool {
                    effect: Effect::Write,
                    ..
                })
            ));
            continue;
        }

        let continued = drive(script, prefix).expect("continuation succeeds");
        assert_eq!(
            continued.log, reference.log,
            "cut at {cut}: continuation must reproduce the reference log"
        );
        assert_eq!(
            derive_state(&continued.log),
            derive_state(&reference.log),
            "cut at {cut}: continuation must reach the identical final state"
        );
        if cut == reference.log.len() {
            assert_eq!(continued.executions, 0, "a full replay executes nothing");
        }
    }
}

/// A completed run touching every event kind a completed run can contain:
/// both context observations, a model call, all three tool effect classes,
/// suspension and resume, and a runtime-interposed budget crossing.
fn completed_script() -> Vec<Step> {
    vec![
        Step::Begin {
            hash: "sha256:agent-v1",
            input: json!({"topic": "otters"}),
        },
        Step::Now {
            value: datetime!(2026-07-09 09:15:42.123456789 UTC),
        },
        Step::Random { value: u64::MAX },
        Step::Model {
            request_hash: "sha256:req-1",
            response: json!({"text": "otters hold hands while sleeping"}),
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 80,
            },
        },
        Step::Tool {
            tool: "search",
            input: json!({"q": "otters"}),
            effect: Effect::Read,
            key: None,
            output: Some(json!({"hits": 3})),
        },
        Step::Suspend {
            reason: "awaiting approval",
            schema: json!({"type": "object"}),
            resume: json!({"approved": true}),
        },
        Step::Budget {
            budget: Budget {
                kind: BudgetKind::CostUsd,
                limit: 2.0,
            },
            observed: 2.5,
            resume: json!({"extend_to": 5.0}),
        },
        Step::Tool {
            tool: "store",
            input: json!({"doc": "summary"}),
            effect: Effect::Idempotent,
            key: Some("key-1"),
            output: Some(json!({"stored": true})),
        },
        Step::Tool {
            tool: "create_ticket",
            input: json!({"title": "publish"}),
            effect: Effect::Write,
            key: None,
            output: Some(json!({"id": "TICKET-1"})),
        },
        Step::Complete {
            output: json!({"summary": "done"}),
        },
    ]
}

/// The write-intent crash: the run dies after persisting a write intent and
/// before the tool completes. Its final state is needs-reconciliation.
fn write_crash_script() -> Vec<Step> {
    vec![
        Step::Begin {
            hash: "sha256:agent-v1",
            input: json!({"topic": "otters"}),
        },
        Step::Now {
            value: datetime!(2026-07-09 09:15:42.123456789 UTC),
        },
        Step::Model {
            request_hash: "sha256:req-1",
            response: json!({"text": "draft"}),
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
            },
        },
        Step::Tool {
            tool: "charge_card",
            input: json!({"amount_cents": 1200}),
            effect: Effect::Write,
            key: None,
            output: None,
        },
    ]
}

/// A run that ends in failure.
fn failed_script() -> Vec<Step> {
    vec![
        Step::Begin {
            hash: "sha256:agent-v1",
            input: json!({"topic": "otters"}),
        },
        Step::Tool {
            tool: "search",
            input: json!({"q": "otters"}),
            effect: Effect::Read,
            key: None,
            output: Some(json!({"hits": 0})),
        },
        Step::Fail {
            error: "no sources found",
        },
    ]
}

/// Every prefix of the sixteen-event completed run folds coherently and
/// continues to the identical final log, except the one prefix ending at
/// the (later completed) write intent, which must demand reconciliation.
#[test]
fn every_boundary_of_a_completed_run() {
    let script = completed_script();
    let reference = drive(&script, &[]).expect("reference run succeeds");
    assert_eq!(reference.log.len(), 16);
    assert!(matches!(
        derive_state(&reference.log).status,
        RunStatus::Completed { .. }
    ));
    assert_every_boundary(&script);
}

/// Every prefix of the write-crash run folds coherently; every continuation
/// ends at the same dangling intent, and the final state is
/// needs-reconciliation with the intent surfaced as evidence.
#[test]
fn every_boundary_of_a_write_crash_run() {
    let script = write_crash_script();
    let reference = drive(&script, &[]).expect("reference run succeeds");
    assert_eq!(reference.log.len(), 5);
    let state = derive_state(&reference.log);
    assert_eq!(state.status, RunStatus::NeedsReconciliation);
    match &state.pending_call {
        Some(PendingCall::Tool { tool, effect, .. }) => {
            assert_eq!(tool, "charge_card");
            assert_eq!(*effect, Effect::Write);
        }
        other => panic!("expected the dangling write as pending call, got {other:?}"),
    }
    assert_every_boundary(&script);
}

/// Every prefix of the failed run folds coherently and continues to the
/// identical failed final state.
#[test]
fn every_boundary_of_a_failed_run() {
    let script = failed_script();
    let reference = drive(&script, &[]).expect("reference run succeeds");
    assert_eq!(reference.log.len(), 4);
    assert!(matches!(
        derive_state(&reference.log).status,
        RunStatus::Failed { .. }
    ));
    assert_every_boundary(&script);
}
