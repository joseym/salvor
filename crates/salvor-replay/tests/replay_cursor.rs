//! Integration tests for the replay cursor: one orchestration function is
//! driven twice, once live (recording a log) and once replaying that log,
//! and the replay must consume every recorded result without executing
//! anything. Non-execution is proven structurally: the replay drive is given
//! an executor that panics if any of its methods run.
//!
//! Also covered here: bit-identical deterministic-context values across a
//! full serialize/deserialize round trip of the log, the exact handoff point
//! from replay to live, and the typed divergence errors.

use std::collections::BTreeMap;

use salvor_replay::{
    DedupOrigin, Effect, Emitted, Event, EventEnvelope, LoggedStep, ModelReply, Outcome,
    PendingCall, ReplayCursor, ReplayError, RequestedStep, RunId, RunStatus, SequenceNumber,
    SuspensionKind, TokenUsage, derive_state,
};
use serde_json::{Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// The clock value the live run observes, chosen with all nine fractional
/// digits populated so precision loss anywhere would be caught.
const LIVE_NOW: OffsetDateTime = datetime!(2026-07-09 09:15:42.123456789 UTC);

/// The random bits the live run draws, chosen near the top of the `u64`
/// range so a lossy number representation (an `f64` anywhere on the path)
/// would be caught.
const LIVE_RANDOM: u64 = u64::MAX - 12_345;

const AGENT_HASH: &str = "sha256:agent-v1";

fn run_id() -> RunId {
    RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap())
}

/// A deterministic timestamp per log position, so re-driven runs produce
/// byte-identical envelopes.
fn ts(seq: SequenceNumber) -> OffsetDateTime {
    datetime!(2026-07-09 12:00:00 UTC) + Duration::seconds(i64::try_from(seq.get()).unwrap())
}

/// Wraps an emitted event in its envelope, standing in for the runtime's IO
/// edge (which owns run identity and the clock).
fn wrap(emitted: Emitted) -> EventEnvelope {
    EventEnvelope::new(run_id(), emitted.seq, ts(emitted.seq), emitted.event)
}

/// The executor seam the orchestration function calls through in live mode.
/// Implementations count or forbid executions; the cursor itself never sees
/// this trait.
trait Io {
    fn clock(&mut self) -> OffsetDateTime;
    fn rng(&mut self) -> u64;
    fn model(&mut self, request_hash: &str) -> (Value, TokenUsage);
    fn tool(&mut self, tool: &str, input: &Value) -> Value;
    fn resume_input(&mut self) -> Value;
}

/// Counts every execution and hands out the canned live values.
#[derive(Default)]
struct CountingIo {
    clocks: u32,
    rngs: u32,
    models: u32,
    tools: u32,
    resumes: u32,
}

impl Io for CountingIo {
    fn clock(&mut self) -> OffsetDateTime {
        self.clocks += 1;
        LIVE_NOW
    }
    fn rng(&mut self) -> u64 {
        self.rngs += 1;
        LIVE_RANDOM
    }
    fn model(&mut self, _request_hash: &str) -> (Value, TokenUsage) {
        self.models += 1;
        (
            json!({"text": "otters are semi-aquatic"}),
            TokenUsage {
                input_tokens: 120,
                output_tokens: 45,
            },
        )
    }
    fn tool(&mut self, tool: &str, _input: &Value) -> Value {
        self.tools += 1;
        match tool {
            "search" => json!({"hits": 3}),
            "store" => json!({"stored": true}),
            "create_ticket" => json!({"id": "TICKET-1"}),
            other => panic!("unexpected tool {other}"),
        }
    }
    fn resume_input(&mut self) -> Value {
        self.resumes += 1;
        json!({"approved": true})
    }
}

/// Panics on any execution. Handing this to a replay drive is the structural
/// proof that replay executes nothing: if any call reached the outside
/// world, the test would abort here.
struct PanicIo;

impl Io for PanicIo {
    fn clock(&mut self) -> OffsetDateTime {
        panic!("replay must not read the clock")
    }
    fn rng(&mut self) -> u64 {
        panic!("replay must not draw randomness")
    }
    fn model(&mut self, _request_hash: &str) -> (Value, TokenUsage) {
        panic!("replay must not call the model")
    }
    fn tool(&mut self, tool: &str, _input: &Value) -> Value {
        panic!("replay must not execute tool {tool}")
    }
    fn resume_input(&mut self) -> Value {
        panic!("replay must not wait for a resume")
    }
}

/// What one drive of the orchestration returned, for comparing the live and
/// replayed runs value by value.
#[derive(Debug, PartialEq)]
struct DriveResult {
    observed_now: OffsetDateTime,
    observed_random: u64,
    final_output: Value,
}

