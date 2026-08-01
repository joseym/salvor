//! The individual conformance checks, one async function per clause of the
//! [`EventStore`](salvor_store::EventStore) implementor contract, plus the
//! shared fixtures they run against.
//!
//! Each check takes a fresh store by value and asserts one property of the
//! contract. They are `pub` so a backend can call any single one directly, but
//! most consumers reach them through [`run_all`](crate::run_all) or the
//! [`conformance_tests!`](crate::conformance_tests) macro rather than by hand.

use std::sync::Arc;

use salvor_core::{
    Budget, BudgetKind, Effect, Event, EventEnvelope, RunId, SequenceNumber, TokenUsage,
};
use salvor_store::{EventStore, StoreError};
use time::OffsetDateTime;

/// Wraps a payload in an envelope for `run` at `seq`, timestamped `seq` seconds
/// after a fixed epoch so `list_runs` aggregates are exact to assert against.
fn envelope(run: RunId, seq: u64, event: Event) -> EventEnvelope {
    let recorded_at =
        OffsetDateTime::from_unix_timestamp(1_000_000 + seq as i64).expect("timestamp in range");
    EventEnvelope::new(run, SequenceNumber::new(seq), recorded_at, event)
}

/// The cheapest distinguishable payload: a `RunFailed` whose error string tags
/// the event so ordering and identity assertions can name it.
fn fail(tag: &str) -> Event {
    Event::RunFailed { error: tag.into() }
}

/// The sequence numbers of a log, in the order the store returned them.
fn seqs(log: &[EventEnvelope]) -> Vec<u64> {
    log.iter().map(|e| e.seq.get()).collect()
}

/// The `RunFailed` error tags of a log, in order. Panics on any other variant,
/// so it is only used with logs built entirely from [`fail`].
fn errors(log: &[EventEnvelope]) -> Vec<String> {
    log.iter()
        .map(|e| match &e.event {
            Event::RunFailed { error } => error.clone(),
            other => panic!("expected RunFailed, got {other:?}"),
        })
        .collect()
}

/// One of each of the twelve `Event` kinds, in a plausible run order.
///
/// The count is asserted in [`round_trip_all_event_kinds`], so a new variant
/// added to the vocabulary without a fixture here fails the kit loudly rather
/// than quietly going uncovered.
fn all_event_kinds() -> Vec<Event> {
    vec![
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
        },
        Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
            request_body: None,
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
            performed_by: None,
        },
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(2),
            output: serde_json::json!({"id": "TICKET-1"}),
        },
        Event::NowObserved {
            now: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp in range"),
        },
        Event::RandomObserved { value: u64::MAX },
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

/// Round-trip fidelity across every event kind: appending one envelope of each
/// of the twelve variants and reading them back yields equal envelopes, in
/// sequence order. The exact serialized wire form must survive the round trip.
pub async fn round_trip_all_event_kinds<S: EventStore>(store: S) {
    let run = RunId::new();

    let mut written = Vec::new();
    for (index, event) in all_event_kinds().into_iter().enumerate() {
        let env = envelope(run, index as u64 + 1, event);
        store.append(&env).await.expect("append");
        written.push(env);
    }
    assert_eq!(
        written.len(),
        12,
        "the fixture must cover all twelve event kinds"
    );

    let read_back = store.read_log(run).await.expect("read log");
    assert_eq!(
        read_back, written,
        "read-back log differs from what was written"
    );
}

/// Ordering is by sequence number, never by append order. Appending a run's
/// events shuffled and reading them back returns them sorted ascending.
pub async fn ordering_independent_of_append_order<S: EventStore>(store: S) {
    let run = RunId::new();

    for seq in [3_u64, 1, 4, 2] {
        store
            .append(&envelope(run, seq, fail(&format!("e{seq}"))))
            .await
            .expect("append");
    }

    let log = store.read_log(run).await.expect("read log");
    assert_eq!(
        seqs(&log),
        vec![1, 2, 3, 4],
        "read_log must sort by sequence regardless of append order"
    );
}

