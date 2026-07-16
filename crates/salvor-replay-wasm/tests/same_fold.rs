//! The same-fold proof, native side — and the generator for its fixtures.
//!
//! One test file with two jobs, so the reference logs are defined once:
//!
//! - `regenerate` (only when `REGEN_FIXTURES=1`) writes `fixtures/logs/*.json`
//!   (the wire-JSON inputs the wasm module folds) and `fixtures/expected/*.json`
//!   (the native fold at every prefix). Run it to (re)commit the fixtures:
//!   `REGEN_FIXTURES=1 cargo test -p salvor-replay-wasm --test same_fold -- --ignored regenerate`.
//!
//! - `committed_fixtures_match_native_fold` (the real test, always on) rebuilds
//!   the reference logs, asserts the committed `fixtures/logs` still equal them
//!   (the inputs cannot silently drift), then folds every committed log at every
//!   prefix natively and asserts the result equals `fixtures/expected` byte for
//!   byte. This keeps the committed native side live-verified on every
//!   `cargo test`, so the Node harness (`js/same-fold.mjs`) — which asserts the
//!   *wasm* fold equals that same committed expected — completes the chain:
//!   native == committed == wasm, all three checked live.
//!
//! The reference logs deliberately touch every event kind and every derived
//! status, including the f64 budget paths and the u64 random/sequence paths, so
//! the wasm-vs-native comparison exercises the cross-target number formatting
//! that is the real risk.

use std::fs;
use std::path::PathBuf;

use salvor_replay::{
    Budget, BudgetKind, Effect, Event, EventEnvelope, RunId, SequenceNumber, TokenUsage,
};
use salvor_replay_wasm::fold_prefix_to_json;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

fn run_id() -> RunId {
    RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-0000000000aa").unwrap())
}

fn recorded_at() -> OffsetDateTime {
    // A fixed instant so the fixtures are byte-stable across regenerations.
    OffsetDateTime::from_unix_timestamp(1_752_566_400).unwrap()
}

fn log(events: Vec<Event>) -> Vec<EventEnvelope> {
    events
        .into_iter()
        .enumerate()
        .map(|(i, event)| {
            EventEnvelope::new(
                run_id(),
                SequenceNumber::new(i as u64),
                recorded_at(),
                event,
            )
        })
        .collect()
}

fn started() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:agent-def".into(),
        input: json!({"topic": "otters", "nested": {"z": 1, "a": 2}}),
    }
}

/// The named reference logs. Each becomes one `logs/<name>.json` +
/// `expected/<name>.json` fixture pair.
fn reference_logs() -> Vec<(&'static str, Vec<EventEnvelope>)> {
    let mut logs: Vec<(&'static str, Vec<EventEnvelope>)> = Vec::new();

    logs.push(("empty", log(vec![])));
    logs.push(("running", log(vec![started()])));

    logs.push((
        "awaiting_model",
        log(vec![
            started(),
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:req-1".into(),
                request_body: None,
            },
        ]),
    ));

    logs.push((
        "awaiting_read",
        log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "search".into(),
                input: json!({"q": "otters"}),
                effect: Effect::Read,
                idempotency_key: None,
            },
        ]),
    ));

    logs.push((
        "awaiting_idempotent",
        log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "store".into(),
                input: json!({"doc": 7}),
                effect: Effect::Idempotent,
                idempotency_key: Some("key-7".into()),
            },
        ]),
    ));

    logs.push((
        "needs_reconciliation",
        log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: json!({"title": "bug"}),
                effect: Effect::Write,
                idempotency_key: None,
            },
        ]),
    ));

    logs.push((
        "completed_write",
        log(vec![
            started(),
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "create_ticket".into(),
                input: json!({"title": "bug"}),
                effect: Effect::Write,
                idempotency_key: Some("idem-1".into()),
            },
            Event::ToolCallCompleted {
                seq: SequenceNumber::new(1),
                output: json!({"id": "TICKET-1"}),
            },
        ]),
    ));

    logs.push((
        "suspend_resume",
        log(vec![
            started(),
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: json!({"type": "object", "required": ["approved"]}),
            },
            Event::Resumed {
                input: json!({"approved": true}),
            },
        ]),
    ));

    logs.push((
        "budget_exceeded",
        log(vec![
            started(),
            Event::BudgetExceeded {
                budget: Budget {
                    kind: BudgetKind::CostUsd,
                    limit: 2.5,
                },
                observed: 2.500001,
            },
        ]),
    ));

    logs.push((
        "context_and_usage",
        log(vec![
            started(),
            Event::NowObserved {
                now: OffsetDateTime::from_unix_timestamp_nanos(1_752_566_400_123_456_789).unwrap(),
            },
            Event::RandomObserved { value: u64::MAX },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(3),
                request_hash: "sha256:req-a".into(),
                request_body: Some(json!({"model": "test", "messages": []})),
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(3),
                response: json!({"text": "one"}),
                usage: TokenUsage {
                    input_tokens: u32::MAX,
                    output_tokens: 40,
                },
            },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(5),
                request_hash: "sha256:req-b".into(),
                request_body: None,
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(5),
                response: json!({"text": "two"}),
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 2,
                },
            },
        ]),
    ));

    logs.push((
        "completed",
        log(vec![
            started(),
            Event::RunCompleted {
                output: json!({"summary": "done", "count": 3}),
            },
        ]),
    ));

    logs.push((
        "failed",
        log(vec![
            started(),
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash: "sha256:req".into(),
                request_body: None,
            },
            Event::RunFailed {
                error: "provider timeout".into(),
            },
        ]),
    ));

    // A ~1k-event synthetic run for scale (repeated model round trips): usage
    // accumulates far past one call, and the log is large enough to make the
    // scrub-latency measurement meaningful.
    let mut big = vec![started()];
    let mut seq = 1u64;
    while big.len() < 1000 {
        big.push(Event::ModelCallRequested {
            seq: SequenceNumber::new(seq),
            request_hash: format!("sha256:req-{seq}"),
            request_body: None,
        });
        let req_seq = seq;
        seq += 1;
        big.push(Event::ModelCallCompleted {
            seq: SequenceNumber::new(req_seq),
            response: json!({"text": format!("turn {req_seq}")}),
            usage: TokenUsage {
                input_tokens: 123,
                output_tokens: 45,
            },
        });
        seq += 1;
    }
    big.push(Event::RunCompleted {
        output: json!({"summary": "large run done"}),
    });
    logs.push(("large_1k", log(big)));

    logs
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The native fold at every prefix `0..=len`, as the exact canonical JSON
/// strings `fold_prefix_to_json` emits. These are the bytes that cross the wasm
/// boundary, so the proof compares them byte for byte — the expected fixture
/// stores one canonical line per prefix (JSONL), no re-normalization.
fn native_expected(events: &[EventEnvelope]) -> Vec<String> {
    let compact = serde_json::to_string(events).unwrap();
    (0..=events.len())
        .map(|n| fold_prefix_to_json(&compact, n).unwrap())
        .collect()
}