/// The reference orchestration: a deterministic function over the values the
/// cursor hands it. Every live result is appended to `sink` so the caller
/// accumulates the log the run produced. Fourteen events end to end.
fn research_run(
    cursor: &mut ReplayCursor,
    io: &mut dyn Io,
    sink: &mut Vec<EventEnvelope>,
) -> Result<DriveResult, ReplayError> {
    let input = match cursor.begin(AGENT_HASH, None)? {
        Outcome::Replayed(input) => input,
        Outcome::Live(permit) => {
            let input = json!({"topic": "otters"});
            sink.push(wrap(permit.record(input.clone())));
            input
        }
    };

    let observed_now = match cursor.now()? {
        Outcome::Replayed(now) => now,
        Outcome::Live(permit) => {
            let now = io.clock();
            sink.push(wrap(permit.record(now)));
            now
        }
    };

    let observed_random = match cursor.random()? {
        Outcome::Replayed(value) => value,
        Outcome::Live(permit) => {
            let value = io.rng();
            sink.push(wrap(permit.record(value)));
            value
        }
    };

    let reply = match cursor.model_call("sha256:req-1", None)? {
        Outcome::Replayed(reply) => reply,
        Outcome::Live(permit) => {
            if let Some(intent) = permit.intent() {
                sink.push(wrap(intent.clone()));
            }
            let (response, usage) = io.model("sha256:req-1");
            sink.push(wrap(permit.record(response.clone(), usage)));
            ModelReply { response, usage }
        }
    };

    let query = json!({"q": input["topic"]});
    let hits = match cursor.tool_call("search", &query, Effect::Read, None)? {
        Outcome::Replayed(output) => output,
        Outcome::Live(permit) => {
            if let Some(intent) = permit.intent() {
                sink.push(wrap(intent.clone()));
            }
            let output = io.tool("search", &query);
            sink.push(wrap(permit.record(output.clone())));
            output
        }
    };

    let schema = json!({"type": "object", "required": ["approved"]});
    match cursor.suspend("awaiting approval", &schema)? {
        Outcome::Replayed(()) => {}
        Outcome::Live(emitted) => sink.push(wrap(emitted)),
    }
    let approval = match cursor.await_resume()? {
        Outcome::Replayed(input) => input,
        Outcome::Live(parked) => {
            let input = io.resume_input();
            sink.push(wrap(parked.resume(input.clone())));
            input
        }
    };

    let doc = json!({"summary": reply.response["text"], "hits": hits["hits"]});
    match cursor.tool_call("store", &doc, Effect::Idempotent, Some("key-1"))? {
        Outcome::Replayed(_) => {}
        Outcome::Live(permit) => {
            if let Some(intent) = permit.intent() {
                sink.push(wrap(intent.clone()));
            }
            let output = io.tool("store", &doc);
            sink.push(wrap(permit.record(output)));
        }
    }

    let ticket = json!({"title": "publish research"});
    match cursor.tool_call("create_ticket", &ticket, Effect::Write, None)? {
        Outcome::Replayed(_) => {}
        Outcome::Live(permit) => {
            if let Some(intent) = permit.intent() {
                sink.push(wrap(intent.clone()));
            }
            let output = io.tool("create_ticket", &ticket);
            sink.push(wrap(permit.record(output)));
        }
    }

    // The final answer folds in the replayed context values, so a divergence
    // in either would also surface as an output mismatch at completion.
    let final_output = json!({
        "approved": approval["approved"],
        "random": observed_random,
        "unix_nanos_hi": (observed_now.unix_timestamp_nanos() >> 64) as i64,
        "unix_nanos_lo": (observed_now.unix_timestamp_nanos() as u64),
    });
    match cursor.complete_run(&final_output)? {
        Outcome::Replayed(()) => {}
        Outcome::Live(emitted) => sink.push(wrap(emitted)),
    }

    Ok(DriveResult {
        observed_now,
        observed_random,
        final_output,
    })
}

/// Runs the orchestration live from an empty log and returns what it
/// produced.
fn record_reference_run() -> (Vec<EventEnvelope>, DriveResult, CountingIo) {
    let mut cursor = ReplayCursor::new(Vec::new()).expect("empty log is valid");
    let mut io = CountingIo::default();
    let mut sink = Vec::new();
    let result = research_run(&mut cursor, &mut io, &mut sink).expect("live run succeeds");
    (sink, result, io)
}

/// Criterion: the replay consumes every recorded result without executing.
/// The live drive records fourteen events; the replay drive runs the same
/// orchestration over them with a panic-on-call executor and appends
/// nothing.
#[test]
fn replay_consumes_whole_log_without_executing() {
    let (log, live_result, io) = record_reference_run();
    assert_eq!(log.len(), 14, "the reference run records fourteen events");
    assert_eq!(
        (io.clocks, io.rngs, io.models, io.tools, io.resumes),
        (1, 1, 1, 3, 1),
        "the live run executed each step exactly once"
    );

    let mut cursor = ReplayCursor::new(log.clone()).expect("recorded log is valid");
    let mut sink = Vec::new();
    let replay_result =
        research_run(&mut cursor, &mut PanicIo, &mut sink).expect("replay succeeds");

    assert!(sink.is_empty(), "replay appended nothing to the log");
    assert!(!cursor.is_replaying(), "every recorded event was consumed");
    assert!(cursor.is_finished(), "the terminal event was consumed");
    assert_eq!(replay_result, live_result, "replay reproduced every value");
}

