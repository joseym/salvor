//! A tool parks its own run on a durable timer: the built-in loop turns the
//! sleep outcome into `sleep_until` + `await_wake`, exactly as it turns a
//! suspension into `suspend` + `await_resume`.
//!
//! # What the sentinel buys, and why it is asserted here
//!
//! The request travels inside the call's own `ToolCallCompleted`, so the call
//! settles before the sleep starts. The recorded order is intent, completion,
//! `SleepStarted`, and that order is the point: a settled completion releases
//! the store's idempotency claim, so a run asleep for a week blocks nobody and
//! leaves no dangling write intent for a crash to strand. Two tests below pin
//! it, one on the recorded order and one on the store's own claim state.
//!
//! # The clock
//!
//! [`TestClock`] is injected and the test moves it by hand. Nothing sleeps in
//! real time.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{
    ScriptedModel, TestClock, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{Effect, Event, EventEnvelope, RunId, RunStatus, derive_state};
use salvor_runtime::{Agent, ParkReason, RunOutcome, Runtime, SLEEP_SENTINEL_KEY};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use wiremock::MockServer;

/// The instant every run below starts at.
const START: OffsetDateTime = datetime!(2026-08-14 08:00:00 UTC);

/// The instant the tool asks the run to park until. Fixed, not derived from a
/// duration, because it is recorded and every drive must present the same one.
const WAKE_AT: OffsetDateTime = datetime!(2026-08-14 09:00:00 UTC);

/// The key the claim-safety scenario's tool declares. Only a key a tool
/// declares for itself is an identity the store deduplicates on, so this is
/// what makes the call take a claim at all.
const DECLARED_KEY: &str = "hold:order-9";

/// The log a completed run records, and the shape every scenario below is
/// measured against. `SleepStarted` after `ToolCallCompleted` is the claim
/// ordering, written down.
const COMPLETED_KINDS: [&str; 12] = [
    "RunStarted",
    "NowObserved",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "SleepStarted",
    "SleepCompleted",
    "NowObserved",
    "ModelCallRequested",
    "ModelCallCompleted",
    "RunCompleted",
];

/// The two-turn conversation every scenario runs: call the tool, then answer.
/// Selected by message count, so a replayed first turn never reaches the
/// server.
async fn scripted_server() -> MockServer {
    ScriptedModel::mount(vec![
        (1, tool_use_response("tu_hold", "hold", json!({}), 100, 10)),
        (3, text_response("held and resumed", 120, 12)),
    ])
    .await
}

/// An agent whose one tool parks the run until [`WAKE_AT`]. `declared_key`
/// makes that call take an idempotency claim; without one there is no identity
/// to claim and the claim question does not arise.
fn napping_agent(
    server_uri: &str,
    effect: Effect,
    declared_key: Option<&str>,
) -> (Agent, Arc<AtomicUsize>) {
    let (tool, calls) = TestTool::new("hold", effect, ToolBehavior::Sleep(WAKE_AT));
    let tool = match declared_key {
        Some(key) => tool.declaring_key(key),
        None => tool,
    };
    let agent = agent_builder(server_uri)
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");
    (agent, calls)
}

/// A fresh in-memory store.
fn store() -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::in_memory().expect("store opens"))
}

/// Drives a run to completion, moving the clock to the recorded wake instant
/// whenever a drive leaves it asleep. This is the whole of what a waker does,
/// with the test standing in for the sweep.
async fn drive_to_completion(
    runtime: &Runtime,
    agent: &Agent,
    run_id: RunId,
    store: &Arc<dyn EventStore>,
    clock: &TestClock,
) -> Value {
    // One sleep needs at most three drives (start, wake, finish); the cap only
    // bounds a runaway bug.
    for _ in 0..8 {
        let log = store.read_log(run_id).await.expect("log reads");
        let outcome = match derive_state(&log).status {
            RunStatus::Completed { output } => return output,
            RunStatus::NotStarted => runtime
                .start_with_id(agent, run_id, json!("hold this"))
                .await
                .expect("the fresh run drives"),
            RunStatus::Sleeping { wake_at } => {
                // Only ever forward, and only as far as the deadline: a caller
                // that already moved its own clock past it keeps its reading.
                if clock.read() < wake_at {
                    clock.set(wake_at);
                }
                runtime.recover(agent, run_id).await.expect("the run wakes")
            }
            // Running or interrupted mid-step: an ordinary recovery.
            _ => runtime
                .recover(agent, run_id)
                .await
                .expect("the crashed run recovers"),
        };
        if let RunOutcome::Completed { output, .. } = outcome {
            return output;
        }
    }
    panic!("the run neither completed nor made progress");
}

