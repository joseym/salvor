//! The individual conformance checks, one async function per clause of the
//! [`EventStore`](salvor_store::EventStore) implementor contract, plus the
//! shared fixtures they run against.
//!
//! Each check takes a fresh store by value and asserts one property of the
//! contract. They are `pub` so a backend can call any single one directly, but
//! most consumers reach them through [`run_all`](crate::run_all) or the
//! [`conformance_tests!`](crate::conformance_tests) macro rather than by hand.

use std::sync::Arc;

use async_trait::async_trait;
use salvor_core::{
    Budget, BudgetKind, Effect, Event, EventEnvelope, RunId, SequenceNumber, TokenUsage,
};
use salvor_store::{CallClaim, CallClaimant, EventStore, StoreError};
use time::OffsetDateTime;

/// A store under test, plus the one door the kit needs to play attacker
/// against it.
///
/// The tamper-evidence clauses of the contract cannot be checked through the
/// [`EventStore`] surface alone: that surface has no way to modify a recorded
/// event, which is exactly the property being checked. So a backend that wants
/// to run the kit supplies a small test-only type that *is* the store (this
/// trait requires [`EventStore`], and implementations normally delegate the
/// three methods to the real store) and can additionally reach behind it the
/// way an attacker with storage access would: a second SQLite connection, a
/// direct table write, an object-store `PUT`.
///
/// The harness type belongs in the backend's tests, never in its shipped API.
/// Nothing in the kit calls [`forge_recorded_envelope`](Self::forge_recorded_envelope)
/// except the tamper checks.
#[async_trait]
pub trait TamperHarness: EventStore + 'static {
    /// Replaces the bytes recorded at `(run_id, seq)` with `envelope_json`,
    /// going around every append-only guard the backend enforces.
    ///
    /// The kit hands over well-formed envelope JSON, so what lands is a row
    /// that parses perfectly and simply is not what was recorded. That is the
    /// case the checks care about: a corrupt row announces itself, a forged
    /// one does not.
    ///
    /// Implementations panic if the write cannot be made. A harness that
    /// silently fails to tamper would turn the check into a test that passes
    /// for the wrong reason.
    async fn forge_recorded_envelope(
        &self,
        run_id: RunId,
        seq: SequenceNumber,
        envelope_json: &str,
    );
}

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
            deduplicated_from: None,
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

/// Tamper evidence, the happy half: a log nobody touched verifies and reads
/// back whole, twice, with the exact bytes that were appended.
///
/// This is the check that keeps the other half honest. A backend could "detect
/// tampering" by refusing every read; this one fails it if it does. It appends
/// out of sequence order on purpose, because a chain built over append order
/// and a log returned in sequence order must both be right at once.
pub async fn recorded_log_verifies_when_untouched<S: EventStore>(store: S) {
    let run = RunId::new();

    let mut written = Vec::new();
    for seq in [4_u64, 1, 3, 2, 5] {
        let env = envelope(run, seq, fail(&format!("e{seq}")));
        store.append(&env).await.expect("append");
        written.push(env);
    }
    written.sort_by_key(|e| e.seq);

    let first = store.read_log(run).await.expect("first read verifies");
    let second = store.read_log(run).await.expect("second read verifies");

    assert_eq!(first, written, "an untouched log must read back whole");
    assert_eq!(second, written, "verification must not be one-shot");
    for (read, appended) in first.iter().zip(&written) {
        assert_eq!(
            serde_json::to_string(read).expect("serialize"),
            serde_json::to_string(appended).expect("serialize"),
            "the stored wire bytes must survive verification unchanged"
        );
    }
}

/// Tamper evidence, the point of the exercise: a recorded row rewritten with
/// *valid* JSON is refused on read, with [`StoreError::TamperEvident`] naming
/// the run and the position.
///
/// The forged row is a well-formed envelope for the same run and position,
/// differing only in payload. Nothing about parsing can tell it from the real
/// one, which is why a store that only reports unreadable rows is not
/// tamper-evident and fails here.
pub async fn valid_json_tamper_is_detected<H: TamperHarness>(harness: H) {
    let run = RunId::new();
    for seq in 1..=3 {
        harness
            .append(&envelope(run, seq, fail("recorded")))
            .await
            .expect("append");
    }
    harness.read_log(run).await.expect("clean log reads");

    let forged = serde_json::to_string(&envelope(run, 2, fail("forged")))
        .expect("the forgery is valid envelope JSON");
    serde_json::from_str::<EventEnvelope>(&forged).expect("and it deserializes cleanly");
    harness
        .forge_recorded_envelope(run, SequenceNumber::new(2), &forged)
        .await;

    match harness.read_log(run).await {
        Err(StoreError::TamperEvident { run_id, seq, .. }) => {
            assert_eq!(run_id, run, "the error names the wrong run");
            assert_eq!(
                seq,
                SequenceNumber::new(2),
                "the error names the wrong position"
            );
        }
        Err(other) => panic!("expected StoreError::TamperEvident, got {other:?}"),
        Ok(log) => panic!(
            "a rewritten row was served as if it were recorded history: {:?}",
            errors(&log)
        ),
    }
}