/// Criterion: recorded `now()`/`random()` values replay bit-identically,
/// even after the log takes a full trip through its JSON wire form.
#[test]
fn context_values_replay_bit_identical_through_the_wire() {
    let (log, _, _) = record_reference_run();

    // Round-trip every envelope through its serialized form, exactly as a
    // store would persist and reload it.
    let reloaded: Vec<EventEnvelope> = log
        .iter()
        .map(|env| {
            let json = serde_json::to_string(env).expect("serialize");
            serde_json::from_str(&json).expect("deserialize")
        })
        .collect();

    let mut cursor = ReplayCursor::new(reloaded).expect("reloaded log is valid");
    let mut sink = Vec::new();
    let result = research_run(&mut cursor, &mut PanicIo, &mut sink).expect("replay succeeds");

    assert_eq!(result.observed_now, LIVE_NOW, "now() replayed exactly");
    assert_eq!(
        result.observed_random, LIVE_RANDOM,
        "random() replayed exactly"
    );
}

/// Criterion: replay hands off to live mode at the exact first unrecorded
/// step. Cutting the log after the resume (nine events) leaves the
/// idempotent store, the write, and completion to run live: exactly two tool
/// executions and nothing else, and the continued log equals the reference.
#[test]
fn handoff_to_live_at_first_unrecorded_step() {
    let (log, live_result, _) = record_reference_run();
    let prefix: Vec<EventEnvelope> = log[..9].to_vec();

    let mut cursor = ReplayCursor::new(prefix.clone()).expect("prefix is valid");
    let mut io = CountingIo::default();
    let mut sink = Vec::new();
    let result = research_run(&mut cursor, &mut io, &mut sink).expect("resumed run succeeds");

    assert_eq!(
        (io.clocks, io.rngs, io.models, io.tools, io.resumes),
        (0, 0, 0, 2, 0),
        "only the two tool calls after the cut executed"
    );
    let continued: Vec<EventEnvelope> = prefix.into_iter().chain(sink).collect();
    assert_eq!(
        continued, log,
        "continuation reproduced the reference log exactly"
    );
    assert_eq!(result, live_result);
    assert_eq!(derive_state(&continued), derive_state(&log));
}

/// Divergence: requesting a different kind of operation than the log
/// recorded at that position fails with the position and both sides.
#[test]
fn divergence_on_kind_mismatch() {
    let (log, _, _) = record_reference_run();
    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    let Outcome::Replayed(_) = cursor.begin(AGENT_HASH, None).expect("begin replays") else {
        panic!("begin must replay");
    };

    // Position 1 recorded NowObserved; orchestration asks for randomness.
    let err = cursor.random().expect_err("kind mismatch must diverge");
    assert_eq!(
        err,
        ReplayError::Divergence {
            position: SequenceNumber::new(1),
            recorded: Box::new(LoggedStep::Event(Event::NowObserved { now: LIVE_NOW })),
            requested: Box::new(RequestedStep::Random),
        }
    );
}

/// Divergence: the right kind of operation with the wrong payload (here, a
/// different model request hash) also fails, carrying both hashes.
#[test]
fn divergence_on_payload_mismatch() {
    let (log, _, _) = record_reference_run();
    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    cursor.now().expect("now replays");
    cursor.random().expect("random replays");

    let err = cursor
        .model_call("sha256:req-DIFFERENT", None)
        .expect_err("payload mismatch must diverge");
    assert_eq!(
        err,
        ReplayError::Divergence {
            position: SequenceNumber::new(3),
            recorded: Box::new(LoggedStep::Event(Event::ModelCallRequested {
                seq: SequenceNumber::new(3),
                request_hash: "sha256:req-1".into(),
                request_body: None,
            })),
            requested: Box::new(RequestedStep::ModelCall {
                request_hash: "sha256:req-DIFFERENT".into(),
            }),
        }
    );
}

/// Divergence: the log ended at a terminal event but orchestration keeps
/// producing steps. Flipping to live here would execute steps the completed
/// run never took, so the cursor refuses with a typed error instead.
#[test]
fn divergence_when_orchestration_outruns_a_terminal_log() {
    let (log, live_result, _) = record_reference_run();
    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    let mut sink = Vec::new();
    research_run(&mut cursor, &mut PanicIo, &mut sink).expect("replay succeeds");

    let err = cursor
        .now()
        .expect_err("a step after the recorded end must diverge");
    assert_eq!(
        err,
        ReplayError::Divergence {
            position: SequenceNumber::new(14),
            recorded: Box::new(LoggedStep::RunAlreadyTerminal(Event::RunCompleted {
                output: live_result.final_output,
            })),
            requested: Box::new(RequestedStep::Now),
        }
    );
}