/// The whole of the tool-driven timer, in the order the log records it: the
/// call completes, the sleep starts, the run parks, an early re-drive changes
/// nothing, and a drive past the deadline wakes it and carries the loop to its
/// answer.
#[tokio::test]
async fn a_tool_parks_the_run_and_the_completion_lands_before_the_sleep() {
    let server = scripted_server().await;
    let (agent, calls) = napping_agent(&server.uri(), Effect::Read, None);
    let store = store();
    let clock = TestClock::new(START);
    let runtime = Runtime::with_hooks(store.clone(), clock.injected(), fixed_random());
    let run_id = fixed_run_id(60);

    let parked = runtime
        .start_with_id(&agent, run_id, json!("hold this"))
        .await
        .expect("the first drive parks");
    assert!(
        matches!(
            parked,
            RunOutcome::Parked {
                reason: ParkReason::Sleeping { wake_at },
                ..
            } if wake_at == WAKE_AT
        ),
        "the park names the instant the tool asked for, got {parked:?}"
    );

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        COMPLETED_KINDS[..7].to_vec(),
        "the log ends at the started sleep"
    );
    // THE ORDERING. The completion settles the call; the sleep starts after
    // it. Read off the recorded log rather than inferred from the outcome.
    assert!(
        matches!(log[5].event, Event::ToolCallCompleted { .. })
            && matches!(log[6].event, Event::SleepStarted { .. }),
        "the completion is recorded before the sleep"
    );
    let Event::ToolCallCompleted { ref output, .. } = log[5].event else {
        unreachable!("checked above")
    };
    assert_eq!(
        output,
        &json!({SLEEP_SENTINEL_KEY: {"wake_at": "2026-08-14T09:00:00Z"}}),
        "the request rides in the completion, not in an event of its own"
    );
    assert_eq!(
        derive_state(&log).status,
        RunStatus::Sleeping { wake_at: WAKE_AT }
    );

    // A minute early. The deadline is enforced inside the run, so this records
    // nothing however often it is asked.
    clock.set(WAKE_AT - Duration::minutes(1));
    let early = runtime
        .recover(&agent, run_id)
        .await
        .expect("an early drive is not an error");
    assert!(
        matches!(
            early,
            RunOutcome::Parked {
                reason: ParkReason::Sleeping { .. },
                ..
            }
        ),
        "still asleep: {early:?}"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads").len(),
        7,
        "and an early drive appends nothing"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool ran once");

    // At the deadline the wake is recorded and the loop carries on to its
    // answer, with the tool never running again.
    let output = drive_to_completion(&runtime, &agent, run_id, &store, &clock).await;
    assert_eq!(output, json!("held and resumed"));
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&log), COMPLETED_KINDS.to_vec());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a replayed completion never re-executes the tool"
    );
}

/// Claim safety, asserted against the store rather than against the log: a
/// keyed call that parks its run settles its commitment first, so the
/// identity is free while the run sleeps and a second run holding the same key
/// is never told the call is in flight.
#[tokio::test]
async fn a_sleeping_run_holds_no_idempotency_claim() {
    let server = scripted_server().await;
    let (agent, calls) = napping_agent(&server.uri(), Effect::Write, Some(DECLARED_KEY));
    let store = store();
    let clock = TestClock::new(START);
    let runtime = Runtime::with_hooks(store.clone(), clock.injected(), fixed_random());

    let first = fixed_run_id(61);
    runtime
        .start_with_id(&agent, first, json!("hold this"))
        .await
        .expect("the first run parks");
    assert_eq!(
        derive_state(&store.read_log(first).await.expect("log reads")).status,
        RunStatus::Sleeping { wake_at: WAKE_AT },
        "the first run is asleep"
    );

    // THE CLAIM STATE, while the run sleeps. A commitment with a completion
    // recorded is a settled one; an unfinished one is what refuses others.
    let commitment = store
        .lookup_call("hold", DECLARED_KEY)
        .await
        .expect("the lookup reads")
        .expect("the keyed call took a claim");
    assert_eq!(commitment.run_id, first);
    assert_eq!(
        commitment.completion_seq.map(|seq| seq.get()),
        Some(5),
        "the claim is settled at the completion the sleep follows"
    );

    // A second, independent run under the same key. Nothing blocks it: it
    // collects the settled call's recorded output, which is the sleep request,
    // and parks on the same deadline without executing the tool.
    let second = fixed_run_id(62);
    let outcome = runtime
        .start_with_id(&agent, second, json!("hold this"))
        .await
        .expect("a sleeping holder does not refuse a second run");
    assert!(
        matches!(
            outcome,
            RunOutcome::Parked {
                reason: ParkReason::Sleeping { wake_at },
                ..
            } if wake_at == WAKE_AT
        ),
        "the second run parks on the deduplicated request: {outcome:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "and the tool ran exactly once across both runs"
    );

    // Both wake and both finish, so the settled claim is not a dead end.
    for run_id in [first, second] {
        assert_eq!(
            drive_to_completion(&runtime, &agent, run_id, &store, &clock).await,
            json!("held and resumed")
        );
    }
}

/// Kill at every event boundary of the control log and continue from there:
/// the finished log must equal the control log exactly, whichever event the
/// process died after.
///
/// A continuation starts its clock where the control run's stood when the cut
/// happened, which is the timestamp of the last surviving event. Anything else
/// would compare a recovered run against a control that ran under a different
/// clock, and the recorded `now` observations would differ for that reason
/// rather than for a real one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_at_every_boundary_continues_identically() {
    let server = scripted_server().await;
    let control = {
        let (agent, _calls) = napping_agent(&server.uri(), Effect::Read, None);
        let store = store();
        let clock = TestClock::new(START);
        let runtime = Runtime::with_hooks(store.clone(), clock.injected(), fixed_random());
        let run_id = fixed_run_id(63);
        drive_to_completion(&runtime, &agent, run_id, &store, &clock).await;
        store.read_log(run_id).await.expect("control log reads")
    };
    assert_eq!(event_kinds(&control), COMPLETED_KINDS.to_vec());

    for cut in 0..=control.len() {
        let (agent, _calls) = napping_agent(&server.uri(), Effect::Read, None);
        let store = store();
        for envelope in &control[..cut] {
            store.append(envelope).await.expect("prefix event appends");
        }
        let start_at = control[..cut]
            .last()
            .map_or(START, |envelope| envelope.recorded_at);
        let clock = TestClock::new(start_at);
        let runtime = Runtime::with_hooks(store.clone(), clock.injected(), fixed_random());
        let run_id = fixed_run_id(63);

        drive_to_completion(&runtime, &agent, run_id, &store, &clock).await;
        let log: Vec<EventEnvelope> = store.read_log(run_id).await.expect("final log reads");
        assert_eq!(
            log, control,
            "cut {cut}: the continued log must equal the control log exactly"
        );
    }
}
