//! Integration tests for the `salvor-store` public API.
//!
//! These exercise the crate exactly as a consumer would: only re-exported
//! types, no `rusqlite`. Driving `SqliteStore` through `Arc<dyn EventStore>`
//! here is itself a test that the trait is object-safe and async-usable.

use std::sync::Arc;
use std::time::Instant;

use salvor_core::{
    Budget, BudgetKind, Effect, Event, EventEnvelope, RunId, SequenceNumber, TokenUsage,
};
use salvor_store::{EventStore, SqliteStore, StoreError};
use time::OffsetDateTime;

/// Wraps a payload in an envelope for `run` at `seq`, timestamped `seq`
/// seconds after the epoch so ordering is easy to reason about.
fn envelope(run: RunId, seq: u64, event: Event) -> EventEnvelope {
    let recorded_at =
        OffsetDateTime::from_unix_timestamp(1_000_000 + seq as i64).expect("timestamp in range");
    EventEnvelope::new(run, SequenceNumber::new(seq), recorded_at, event)
}

/// One of each of the ten `Event` variants, in a plausible run order.
fn all_ten_variants() -> Vec<Event> {
    vec![
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: serde_json::json!({"topic": "otters"}),
        },
        Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
        },
        Event::ModelCallCompleted {
            seq: SequenceNumber::new(1),
            response: serde_json::json!({"text": "hello"}),
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 7,
            },
        },
        Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".into()),
        },
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(2),
            output: serde_json::json!({"id": "TICKET-1"}),
        },
        Event::Suspended {
            reason: "awaiting approval".into(),
            input_schema: serde_json::json!({"type": "object"}),
        },
        Event::Resumed {
            input: serde_json::json!({"approved": true}),
        },
        Event::BudgetExceeded {
            budget: Budget {
                kind: BudgetKind::CostUsd,
                limit: 2.0,
            },
            observed: 2.5,
        },
        Event::RunCompleted {
            output: serde_json::json!({"summary": "done"}),
        },
        Event::RunFailed {
            error: "provider timeout".into(),
        },
    ]
}

/// Criterion 4: envelopes covering all ten variants append then read back
/// equal, in sequence order.
#[tokio::test]
async fn round_trip_preserves_all_ten_variants() {
    let store = SqliteStore::in_memory().expect("open in-memory store");
    let run = RunId::new();

    let mut written = Vec::new();
    for (index, event) in all_ten_variants().into_iter().enumerate() {
        let env = envelope(run, index as u64 + 1, event);
        store.append(&env).await.expect("append");
        written.push(env);
    }

    let read_back = store.read_log(run).await.expect("read log");
    assert_eq!(
        read_back, written,
        "read-back log differs from what was written"
    );
}

/// Criterion 5: two runs' events interleaved on append; each read-log returns
/// only that run's events, in sequence order.
#[tokio::test]
async fn runs_are_isolated_and_ordered() {
    let store = SqliteStore::in_memory().expect("open in-memory store");
    let run_a = RunId::new();
    let run_b = RunId::new();

    // Interleave appends, and push run_a's out of sequence order on purpose to
    // prove read_log sorts rather than returns insertion order.
    store
        .append(&envelope(run_a, 2, fail("a2")))
        .await
        .expect("a2");
    store
        .append(&envelope(run_b, 1, fail("b1")))
        .await
        .expect("b1");
    store
        .append(&envelope(run_a, 1, fail("a1")))
        .await
        .expect("a1");
    store
        .append(&envelope(run_b, 2, fail("b2")))
        .await
        .expect("b2");

    let log_a = store.read_log(run_a).await.expect("read a");
    let log_b = store.read_log(run_b).await.expect("read b");

    assert_eq!(seqs(&log_a), vec![1, 2], "run_a not ordered by seq");
    assert_eq!(seqs(&log_b), vec![1, 2], "run_b not ordered by seq");
    assert!(
        log_a.iter().all(|e| e.run_id == run_a),
        "run_a log leaked another run's events"
    );
    assert!(
        log_b.iter().all(|e| e.run_id == run_b),
        "run_b log leaked another run's events"
    );
    assert_eq!(errors(&log_a), vec!["a1", "a2"]);
    assert_eq!(errors(&log_b), vec!["b1", "b2"]);
}