/// Divergence: orchestration finishing while recorded history remains is the
/// mirror image, and surfaces as a mismatch at the next recorded event.
#[test]
fn divergence_when_orchestration_ends_before_the_log() {
    let (log, _, _) = record_reference_run();
    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    cursor.now().expect("now replays");
    cursor.random().expect("random replays");

    let err = cursor
        .complete_run(&json!({"early": true}))
        .expect_err("finishing early must diverge");
    match err {
        ReplayError::Divergence {
            position,
            recorded,
            requested,
        } if matches!(
            *recorded,
            LoggedStep::Event(Event::ModelCallRequested { .. })
        ) && matches!(*requested, RequestedStep::CompleteRun { .. }) =>
        {
            assert_eq!(position, SequenceNumber::new(3));
        }
        other => panic!("expected divergence against the recorded model intent, got {other:?}"),
    }
}

/// A dangling write intent refuses to replay or retry: the typed
/// needs-reconciliation error carries the recorded evidence.
#[test]
fn dangling_write_intent_needs_reconciliation() {
    let base = datetime!(2026-07-09 12:00:00 UTC);
    let ticket = json!({"title": "bug"});
    let log = vec![
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(0),
            base,
            Event::RunStarted {
                agent_def_hash: AGENT_HASH.into(),
                input: json!({}),
                labels: None,
            },
        ),
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(1),
            base,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: ticket.clone(),
                effect: Effect::Write,
                idempotency_key: None,
                performed_by: None,
            },
        ),
    ];
    assert_eq!(derive_state(&log).status, RunStatus::NeedsReconciliation);

    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let err = cursor
        .tool_call("create_ticket", &ticket, Effect::Write, None)
        .expect_err("a dangling write must not re-execute");
    assert_eq!(
        err,
        ReplayError::NeedsReconciliation {
            position: SequenceNumber::new(1),
            tool: "create_ticket".into(),
            input: ticket,
            idempotency_key: None,
        }
    );
}

/// A dangling idempotent intent re-executes live under its recorded key: the
/// permit surfaces the key, reports the intent as already recorded, and the
/// fresh completion correlates back to the recorded intent.
#[test]
fn dangling_idempotent_intent_retries_under_recorded_key() {
    let base = datetime!(2026-07-09 12:00:00 UTC);
    let doc = json!({"doc": 1});
    let log = vec![
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(0),
            base,
            Event::RunStarted {
                agent_def_hash: AGENT_HASH.into(),
                input: json!({}),
                labels: None,
            },
        ),
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(1),
            base,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "store".into(),
                input: doc.clone(),
                effect: Effect::Idempotent,
                idempotency_key: Some("key-9".into()),
                performed_by: None,
            },
        ),
    ];

    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let Outcome::Live(permit) = cursor
        .tool_call("store", &doc, Effect::Idempotent, Some("key-9"))
        .expect("a dangling idempotent intent goes live")
    else {
        panic!("expected a live permit");
    };
    assert!(permit.intent().is_none(), "the intent is already recorded");
    assert_eq!(permit.idempotency_key(), Some("key-9"));
    assert_eq!(permit.seq(), SequenceNumber::new(1));

    let emitted = permit.record(json!({"stored": true}));
    assert_eq!(emitted.seq, SequenceNumber::new(2));
    assert_eq!(
        emitted.event,
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"stored": true}),
            deduplicated_from: None,
        }
    );
}

/// A `request_body` recorded on `ModelCallRequested` is inert on replay. A log
/// that carries the full request body folds to the identical derived state and
/// replays the identical model reply as the same log without a body: the
/// cursor correlates on the hash alone and ignores the body. This is the
/// determinism guarantee that a log captured with bodies (recording on) and
/// one captured without (recording off) replay to the same states and
/// completions, with no divergence.
#[test]
fn recorded_request_body_does_not_change_replay() {
    // Builds a four-event log for the same run, differing only in whether the
    // model intent carries a request body.
    fn build_log(body: Option<Value>) -> Vec<EventEnvelope> {
        let events = vec![
            Event::RunStarted {
                agent_def_hash: AGENT_HASH.into(),
                input: json!({"topic": "otters"}),
                labels: None,
            },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:req-1".into(),
                request_body: body,
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(1),
                response: json!({"text": "otters are semi-aquatic"}),
                usage: TokenUsage {
                    input_tokens: 120,
                    output_tokens: 45,
                },
            },
            Event::RunCompleted {
                output: json!({"text": "otters are semi-aquatic"}),
            },
        ];
        events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                let seq = SequenceNumber::new(u64::try_from(index).unwrap());
                EventEnvelope::new(run_id(), seq, ts(seq), event)
            })
            .collect()
    }

    // Folds the log and replays every step, returning the observable results.
    // Nothing executes: each request is answered from the log.
    fn replay_all(log: Vec<EventEnvelope>) -> (RunStatus, Value, ModelReply) {
        let status = derive_state(&log).status;
        let mut cursor = ReplayCursor::new(log).expect("log is valid");
        let Outcome::Replayed(input) = cursor.begin(AGENT_HASH, None).expect("begin replays")
        else {
            panic!("begin should replay from a recorded log");
        };
        // Pass no body here on purpose: replay ignores the argument, so a log
        // recorded with a body still matches a caller that supplies none.
        let Outcome::Replayed(reply) = cursor
            .model_call("sha256:req-1", None)
            .expect("model_call replays")
        else {
            panic!("model_call should replay from a recorded log");
        };
        let output = json!({"text": "otters are semi-aquatic"});
        cursor.complete_run(&output).expect("complete_run replays");
        (status, input, reply)
    }

    let with_body = replay_all(build_log(Some(json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "otters"}]
    }))));
    let without_body = replay_all(build_log(None));

    assert_eq!(
        with_body.0, without_body.0,
        "derived status must be identical"
    );
    assert_eq!(
        with_body.1, without_body.1,
        "replayed run input must be identical"
    );
    assert_eq!(
        with_body.2, without_body.2,
        "replayed model reply must be identical"
    );
}