/// Run isolation: interleaving appends across two runs keeps their logs
/// separate. Each `read_log` returns only its own run's events, in order.
pub async fn runs_are_isolated<S: EventStore>(store: S) {
    let run_a = RunId::new();
    let run_b = RunId::new();

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

/// Uniqueness: a second append at an occupied `(run_id, seq)` position returns
/// the typed [`StoreError::Conflict`] naming that position, and leaves the
/// original event untouched rather than overwriting it.
pub async fn duplicate_append_conflicts<S: EventStore>(store: S) {
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
            assert_eq!(run_id, run, "conflict names the wrong run");
            assert_eq!(seq, SequenceNumber::new(1), "conflict names the wrong seq");
        }
        other => panic!("expected StoreError::Conflict, got {other:?}"),
    }

    let log = store.read_log(run).await.expect("read log");
    assert_eq!(
        errors(&log),
        vec!["first"],
        "the original event must be untouched by the rejected append"
    );
}

/// An unknown run reads back as an empty log, not an error.
pub async fn unknown_run_reads_empty<S: EventStore>(store: S) {
    let known = RunId::new();
    let unknown = RunId::new();

    store
        .append(&envelope(known, 1, fail("x")))
        .await
        .expect("append to known run");

    let log = store.read_log(unknown).await.expect("read unknown run");
    assert!(
        log.is_empty(),
        "an unknown run must read back empty, not error"
    );
}

/// `list_runs` reports every run exactly once, with correct aggregates: the
/// event count, and the earliest and latest recorded timestamps.
pub async fn list_runs_reports_each_run_once<S: EventStore>(store: S) {
    let run_a = RunId::new();
    let run_b = RunId::new();

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

    // run_a spans seq 1..=3, so its first and last timestamps differ by two
    // seconds; run_b has one event, so its first and last coincide.
    assert_eq!(
        summary_a.first_recorded_at,
        OffsetDateTime::from_unix_timestamp(1_000_001).unwrap()
    );
    assert_eq!(
        summary_a.last_recorded_at,
        OffsetDateTime::from_unix_timestamp(1_000_003).unwrap()
    );
    assert_eq!(summary_b.first_recorded_at, summary_b.last_recorded_at);
}

/// Object safety and thread safety: the store drives through
/// `Arc<dyn EventStore>`, including from a spawned task, which only compiles if
/// the trait object is `Send + Sync`.
pub async fn usable_as_arc_dyn_event_store<S: EventStore + 'static>(store: S) {
    let store: Arc<dyn EventStore> = Arc::new(store);
    let run = RunId::new();

    store
        .append(&envelope(run, 1, fail("only")))
        .await
        .expect("append via dyn");

    let handle = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.read_log(run).await })
    };
    let log = handle.await.expect("task joined").expect("read via dyn");
    assert_eq!(seqs(&log), vec![1]);
}