/// Criterion 6: a duplicate `(run_id, seq)` append returns the typed conflict
/// variant, with the offending run and seq.
#[tokio::test]
async fn duplicate_append_returns_conflict() {
    let store = SqliteStore::in_memory().expect("open in-memory store");
    let run = RunId::new();

    store
        .append(&envelope(run, 1, fail("first")))
        .await
        .expect("first append");
    let err = store
        .append(&envelope(run, 1, fail("second")))
        .await
        .expect_err("duplicate append must fail");

    match err {
        StoreError::Conflict { run_id, seq } => {
            assert_eq!(run_id, run);
            assert_eq!(seq, SequenceNumber::new(1));
        }
        other => panic!("expected StoreError::Conflict, got {other:?}"),
    }

    // The original is untouched, not overwritten.
    let log = store.read_log(run).await.expect("read log");
    assert_eq!(errors(&log), vec!["first"]);
}

/// Criterion 7: each run appears exactly once in list_runs with correct
/// summary fields.
#[tokio::test]
async fn list_runs_summarizes_each_run_once() {
    let store = SqliteStore::in_memory().expect("open in-memory store");
    let run_a = RunId::new();
    let run_b = RunId::new();

    // run_a gets three events, run_b gets one.
    for seq in 1..=3 {
        store
            .append(&envelope(run_a, seq, fail("a")))
            .await
            .expect("append a");
    }
    store
        .append(&envelope(run_b, 1, fail("b")))
        .await
        .expect("append b");

    let runs = store.list_runs().await.expect("list runs");
    assert_eq!(runs.len(), 2, "each run should appear exactly once");

    let summary_a = runs
        .iter()
        .find(|r| r.run_id == run_a)
        .expect("run_a summarized");
    let summary_b = runs
        .iter()
        .find(|r| r.run_id == run_b)
        .expect("run_b summarized");

    assert_eq!(summary_a.event_count, 3);
    assert_eq!(summary_b.event_count, 1);

    // run_a spans seq 1..=3, so first and last timestamps differ by 2 seconds.
    assert_eq!(
        summary_a.first_recorded_at,
        OffsetDateTime::from_unix_timestamp(1_000_001).unwrap()
    );
    assert_eq!(
        summary_a.last_recorded_at,
        OffsetDateTime::from_unix_timestamp(1_000_003).unwrap()
    );
    // run_b has one event, so first and last coincide.
    assert_eq!(summary_b.first_recorded_at, summary_b.last_recorded_at);
}

/// Criterion 1: the store is usable as `Arc<dyn EventStore>` from async code,
/// including across a spawned task (which requires the trait object to be
/// `Send + Sync`).
#[tokio::test]
async fn drives_through_arc_dyn_event_store() {
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("open store"));
    let run = RunId::new();

    store
        .append(&envelope(run, 1, fail("only")))
        .await
        .expect("append via dyn");

    // Move the handle into another task to prove Send + Sync through the trait
    // object, then read the log back there.
    let handle = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.read_log(run).await })
    };
    let log = handle.await.expect("task joined").expect("read via dyn");
    assert_eq!(seqs(&log), vec![1]);
}

/// Criterion 8: against a file-backed WAL store, appending at least 100 events
/// keeps the mean per-append under 5ms. Prints the observed mean; see it with
/// `cargo test -- --nocapture`.
#[tokio::test]
async fn append_overhead_stays_under_five_ms() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open file-backed store");
    let run = RunId::new();

    const APPENDS: u64 = 200;
    let start = Instant::now();
    for seq in 1..=APPENDS {
        store
            .append(&envelope(run, seq, fail("bench")))
            .await
            .expect("append");
    }
    let mean = start.elapsed() / APPENDS as u32;

    println!(
        "append overhead: {APPENDS} appends, mean {:.3} ms per append",
        mean.as_secs_f64() * 1000.0
    );
    assert!(
        mean.as_secs_f64() * 1000.0 < 5.0,
        "mean append {:.3} ms exceeded the 5ms target",
        mean.as_secs_f64() * 1000.0
    );
}

/// A cheap distinct payload: a `RunFailed` whose error string tags the event.
fn fail(tag: &str) -> Event {
    Event::RunFailed { error: tag.into() }
}

/// The sequence numbers of a log, in order.
fn seqs(log: &[EventEnvelope]) -> Vec<u64> {
    log.iter().map(|e| e.seq.get()).collect()
}

/// The `RunFailed` error tags of a log, in order.
fn errors(log: &[EventEnvelope]) -> Vec<String> {
    log.iter()
        .map(|e| match &e.event {
            Event::RunFailed { error } => error.clone(),
            other => panic!("expected RunFailed, got {other:?}"),
        })
        .collect()
}