/// A forged row in one run does not make another run unreadable: the chain is
/// per run, so the blast radius of a tamper is the run it happened in.
///
/// This matters for a control plane listing runs. One damaged log must not
/// take the rest of the store down with it.
pub async fn tamper_is_confined_to_its_run<H: TamperHarness>(harness: H) {
    let forged_run = RunId::new();
    let intact_run = RunId::new();
    for seq in 1..=2 {
        harness
            .append(&envelope(forged_run, seq, fail("f")))
            .await
            .expect("append forged-run event");
        harness
            .append(&envelope(intact_run, seq, fail("i")))
            .await
            .expect("append intact-run event");
    }

    let forged =
        serde_json::to_string(&envelope(forged_run, 1, fail("forged"))).expect("serialize");
    harness
        .forge_recorded_envelope(forged_run, SequenceNumber::new(1), &forged)
        .await;

    assert!(
        matches!(
            harness.read_log(forged_run).await,
            Err(StoreError::TamperEvident { .. })
        ),
        "the tampered run must be refused"
    );
    let intact = harness
        .read_log(intact_run)
        .await
        .expect("intact run reads");
    assert_eq!(
        errors(&intact),
        vec!["i", "i"],
        "an untouched run must survive its neighbor being tampered with"
    );
    assert_eq!(
        harness.list_runs().await.expect("list runs").len(),
        2,
        "both runs stay listed; only reading the tampered log fails"
    );
}

/// A claimant for `tool` under `key`, on behalf of `run`'s intent at `seq`.
fn claimant<'a>(tool: &'a str, key: &'a str, run: RunId, seq: u64) -> CallClaimant<'a> {
    CallClaimant {
        tool,
        idempotency_key: key,
        run_id: run,
        intent_seq: SequenceNumber::new(seq),
    }
}

/// Call commitment, the exclusivity clause: an identity has exactly one owner,
/// a second run is told who that owner is, the owner may re-claim its own
/// identity, and `lookup_call` reports all of it without creating anything.
///
/// The re-claim case is the one that is easy to get wrong and expensive to get
/// wrong: a run that crashed inside its own call and came back must be told
/// `Claimed`, not `Held`, or it can never finish the call it started.
pub async fn call_claim_is_exclusive<S: EventStore>(store: S) {
    let owner = RunId::new();
    let rival = RunId::new();

    assert_eq!(
        store.lookup_call("pay", "claim-1").await.expect("lookup"),
        None,
        "an unclaimed identity must look up as None"
    );

    let first = store
        .claim_call(claimant("pay", "claim-1", owner, 4))
        .await
        .expect("first claim");
    assert_eq!(first, CallClaim::Claimed, "the first claim must win");

    let again = store
        .claim_call(claimant("pay", "claim-1", owner, 4))
        .await
        .expect("re-claim");
    assert_eq!(
        again,
        CallClaim::Claimed,
        "a claimant must be able to re-claim the identity it already holds"
    );

    let lost = store
        .claim_call(claimant("pay", "claim-1", rival, 9))
        .await
        .expect("rival claim");
    match lost {
        CallClaim::Held(commitment) => {
            assert_eq!(commitment.run_id, owner, "the holder must be the first run");
            assert_eq!(commitment.intent_seq, SequenceNumber::new(4));
            assert_eq!(
                commitment.completion_seq, None,
                "an unfinished call must report no completion"
            );
        }
        CallClaim::Claimed => panic!("a second run must not be granted a held identity"),
    }

    // The identity is the pair. Neither half alone collides.
    assert_eq!(
        store
            .claim_call(claimant("refund", "claim-1", rival, 9))
            .await
            .expect("other tool"),
        CallClaim::Claimed,
        "the same key under a different tool is a different identity"
    );
    assert_eq!(
        store
            .claim_call(claimant("pay", "claim-2", rival, 9))
            .await
            .expect("other key"),
        CallClaim::Claimed,
        "a different key under the same tool is a different identity"
    );

    let looked_up = store
        .lookup_call("pay", "claim-1")
        .await
        .expect("lookup")
        .expect("the identity is claimed");
    assert_eq!(looked_up.run_id, owner);
    assert_eq!(looked_up.intent_seq, SequenceNumber::new(4));
    assert_eq!(
        store
            .lookup_call("pay", "never-claimed")
            .await
            .expect("lookup"),
        None,
        "lookup must not create the commitment it was asked about"
    );
}