/// Concurrency: many tasks race to append the same `(run_id, seq)`. Exactly one
/// must win with `Ok`, every other must see [`StoreError::Conflict`], and the
/// log must hold exactly one event at that position, the winner's.
///
/// A [`tokio::sync::Barrier`] releases all racers together so they contend for
/// real, and the winner's payload is compared against what actually landed, so
/// this proves the surviving write is the one that reported success rather than
/// just that a single row remains.
pub async fn concurrent_single_position_has_one_winner<S: EventStore + 'static>(store: S) {
    const RACERS: usize = 16;
    const CONTESTED_SEQ: u64 = 3;

    let store = Arc::new(store);
    let run = RunId::new();
    let gate = Arc::new(tokio::sync::Barrier::new(RACERS));

    let mut handles = Vec::with_capacity(RACERS);
    for racer in 0..RACERS {
        let store = Arc::clone(&store);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            let env = envelope(run, CONTESTED_SEQ, fail(&format!("racer-{racer}")));
            gate.wait().await;
            (racer, store.append(&env).await)
        }));
    }

    let mut wins = 0_usize;
    let mut conflicts = 0_usize;
    let mut winner = None;
    for handle in handles {
        let (racer, result) = handle.await.expect("racer task joined");
        match result {
            Ok(()) => {
                wins += 1;
                winner = Some(racer);
            }
            Err(StoreError::Conflict { run_id, seq }) => {
                conflicts += 1;
                assert_eq!(run_id, run, "conflict names the wrong run");
                assert_eq!(
                    seq,
                    SequenceNumber::new(CONTESTED_SEQ),
                    "conflict names the wrong seq"
                );
            }
            Err(other) => panic!("a racer failed with an unexpected error: {other:?}"),
        }
    }

    assert_eq!(wins, 1, "exactly one racer must win the position");
    assert_eq!(
        conflicts,
        RACERS - 1,
        "every losing racer must see a Conflict"
    );

    let winner = winner.expect("some racer won");
    let log = store.read_log(run).await.expect("read log");
    let at_position: Vec<&EventEnvelope> = log
        .iter()
        .filter(|e| e.seq == SequenceNumber::new(CONTESTED_SEQ))
        .collect();
    assert_eq!(
        at_position.len(),
        1,
        "the log must hold exactly one event at the contested position"
    );
    match &at_position[0].event {
        Event::RunFailed { error } => assert_eq!(
            error,
            &format!("racer-{winner}"),
            "the stored event is not the one whose append returned Ok"
        ),
        other => panic!("unexpected stored event: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    //! Self-test: the kit runs against an in-crate reference store built on a
    //! plain locked vector. This proves the checks are store-agnostic (they
    //! never reach for a SQLite detail) and that a straightforwardly correct
    //! store passes every one of them.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use salvor_store::RunSummary;

    use super::*;

    /// A correct, minimal [`EventStore`] backed by a locked vector of
    /// envelopes. The lock makes the uniqueness check atomic, which is what the
    /// concurrency check needs from any real backend too.
    #[derive(Default)]
    struct VecStore {
        events: Mutex<Vec<EventEnvelope>>,
    }

    #[async_trait]
    impl EventStore for VecStore {
        async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
            let mut events = self.events.lock().expect("lock");
            if events
                .iter()
                .any(|e| e.run_id == envelope.run_id && e.seq == envelope.seq)
            {
                return Err(StoreError::Conflict {
                    run_id: envelope.run_id,
                    seq: envelope.seq,
                });
            }
            events.push(envelope.clone());
            Ok(())
        }

        async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
            let events = self.events.lock().expect("lock");
            let mut log: Vec<EventEnvelope> = events
                .iter()
                .filter(|e| e.run_id == run_id)
                .cloned()
                .collect();
            log.sort_by_key(|e| e.seq);
            Ok(log)
        }

        async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
            let events = self.events.lock().expect("lock");
            let mut by_run: HashMap<RunId, (u64, OffsetDateTime, OffsetDateTime)> = HashMap::new();
            for e in events.iter() {
                by_run
                    .entry(e.run_id)
                    .and_modify(|(count, first, last)| {
                        *count += 1;
                        *first = (*first).min(e.recorded_at);
                        *last = (*last).max(e.recorded_at);
                    })
                    .or_insert((1, e.recorded_at, e.recorded_at));
            }
            Ok(by_run
                .into_iter()
                .map(|(run_id, (event_count, first, last))| RunSummary {
                    run_id,
                    event_count,
                    first_recorded_at: first,
                    last_recorded_at: last,
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn reference_store_passes_round_trip() {
        round_trip_all_event_kinds(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_ordering() {
        ordering_independent_of_append_order(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_isolation() {
        runs_are_isolated(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_conflict() {
        duplicate_append_conflicts(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_unknown_run() {
        unknown_run_reads_empty(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_list_runs() {
        list_runs_reports_each_run_once(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_dyn() {
        usable_as_arc_dyn_event_store(VecStore::default()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reference_store_passes_concurrency() {
        concurrent_single_position_has_one_winner(VecStore::default()).await;
    }
}