/// Labels recorded on a live `begin` land on the `RunStarted` event exactly as
/// given, and a replayed log preserves them by construction: they live in the
/// recorded event itself, so folding the log back (the same mechanism a
/// recovered or re-driven run uses) reads the identical map back, with no
/// separate carry-through step to get wrong. This also proves the divergence
/// check on `begin` never involves labels: a replaying caller who passes
/// `None` (or a different map) still replays cleanly, matching
/// `recorded_request_body_does_not_change_replay`'s proof for `request_body`.
#[test]
fn labels_recorded_live_survive_a_replayed_log() {
    let mut cursor = ReplayCursor::new(Vec::new()).expect("empty log is valid");
    let labels = BTreeMap::from([
        ("build".to_owned(), "42".to_owned()),
        ("env".to_owned(), "prod".to_owned()),
    ]);
    let Outcome::Live(permit) = cursor
        .begin(AGENT_HASH, Some(labels.clone()))
        .expect("begin")
    else {
        panic!("an empty log must hand back a live begin permit");
    };
    let emitted = permit.record(json!({"topic": "otters"}));
    let Event::RunStarted {
        labels: recorded, ..
    } = &emitted.event
    else {
        panic!("begin must record a RunStarted event");
    };
    assert_eq!(recorded, &Some(labels.clone()), "labels land on the event");

    // Round-trip the event through the wire form, the way a store would, then
    // fold it into a fresh log and replay `begin` against it. The recorded
    // labels come back unchanged, and replay needs no `labels` argument of its
    // own to see them (it is passed `None` here on purpose, mirroring how
    // `recorded_request_body_does_not_change_replay` replays with no body).
    let envelope = EventEnvelope::new(run_id(), emitted.seq, ts(emitted.seq), emitted.event);
    let wire = serde_json::to_string(&envelope).expect("serialize");
    assert!(
        wire.contains(r#""labels":{"build":"42","env":"prod"}"#),
        "labels ride the wire form: {wire}"
    );
    let restored: EventEnvelope = serde_json::from_str(&wire).expect("deserialize");

    // What replay reads is exactly `restored` (the cursor consumes the log it
    // was built over unchanged), so asserting on it directly is asserting on
    // what a replayed `begin` sees.
    let Event::RunStarted {
        labels: replayed_labels,
        ..
    } = &restored.event
    else {
        panic!("position 0 must be RunStarted");
    };
    assert_eq!(
        replayed_labels,
        &Some(labels.clone()),
        "the round-tripped envelope preserves the recorded labels unchanged"
    );

    // And `begin` itself still replays cleanly against this log: the
    // divergence check matches on `agent_def_hash` alone, so passing `None`
    // here (a different `labels` argument than what was recorded) is not a
    // mismatch, exactly as `recorded_request_body_does_not_change_replay`
    // proves for `request_body`.
    let mut replay_cursor = ReplayCursor::new(vec![restored]).expect("log is valid");
    let Outcome::Replayed(_) = replay_cursor
        .begin(AGENT_HASH, None)
        .expect("begin replays")
    else {
        panic!("begin should replay from a recorded log");
    };
}

/// The wake instant the sleeping tests park on, with all nine fractional
/// digits populated so precision loss anywhere on the wire would be caught.
const WAKE_AT: OffsetDateTime = datetime!(2026-07-16 09:15:42.123456789 UTC);

/// A one-event log holding just the run head, the base every sleep test builds
/// its own tail onto.
fn head_only_log() -> Vec<EventEnvelope> {
    vec![EventEnvelope::new(
        run_id(),
        SequenceNumber::new(0),
        ts(SequenceNumber::new(0)),
        Event::RunStarted {
            agent_def_hash: AGENT_HASH.into(),
            input: json!({"topic": "otters"}),
            labels: None,
        },
    )]
}

/// A durable timer recorded live replays from the log without executing
/// anything: the start matches on its recorded instant and the wake is read
/// back, so a resumed run never re-parks on a timer it already served.
#[test]
fn a_recorded_sleep_replays_from_the_log() {
    let mut cursor = ReplayCursor::new(head_only_log()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");

    let Outcome::Live(emitted) = cursor.sleep_started(WAKE_AT).expect("sleep starts") else {
        panic!("an exhausted log must hand back a live emission");
    };
    assert_eq!(emitted.seq, SequenceNumber::new(1));
    assert_eq!(emitted.event, Event::SleepStarted { wake_at: WAKE_AT });
    let started = wrap(emitted);

    let Outcome::Live(asleep) = cursor.sleep_completed().expect("sleep is asked about") else {
        panic!("an unrecorded wake must park the run");
    };
    let woke = wrap(asleep.wake());
    assert_eq!(woke.event, Event::SleepCompleted {});

    // Round-trip the recorded pair through the wire form, exactly as a store
    // would, then replay the same two requests over it.
    let log: Vec<EventEnvelope> = [head_only_log(), vec![started, woke]]
        .concat()
        .iter()
        .map(|env| {
            let wire = serde_json::to_string(env).expect("serialize");
            serde_json::from_str(&wire).expect("deserialize")
        })
        .collect();
    let mut cursor = ReplayCursor::new(log).expect("recorded log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let Outcome::Replayed(()) = cursor.sleep_started(WAKE_AT).expect("sleep start replays") else {
        panic!("the recorded sleep must replay");
    };
    let Outcome::Replayed(()) = cursor.sleep_completed().expect("wake replays") else {
        panic!("the recorded wake must replay");
    };
    assert!(!cursor.is_replaying(), "every recorded event was consumed");
}

/// A recomputed wake instant that differs from the recorded one is a
/// divergence, not a detail to paper over: the caller derives the instant from
/// recorded data, so a different one means the derivation changed and the run
/// would continue under a deadline that is not the one on disk.
#[test]
fn a_mismatched_wake_instant_diverges() {
    let mut log = head_only_log();
    log.push(EventEnvelope::new(
        run_id(),
        SequenceNumber::new(1),
        ts(SequenceNumber::new(1)),
        Event::SleepStarted { wake_at: WAKE_AT },
    ));
    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");

    let drifted = WAKE_AT + Duration::nanoseconds(1);
    let err = cursor
        .sleep_started(drifted)
        .expect_err("a different wake instant must not replay");
    let ReplayError::Divergence {
        position,
        recorded,
        requested,
    } = err
    else {
        panic!("expected a divergence");
    };
    assert_eq!(position, SequenceNumber::new(1));
    assert_eq!(
        *recorded,
        LoggedStep::Event(Event::SleepStarted { wake_at: WAKE_AT })
    );
    assert_eq!(*requested, RequestedStep::SleepStarted { wake_at: drifted });
}

/// The park-versus-continue distinction, which is the whole reason
/// `sleep_completed` returns an outcome rather than nothing: a log that stops
/// at the started sleep parks the run (and the fold agrees it is sleeping),
/// while the same log with the wake recorded carries on. The cursor decides
/// this by reading the log, never by comparing the deadline against a clock it
/// does not have: the run below is asked about its sleep with the recorded
/// wake instant still in the future, and still continues, because what it asks
/// is whether the wake was recorded.
#[test]
fn an_unrecorded_wake_parks_and_a_recorded_one_continues() {
    let mut parked_log = head_only_log();
    parked_log.push(EventEnvelope::new(
        run_id(),
        SequenceNumber::new(1),
        ts(SequenceNumber::new(1)),
        Event::SleepStarted { wake_at: WAKE_AT },
    ));

    let mut cursor = ReplayCursor::new(parked_log.clone()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    cursor.sleep_started(WAKE_AT).expect("sleep start replays");
    // Never redeeming the value is how a driver leaves the run parked: the log
    // is unchanged and the fold reads the run as sleeping.
    let Outcome::Live(_) = cursor.sleep_completed().expect("sleep is asked about") else {
        panic!("an unrecorded wake must park the run, not continue it");
    };
    assert_eq!(
        derive_state(&parked_log).status,
        RunStatus::Sleeping { wake_at: WAKE_AT }
    );

    let mut woken_log = parked_log;
    woken_log.push(EventEnvelope::new(
        run_id(),
        SequenceNumber::new(2),
        ts(SequenceNumber::new(2)),
        Event::SleepCompleted {},
    ));
    let mut cursor = ReplayCursor::new(woken_log.clone()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    cursor.sleep_started(WAKE_AT).expect("sleep start replays");
    let Outcome::Replayed(()) = cursor.sleep_completed().expect("wake replays") else {
        panic!("a recorded wake must continue the run");
    };
    assert_eq!(derive_state(&woken_log).status, RunStatus::Running);
}

/// The cursor can be asked whether it is standing in front of a dangling
/// intent, without taking a step. That look-before-you-leap is what lets the
/// IO edge consult the outside world before committing to `tool_call`, which
/// either advances or fails with nothing in between.
#[test]
fn dangling_intent_reports_the_gap_without_advancing() {
    let base = datetime!(2026-07-09 12:00:00 UTC);
    let payout = json!({"claim_id": "wreck-9931"});
    let intent = |seq: u64| {
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(seq),
            base,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(seq),
                tool: "pay_claim".into(),
                input: payout.clone(),
                effect: Effect::Write,
                idempotency_key: Some("pay_claim:wreck-9931".into()),
                performed_by: None,
            },
        )
    };
    let started = EventEnvelope::new(
        run_id(),
        SequenceNumber::new(0),
        base,
        Event::RunStarted {
            agent_def_hash: AGENT_HASH.into(),
            input: json!({}),
            labels: None,
        },
    );

    let mut cursor = ReplayCursor::new(vec![started.clone(), intent(1)]).expect("log is valid");
    assert_eq!(cursor.dangling_intent(), None, "not there yet");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let Some(PendingCall::Tool {
        seq,
        tool,
        idempotency_key,
        ..
    }) = cursor.dangling_intent()
    else {
        panic!("expected a dangling tool intent");
    };
    assert_eq!(seq, SequenceNumber::new(1));
    assert_eq!(tool, "pay_claim");
    assert_eq!(idempotency_key.as_deref(), Some("pay_claim:wreck-9931"));
    // Looking must not have moved the cursor: the same look answers the same.
    assert!(cursor.dangling_intent().is_some());

    // An intent with a completion behind it is not dangling.
    let completed = EventEnvelope::new(
        run_id(),
        SequenceNumber::new(2),
        base,
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: None,
        },
    );
    let mut cursor = ReplayCursor::new(vec![started, intent(1), completed]).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    assert_eq!(cursor.dangling_intent(), None);
}

/// A dangling write intent the caller has proven did not execute is finished as
/// the copy it was: the permit reports the intent as already recorded, and the
/// completion carries the origin it copied.
///
/// The cursor never checks the proof, because it cannot: whether an intent
/// executed is a fact about the store and the world. What it does enforce is
/// that the position really holds this call's dangling intent, so the escape
/// cannot be pointed at anything else.
#[test]
fn a_proven_unexecuted_intent_records_a_deduplicated_completion() {
    let base = datetime!(2026-07-09 12:00:00 UTC);
    let payout = json!({"claim_id": "wreck-9931"});
    let log = vec![
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(0),
            base,
            Event::RunStarted {
                agent_def_hash: AGENT_HASH.into(),
                input: json!({}),
                labels: None,
            },
        ),
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(1),
            base,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "pay_claim".into(),
                input: payout.clone(),
                effect: Effect::Write,
                idempotency_key: Some("pay_claim:wreck-9931".into()),
                performed_by: None,
            },
        ),
    ];

    // The ordinary path still refuses it, which is the behavior every other
    // dangling write keeps.
    let mut cursor = ReplayCursor::new(log.clone()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    assert!(matches!(
        cursor.tool_call(
            "pay_claim",
            &payout,
            Effect::Write,
            Some("pay_claim:wreck-9931")
        ),
        Err(ReplayError::NeedsReconciliation { .. })
    ));

    let mut cursor = ReplayCursor::new(log).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    // A different call cannot borrow the escape.
    assert!(
        cursor
            .resume_unexecuted_tool_call(
                "pay_claim",
                &json!({"claim_id": "someone-else"}),
                Effect::Write,
                "pay_claim:wreck-9931"
            )
            .is_err(),
        "the escape must be pinned to the recorded intent"
    );

    let origin = DedupOrigin {
        run_id: RunId::from_uuid(
            Uuid::parse_str("00000000-0000-4000-8000-0000000000aa").expect("uuid"),
        ),
        seq: SequenceNumber::new(4),
    };
    let permit = cursor
        .resume_unexecuted_tool_call("pay_claim", &payout, Effect::Write, "pay_claim:wreck-9931")
        .expect("the proven-unexecuted intent goes live");
    assert!(permit.intent().is_none(), "the intent is already recorded");
    assert_eq!(permit.seq(), SequenceNumber::new(1));

    let emitted = permit.record_deduplicated(json!({"charge_id": "po_1"}), origin);
    assert_eq!(emitted.seq, SequenceNumber::new(2));
    let Event::ToolCallCompleted {
        seq,
        output,
        deduplicated_from,
    } = emitted.event
    else {
        panic!("expected a tool completion");
    };
    assert_eq!(seq, SequenceNumber::new(1), "it correlates to the intent");
    assert_eq!(output, json!({"charge_id": "po_1"}));
    assert_eq!(
        deduplicated_from,
        Some(origin),
        "the completion must name what it copied"
    );
}