/// Call commitment, the settlement clause: settling appends the completion and
/// marks the commitment in one indivisible step, a run that does not hold the
/// identity cannot settle it, and a refused settlement appends nothing.
///
/// The "appends nothing" half is what makes this worth a check of its own. A
/// backend that appended first and then discovered it could not settle would
/// leave a completion in a log with no commitment behind it, which is the exact
/// state the mechanism exists to prevent.
pub async fn settling_append_is_atomic<S: EventStore>(store: S) {
    let owner = RunId::new();
    let rival = RunId::new();

    store
        .append(&envelope(owner, 1, fail("intent stand-in")))
        .await
        .expect("append");
    store
        .claim_call(claimant("pay", "claim-1", owner, 1))
        .await
        .expect("claim");

    let completion = envelope(owner, 2, fail("completion stand-in"));
    store
        .append_settling_call(&completion, claimant("pay", "claim-1", owner, 1))
        .await
        .expect("settle");

    let commitment = store
        .lookup_call("pay", "claim-1")
        .await
        .expect("lookup")
        .expect("claimed");
    assert_eq!(
        commitment.completion_seq,
        Some(SequenceNumber::new(2)),
        "settlement must record where the completion landed"
    );
    let log = store.read_log(owner).await.expect("read log");
    assert_eq!(
        seqs(&log),
        vec![1, 2],
        "the settling append must have landed like any other append"
    );

    // A run that does not hold the identity cannot settle it, and its append
    // must not survive the refusal.
    store
        .append(&envelope(rival, 1, fail("rival intent")))
        .await
        .expect("append");
    let stolen = envelope(rival, 2, fail("rival completion"));
    let refused = store
        .append_settling_call(&stolen, claimant("pay", "claim-1", rival, 1))
        .await;
    assert!(
        refused.is_err(),
        "settling an identity another run holds must fail"
    );
    assert_eq!(
        seqs(&store.read_log(rival).await.expect("read log")),
        vec![1],
        "a refused settlement must append nothing"
    );
}

/// Concurrency, the arbitration clause: many tasks race to claim one identity.
/// Exactly one must be told `Claimed`, every other must be told `Held` naming
/// that same winner, and the recorded commitment must be the winner's.
///
/// This is the sibling of [`concurrent_single_position_has_one_winner`], run
/// against the other uniqueness constraint in the store, and it is the one that
/// decides whether two live runs can pay the same invoice at the same instant.
/// A [`tokio::sync::Barrier`] releases the racers together so they contend for
/// real.
pub async fn concurrent_call_claims_have_one_winner<S: EventStore + 'static>(store: S) {
    const RACERS: usize = 16;

    let store = Arc::new(store);
    let gate = Arc::new(tokio::sync::Barrier::new(RACERS));
    let runs: Vec<RunId> = (0..RACERS).map(|_| RunId::new()).collect();

    let mut handles = Vec::with_capacity(RACERS);
    for (racer, run) in runs.iter().copied().enumerate() {
        let store = Arc::clone(&store);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            gate.wait().await;
            let claim = store
                .claim_call(claimant("pay", "contested", run, racer as u64 + 1))
                .await;
            (run, claim)
        }));
    }

    let mut winners = Vec::new();
    let mut held = Vec::new();
    for handle in handles {
        let (run, claim) = handle.await.expect("racer task joined");
        match claim.expect("a claim must not error") {
            CallClaim::Claimed => winners.push(run),
            CallClaim::Held(commitment) => held.push(commitment),
        }
    }

    assert_eq!(
        winners.len(),
        1,
        "exactly one racer may be granted the identity"
    );
    let winner = winners[0];
    assert_eq!(held.len(), RACERS - 1, "every loser must be told it lost");
    for commitment in &held {
        assert_eq!(
            commitment.run_id, winner,
            "every loser must be pointed at the one winner"
        );
        assert_eq!(
            commitment.completion_seq, None,
            "the winner had not finished, so no loser may be told it had"
        );
    }

    let recorded = store
        .lookup_call("pay", "contested")
        .await
        .expect("lookup")
        .expect("claimed");
    assert_eq!(
        recorded.run_id, winner,
        "the stored commitment must belong to the racer whose claim returned Claimed"
    );
}

