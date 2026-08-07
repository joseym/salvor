//! The kill-at-every-boundary property, exercised at the replay-engine
//! level: for every prefix of a reference log, the state fold yields a
//! coherent state, and replaying then continuing from that prefix reaches
//! the identical final state (in fact, the identical final log) as the
//! uninterrupted run.
//!
//! Four reference runs cover the log shapes a crash can leave behind: a
//! completed run touching every event kind, a run that dies between a write
//! intent and its completion, a run that ends in failure, and a graph run
//! whose fold node runs two passes and settles. The one deliberate exception
//! to "continuing reaches the same final state" is a prefix ending in a
//! dangling write intent: continuing from there must refuse with
//! needs-reconciliation rather than re-execute the write, and the test
//! asserts exactly that.

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
    /// A graph run's head, the counterpart of `Begin` for a log that opens
    /// with `GraphRunStarted`.
    BeginGraph {
        hash: &'static str,
        input: Value,
    },
    Enter {
        node: &'static str,
    },
    Exit {
        node: &'static str,
    },
    FoldStart {
        node: &'static str,
        index: u64,
    },
    FoldJoin {
        node: &'static str,
        index: u64,
    },
    FoldConverged {
        node: &'static str,
        winner_index: u64,
        reason: &'static str,
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
            Step::Begin { hash, input } => match cursor.begin(hash, None)? {
                Outcome::Replayed(recorded) => assert_eq!(&recorded, input),
                Outcome::Live(permit) => push!(permit.record(input.clone())),
            },
            Step::BeginGraph { hash, input } => match cursor.begin_graph(hash, None, None)? {
                Outcome::Replayed(recorded) => assert_eq!(&recorded, input),
                Outcome::Live(permit) => push!(permit.record(input.clone())),
            },
            Step::Enter { node } => match cursor.node_entered(node)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
            Step::Exit { node } => match cursor.node_exited(node)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
            Step::FoldStart { node, index } => match cursor.fold_iteration_started(node, *index)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
            Step::FoldJoin { node, index } => match cursor.fold_iteration_joined(node, *index)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
            },
            Step::FoldConverged {
                node,
                winner_index,
                reason,
            } => match cursor.fold_converged(node, *winner_index, reason)? {
                Outcome::Replayed(()) => {}
                Outcome::Live(emitted) => push!(emitted),
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
            } => match cursor.model_call(request_hash, None)? {
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
        // The graph events share the agent run's status vocabulary: the head
        // starts the run and the node/branch/map/fold markers change no status,
        // so a prefix ending at any of them reads as `Running`, exactly as the
        // fold derives. Only the graph fold script below emits any of them, but
        // the match stays exhaustive so a new event kind cannot slip past it.
        Event::RunStarted { .. }
        | Event::GraphRunStarted { .. }
        | Event::NodeEntered { .. }
        | Event::NodeExited { .. }
        | Event::NodeSkipped { .. }
        | Event::BranchTaken { .. }
        | Event::MapFannedOut { .. }
        | Event::MapIterationStarted { .. }
        | Event::MapIterationJoined { .. }
        | Event::FoldIterationStarted { .. }
        | Event::FoldIterationJoined { .. }
        | Event::FoldConverged { .. }
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
        // Operator-appended terminal. This agent-run property script never
        // emits it, but the match stays exhaustive so a new event kind cannot
        // slip past it; the fold derives the abandoned terminal carrying its
        // recorded reason and any unresolved-write evidence.
        Event::RunAbandoned {
            reason,
            unresolved_write,
        } => RunStatus::Abandoned {
            reason: reason.clone(),
            unresolved_write: unresolved_write.clone(),
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

/// A graph run whose one fold node runs two passes and settles on the second.
/// The passes run inline in this log, so every fold marker is a boundary a
/// crash can land on, and each one has to replay on the way back.
fn fold_script() -> Vec<Step> {
    vec![
        Step::BeginGraph {
            hash: "sha256:graph-v1",
            input: json!({"draft": "otters"}),
        },
        Step::Enter { node: "refine" },
        Step::FoldStart {
            node: "refine",
            index: 0,
        },
        Step::Model {
            request_hash: "sha256:pass-0",
            response: json!({"text": "first pass"}),
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 10,
            },
        },
        Step::FoldJoin {
            node: "refine",
            index: 0,
        },
        Step::FoldStart {
            node: "refine",
            index: 1,
        },
        Step::Model {
            request_hash: "sha256:pass-1",
            response: json!({"text": "second pass"}),
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 12,
            },
        },
        Step::FoldJoin {
            node: "refine",
            index: 1,
        },
        Step::FoldConverged {
            node: "refine",
            winner_index: 1,
            reason: "stop predicate fired",
        },
        Step::Exit { node: "refine" },
        Step::Complete {
            output: json!({"text": "second pass"}),
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

/// Every prefix of the fold run folds coherently and continues to the
/// identical final log: a kill between any two fold markers replays the
/// markers it already recorded and never re-records one.
#[test]
fn every_boundary_of_a_graph_fold_run() {
    let script = fold_script();
    let reference = drive(&script, &[]).expect("reference run succeeds");
    assert_eq!(reference.log.len(), 13);
    assert!(matches!(
        derive_state(&reference.log).status,
        RunStatus::Completed { .. }
    ));
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