/// The schema a signal's payload is validated against, shared by the tests
/// below so a divergence between them can only come from the discriminator.
fn signal_schema() -> Value {
    json!({"type": "object", "required": ["tracking_number"]})
}

/// A suspension recorded as a signal wait round-trips the wire and replays,
/// and the discriminator survives with it: an external wait resumed through
/// the same `Resumed` event a gate uses, still knowing it was never a gate.
#[test]
fn a_signal_suspension_round_trips_and_replays() {
    let mut cursor = ReplayCursor::new(head_only_log()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");

    let reason = "awaiting the carrier webhook";
    let Outcome::Live(emitted) = cursor
        .suspend_for_signal(reason, &signal_schema())
        .expect("the signal wait is recorded")
    else {
        panic!("an exhausted log must hand back a live emission");
    };
    assert_eq!(
        emitted.event,
        Event::Suspended {
            reason: reason.to_owned(),
            input_schema: signal_schema(),
            kind: Some(SuspensionKind::Signal),
        }
    );
    let suspended = wrap(emitted);

    let Outcome::Live(parked) = cursor.await_resume().expect("the wait is asked about") else {
        panic!("an unrecorded resume must park the run");
    };
    let payload = json!({"tracking_number": "1Z999"});
    let resumed = wrap(parked.resume(payload.clone()));

    // Through the wire form, exactly as a store would carry it, then replayed.
    let log: Vec<EventEnvelope> = [head_only_log(), vec![suspended, resumed]]
        .concat()
        .iter()
        .map(|env| {
            let wire = serde_json::to_string(env).expect("serialize");
            serde_json::from_str(&wire).expect("deserialize")
        })
        .collect();
    let mut cursor = ReplayCursor::new(log).expect("recorded log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let Outcome::Replayed(()) = cursor
        .suspend_for_signal(reason, &signal_schema())
        .expect("the signal wait replays")
    else {
        panic!("the recorded suspension must replay");
    };
    let Outcome::Replayed(input) = cursor.await_resume().expect("the payload replays") else {
        panic!("the recorded resume must replay");
    };
    assert_eq!(input, payload);
    assert!(!cursor.is_replaying(), "every recorded event was consumed");
}

/// A suspension recorded as a gate replays as one, unchanged: the existing
/// call, the existing events, no discriminator anywhere. This is the other
/// half of the compatibility claim, checked through the cursor rather than
/// through the wire form.
#[test]
fn a_gate_suspension_replays_unchanged() {
    let mut cursor = ReplayCursor::new(head_only_log()).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let schema = json!({"type": "object", "required": ["approved"]});

    let Outcome::Live(emitted) = cursor
        .suspend("awaiting approval", &schema)
        .expect("the gate is recorded")
    else {
        panic!("an exhausted log must hand back a live emission");
    };
    assert_eq!(
        emitted.event,
        Event::Suspended {
            reason: "awaiting approval".to_owned(),
            input_schema: schema.clone(),
            kind: None,
        },
        "a gate records no discriminator at all"
    );

    let log = [head_only_log(), vec![wrap(emitted)]].concat();
    let mut cursor = ReplayCursor::new(log).expect("recorded log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let Outcome::Replayed(()) = cursor
        .suspend("awaiting approval", &schema)
        .expect("the gate replays")
    else {
        panic!("the recorded suspension must replay");
    };
}

/// A recomputed suspension that changed what it waits on is a divergence, not
/// a detail: the run would go on expecting an answer from a party the log says
/// was never asked. Both directions fail, gate against signal and signal
/// against gate, with reason and schema identical either way so nothing but
/// the discriminator can be what fails.
#[test]
fn a_suspension_whose_kind_changed_diverges() {
    let reason = "awaiting the carrier webhook";
    let recorded = |kind| {
        let mut log = head_only_log();
        log.push(EventEnvelope::new(
            run_id(),
            SequenceNumber::new(1),
            ts(SequenceNumber::new(1)),
            Event::Suspended {
                reason: reason.to_owned(),
                input_schema: signal_schema(),
                kind,
            },
        ));
        log
    };

    // Recorded as a signal, recomputed as a gate.
    let mut cursor =
        ReplayCursor::new(recorded(Some(SuspensionKind::Signal))).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let err = cursor
        .suspend(reason, &signal_schema())
        .expect_err("a gate must not replay a recorded signal wait");
    let ReplayError::Divergence {
        position,
        recorded: logged,
        requested,
    } = err
    else {
        panic!("expected a divergence");
    };
    assert_eq!(position, SequenceNumber::new(1));
    assert_eq!(
        *logged,
        LoggedStep::Event(Event::Suspended {
            reason: reason.to_owned(),
            input_schema: signal_schema(),
            kind: Some(SuspensionKind::Signal),
        })
    );
    assert_eq!(
        *requested,
        RequestedStep::Suspend {
            reason: reason.to_owned(),
            input_schema: signal_schema(),
            kind: None,
        }
    );

    // Recorded as a gate, recomputed as a signal.
    let mut cursor = ReplayCursor::new(recorded(None)).expect("log is valid");
    cursor.begin(AGENT_HASH, None).expect("begin replays");
    let err = cursor
        .suspend_for_signal(reason, &signal_schema())
        .expect_err("a signal wait must not replay a recorded gate");
    assert!(matches!(err, ReplayError::Divergence { .. }), "{err}");

    // The message names both sides, which is the whole point of carrying the
    // discriminator into the divergence report: two suspensions sharing a
    // reason would otherwise print identically.
    let message = err.to_string();
    assert!(message.contains("kind=gate"), "{message}");
    assert!(message.contains("kind=signal"), "{message}");
}