#[cfg(test)]
mod tests {
    //! Self-test: the kit runs against an in-crate reference store built on a
    //! plain locked map of chained rows. This proves the checks are
    //! store-agnostic (they never reach for a SQLite detail) and that a
    //! straightforwardly correct store passes every one of them, tamper
    //! evidence included.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use salvor_store::chain::{self, ChainHead, ChainRow};
    use salvor_store::{CallCommitment, RunSummary};

    use super::*;

    /// One recorded row: the exact bytes stored, and the two chain values
    /// recorded beside them. Keeping the serialized text rather than the parsed
    /// envelope is not an implementation detail, it is the requirement: the
    /// chain is a statement about stored bytes, so the store must have bytes to
    /// make it about.
    struct Row {
        seq: SequenceNumber,
        recorded_at: OffsetDateTime,
        envelope_json: String,
        prev_hash: String,
        row_hash: String,
    }

    /// A correct, minimal [`EventStore`]: one locked map from run to that run's
    /// rows, held in append order. The lock makes the uniqueness check atomic,
    /// which is what the concurrency check needs from any real backend too, and
    /// it is also what makes reading the chain head and appending onto it one
    /// indivisible step.
    ///
    /// This is the shortest complete worked example of what the trait now asks
    /// for: chain on append with [`chain::row_hash`], verify on read with
    /// [`chain::verify`].
    #[derive(Default)]
    struct VecStore {
        runs: Mutex<HashMap<RunId, Vec<Row>>>,
        /// One entry per `(tool, idempotency_key)` identity. Behind the *same*
        /// lock discipline as the rows, and taken in the same order, so that a
        /// settlement writes the completion and the commitment as one step.
        commitments: Mutex<HashMap<(String, String), CallCommitment>>,
    }

    impl VecStore {
        /// Writes one row into an already-locked run map: the append body, with
        /// the lock hoisted so a settlement can hold it across two writes.
        fn write_row(
            runs: &mut HashMap<RunId, Vec<Row>>,
            envelope: &EventEnvelope,
        ) -> Result<(), StoreError> {
            let rows = runs.entry(envelope.run_id).or_default();
            if rows.iter().any(|row| row.seq == envelope.seq) {
                return Err(StoreError::Conflict {
                    run_id: envelope.run_id,
                    seq: envelope.seq,
                });
            }
            let envelope_json = serde_json::to_string(envelope)?;
            let prev_hash = rows.last().map_or_else(
                || chain::GENESIS_PREV_HASH.to_owned(),
                |row| row.row_hash.clone(),
            );
            let row_hash =
                chain::row_hash(&prev_hash, envelope.run_id, envelope.seq, &envelope_json);
            rows.push(Row {
                seq: envelope.seq,
                recorded_at: envelope.recorded_at,
                envelope_json,
                prev_hash,
                row_hash,
            });
            Ok(())
        }
    }

    #[async_trait]
    impl EventStore for VecStore {
        async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
            let mut runs = self.runs.lock().expect("lock");
            Self::write_row(&mut runs, envelope)
        }

        async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
            let runs = self.runs.lock().expect("lock");
            let Some(rows) = runs.get(&run_id) else {
                return Ok(Vec::new());
            };

            let chain_rows: Vec<ChainRow<'_>> = rows
                .iter()
                .map(|row| ChainRow {
                    seq: row.seq,
                    envelope_json: &row.envelope_json,
                    prev_hash: &row.prev_hash,
                    row_hash: &row.row_hash,
                })
                .collect();
            let head = rows.last().map(|row| ChainHead {
                len: rows.len() as u64,
                hash: &row.row_hash,
            });
            chain::verify(run_id, &chain_rows, head)?;