/// Writes the fixtures. Ignored by default; run explicitly with
/// `REGEN_FIXTURES=1 ... -- --ignored regenerate` to (re)commit them.
#[test]
#[ignore = "generator: run with REGEN_FIXTURES=1 to rewrite committed fixtures"]
fn regenerate() {
    assert_eq!(
        std::env::var("REGEN_FIXTURES").ok().as_deref(),
        Some("1"),
        "refusing to write fixtures without REGEN_FIXTURES=1"
    );
    let logs_dir = fixtures_dir().join("logs");
    let expected_dir = fixtures_dir().join("expected");
    fs::create_dir_all(&logs_dir).unwrap();
    fs::create_dir_all(&expected_dir).unwrap();

    for (name, events) in reference_logs() {
        let log_json = serde_json::to_string_pretty(&events).unwrap();
        let expected = native_expected(&events);
        fs::write(logs_dir.join(format!("{name}.json")), log_json + "\n").unwrap();
        // One canonical state per line (prefix 0, 1, …, len), exactly as it
        // crosses the wasm boundary.
        fs::write(
            expected_dir.join(format!("{name}.jsonl")),
            expected.join("\n") + "\n",
        )
        .unwrap();
        eprintln!("wrote fixture {name} ({} events)", events.len());
    }
}

/// The real proof's native anchor: the committed fixtures still equal what the
/// live native fold produces, at every prefix of every reference log.
#[test]
fn committed_fixtures_match_native_fold() {
    let logs_dir = fixtures_dir().join("logs");
    let expected_dir = fixtures_dir().join("expected");

    let mut total_prefixes = 0usize;
    let mut total_logs = 0usize;

    for (name, events) in reference_logs() {
        total_logs += 1;

        // The committed input log equals the reference log (inputs cannot drift).
        let committed_log = fs::read_to_string(logs_dir.join(format!("{name}.json")))
            .unwrap_or_else(|_| {
                panic!("missing committed log fixture {name}; run the regenerator")
            });
        let committed_events: Vec<EventEnvelope> =
            serde_json::from_str(&committed_log).expect("committed log parses");
        assert_eq!(
            committed_events, events,
            "committed log fixture {name} drifted from the reference logs; regenerate"
        );

        // The committed expected side equals the live native fold at every
        // prefix, compared byte for byte as canonical JSON lines.
        let committed_expected_raw =
            fs::read_to_string(expected_dir.join(format!("{name}.jsonl"))).unwrap();
        let committed_expected: Vec<&str> = committed_expected_raw.lines().collect();
        let live = native_expected(&events);

        assert_eq!(
            committed_expected.len(),
            events.len() + 1,
            "expected fixture {name} must cover prefixes 0..={}",
            events.len()
        );
        for (n, (committed, fresh)) in committed_expected.iter().zip(live.iter()).enumerate() {
            assert_eq!(
                *committed, fresh,
                "committed expected fixture {name} prefix {n} drifted from the native fold; regenerate"
            );
        }
        total_prefixes += live.len();
    }

    eprintln!("same-fold native anchor: {total_logs} logs, {total_prefixes} prefixes verified");
    assert!(total_logs >= 12, "expected the full reference-log set");
}