            let mut log: Vec<EventEnvelope> = Vec::with_capacity(rows.len());
            for row in rows {
                log.push(serde_json::from_str(&row.envelope_json)?);
            }
            log.sort_by_key(|envelope| envelope.seq);
            Ok(log)
        }

        async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
            let runs = self.runs.lock().expect("lock");
            Ok(runs
                .iter()
                .filter(|(_, rows)| !rows.is_empty())
                .map(|(run_id, rows)| RunSummary {
                    run_id: *run_id,
                    event_count: rows.len() as u64,
                    first_recorded_at: rows
                        .iter()
                        .map(|row| row.recorded_at)
                        .min()
                        .expect("non-empty"),
                    last_recorded_at: rows
                        .iter()
                        .map(|row| row.recorded_at)
                        .max()
                        .expect("non-empty"),
                })
                .collect())
        }

        /// The whole arbitration, in the shortest correct form: one lock held
        /// across the look and the write, so no rival can slip between them.
        /// A real backend gets this from a unique constraint instead.
        async fn claim_call(&self, claimant: CallClaimant<'_>) -> Result<CallClaim, StoreError> {
            let mut commitments = self.commitments.lock().expect("lock");
            let identity = (
                claimant.tool.to_owned(),
                claimant.idempotency_key.to_owned(),
            );
            match commitments.get(&identity) {
                // Already ours and unfinished: this is the same call coming
                // back, not a rival, so it keeps the right to execute.
                Some(held)
                    if held.run_id == claimant.run_id
                        && held.intent_seq == claimant.intent_seq
                        && held.completion_seq.is_none() =>
                {
                    Ok(CallClaim::Claimed)
                }
                Some(held) => Ok(CallClaim::Held(*held)),
                None => {
                    commitments.insert(
                        identity,
                        CallCommitment {
                            run_id: claimant.run_id,
                            intent_seq: claimant.intent_seq,
                            completion_seq: None,
                        },
                    );
                    Ok(CallClaim::Claimed)
                }
            }
        }

        async fn lookup_call(
            &self,
            tool: &str,
            idempotency_key: &str,
        ) -> Result<Option<CallCommitment>, StoreError> {
            let commitments = self.commitments.lock().expect("lock");
            Ok(commitments
                .get(&(tool.to_owned(), idempotency_key.to_owned()))
                .copied())
        }

        /// Both locks are held across both writes, so the completion and the
        /// settlement land together or not at all. Ownership is checked before
        /// the row is written, so a refusal appends nothing.
        async fn append_settling_call(
            &self,
            envelope: &EventEnvelope,
            claimant: CallClaimant<'_>,
        ) -> Result<(), StoreError> {
            let mut runs = self.runs.lock().expect("lock");
            let mut commitments = self.commitments.lock().expect("lock");
            let identity = (
                claimant.tool.to_owned(),
                claimant.idempotency_key.to_owned(),
            );
            let held = commitments.get_mut(&identity).filter(|held| {
                held.run_id == claimant.run_id
                    && held.intent_seq == claimant.intent_seq
                    && held.completion_seq.is_none()
            });
            let Some(held) = held else {
                return Err(StoreError::Backend(format!(
                    "run {} does not hold an unsettled commitment for tool `{}` key `{}`",
                    claimant.run_id.as_uuid(),
                    claimant.tool,
                    claimant.idempotency_key
                )));
            };
            Self::write_row(&mut runs, envelope)?;
            held.completion_seq = Some(envelope.seq);
            Ok(())
        }
    }

    #[async_trait]
    impl TamperHarness for VecStore {
        async fn forge_recorded_envelope(
            &self,
            run_id: RunId,
            seq: SequenceNumber,
            envelope_json: &str,
        ) {
            let mut runs = self.runs.lock().expect("lock");
            let row = runs
                .get_mut(&run_id)
                .and_then(|rows| rows.iter_mut().find(|row| row.seq == seq))
                .expect("the row to forge must exist");
            // Only the bytes change. The chain values are left exactly as
            // recorded, which is the attacker who does not know they are there.
            row.envelope_json = envelope_json.to_owned();
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

    #[tokio::test]
    async fn reference_store_passes_clean_chain() {
        recorded_log_verifies_when_untouched(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_valid_json_tamper() {
        valid_json_tamper_is_detected(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_tamper_confinement() {
        tamper_is_confined_to_its_run(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_call_claim_exclusivity() {
        call_claim_is_exclusive(VecStore::default()).await;
    }

    #[tokio::test]
    async fn reference_store_passes_settling_append() {
        settling_append_is_atomic(VecStore::default()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reference_store_passes_claim_arbitration() {
        concurrent_call_claims_have_one_winner(VecStore::default()).await;
    }
}
