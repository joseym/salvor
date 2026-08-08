//! The wake sweeper: what a server does about a run whose durable timer has
//! come due, and what it refuses to do about one that has not.
//!
//! # The clock
//!
//! Every state here injects a fixed clock (`app_state` does it for the whole
//! suite), and the sweeper reads that same clock, so "due" is decided by where
//! a seeded deadline sits relative to a constant rather than by how long a test
//! took to run. Nothing below sleeps for a deadline to pass.
//!
//! # Why the logs are seeded by hand
//!
//! A sleeping run reaches a store three ways: a caller's own `RunCtx` flow, a
//! tool that returned the sleep outcome (both driven at the runtime tier, in
//! `sleep.rs` and `tool_sleep.rs`), or a log written directly, as here. The
//! questions this file asks are all about which runs the server reaches for
//! and which it leaves alone, and a seeded log answers them without a model
//! script standing between the deadline and the assertion.

mod common;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    memory_store, open_store, register_agent, sample_toml,
};
use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber};
use salvor_runtime::due_runs;
use salvor_server::{AgentFactory, AppState};
use salvor_store::EventStore;
use serde_json::json;
use tempfile::tempdir;
use time::OffsetDateTime;
use time::macros::datetime;

/// The instant `common::fixed_clock` reads, which is the instant the sweeper
/// measures every deadline against.
const NOW: OffsetDateTime = datetime!(2026-07-10 12:00:00 UTC);

/// Writes a run that folds to `sleeping` with exactly `wake_at`, under
/// `agent_hash`. Two events: the head, and the started sleep.
async fn seed_sleeping(
    store: &Arc<dyn EventStore>,
    agent_hash: &str,
    wake_at: OffsetDateTime,
) -> RunId {
    let run_id = RunId::new();
    for (seq, event) in [
        Event::RunStarted {
            agent_def_hash: agent_hash.to_owned(),
            input: json!("go"),
            labels: None,
        },
        Event::SleepStarted { wake_at },
    ]
    .into_iter()
    .enumerate()
    {
        let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq as u64), NOW, event);
        store.append(&envelope).await.expect("append");
    }
    run_id
}

/// Wraps a factory with a counter of how many agents it was asked to build.
///
/// This is the sweeper's own footprint made visible. Re-driving an agent run
/// rebuilds its agent from the registered definition before anything is
/// spawned, so a build the test never asked for is proof that the sweeper
/// reached the real resume path on its own.
fn counting_factory(inner: AgentFactory, builds: Arc<AtomicUsize>) -> AgentFactory {
    Arc::new(move |definition| {
        builds.fetch_add(1, Ordering::SeqCst);
        let inner = inner.clone();
        Box::pin(async move { inner(definition).await })
            as Pin<Box<dyn std::future::Future<Output = _> + Send>>
    })
}

/// A state whose factory builds the suite's stock agent, with a build counter.
async fn state_with_counter(
    store: Arc<dyn EventStore>,
) -> (AppState, Arc<AtomicUsize>, wiremock::MockServer) {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let builds = Arc::new(AtomicUsize::new(0));
    let state = app_state(store, counting_factory(factory, builds.clone()));
    (state, builds, model)
}

/// A due run is re-driven with no client call at all: the server rebuilds the
/// agent and hands the run to a driver task off its own timer.
///
/// The registration is over HTTP because that is how a definition genuinely
/// arrives; everything after it is the server acting alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_past_its_deadline_is_woken_by_the_server_alone() {
    let store = memory_store();
    let (state, builds, _model) = state_with_counter(store.clone()).await;
    // A short interval, for the same reason the stream poll is shortened in
    // this suite: to make the loop observable without waiting on a wall clock.
    let state = state.with_wake_interval(Duration::from_millis(20));
    let server = TestServer::spawn(state).await;

    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;
    let builds_after_register = builds.load(Ordering::SeqCst);

    let run_id = seed_sleeping(&store, &agent, NOW - time::Duration::hours(1)).await;

    // No request is made about this run. The only thing that could reach it is
    // the sweeper.
    for _ in 0..300 {
        if builds.load(Ordering::SeqCst) > builds_after_register {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the sweeper never re-drove run {}; it was due an hour before the injected clock",
        run_id.as_uuid()
    );
}

/// A single pass, called directly, reports which runs it drove. That is the
/// selection rule and the routing in one assertion: the due run is driven, the
/// run whose deadline is ahead is not touched, and neither log is disturbed by
/// a pass that could not do anything with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_drives_the_due_run_and_leaves_the_rest_alone() {
    let store = memory_store();
    let (state, _builds, _model) = state_with_counter(store.clone()).await;
    let server = TestServer::spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let due = seed_sleeping(&store, &agent, NOW - time::Duration::days(1)).await;
    let not_due = seed_sleeping(&store, &agent, NOW + time::Duration::days(1)).await;

    let driven = salvor_server::sweep(&state).await;
    assert_eq!(driven, vec![due], "exactly the run past its deadline");

    let untouched = store.read_log(not_due).await.expect("log reads");
    assert_eq!(untouched.len(), 2, "the run not yet due is not appended to");
    assert_eq!(
        due_runs(store.as_ref(), NOW)
            .await
            .expect("selection reads")
            .len(),
        1,
        "and it is still not due"
    );
}

/// A run a driver in this process is already on is skipped, however overdue it
/// is. Two drivers on one run would both be replaying and appending to the same
/// log, which is the one thing the active set exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_already_being_driven_here_is_skipped() {
    let store = memory_store();
    let (state, builds, _model) = state_with_counter(store.clone()).await;
    let server = TestServer::spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let run_id = seed_sleeping(&store, &agent, NOW - time::Duration::hours(6)).await;

    // Claim the run the way a driver task does, synchronously, before the
    // sweeper looks.
    state.begin_run(run_id);
    let before = builds.load(Ordering::SeqCst);
    assert!(
        salvor_server::sweep(&state).await.is_empty(),
        "a run with a driver on it is not driven again"
    );
    assert_eq!(
        builds.load(Ordering::SeqCst),
        before,
        "and the skip happens before anything is rebuilt"
    );

    // The claim released, the very same run is due and driven.
    state.end_run(run_id);
    assert_eq!(
        salvor_server::sweep(&state).await,
        vec![run_id],
        "nothing about the run changed; only the claim did"
    );
}

/// A run whose agent this server has never held cannot be woken here. That is
/// reported and skipped, not raised: the run keeps its log, stays sleeping, and
/// stays due, so registering the definition is all it takes for a later pass to
/// wake it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unregistered_agent_leaves_the_run_asleep() {
    let store = memory_store();
    let (state, _builds, _model) = state_with_counter(store.clone()).await;
    let run_id = seed_sleeping(
        &store,
        "sha256:never-registered-here",
        NOW - time::Duration::hours(1),
    )
    .await;

    assert!(
        salvor_server::sweep(&state).await.is_empty(),
        "the server cannot wake a run it has no definition for"
    );
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 2, "the run's log is untouched");
    assert_eq!(
        due_runs(store.as_ref(), NOW).await.expect("reads").len(),
        1,
        "and it is still due, so a later pass gets another chance"
    );
}

/// One run the pass cannot do anything with does not end the pass. The due runs
/// after it still get their turn, which is what keeps a single stale agent hash
/// from freezing every timer in the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_will_not_drive_does_not_stop_the_pass() {
    let store = memory_store();
    let (state, _builds, _model) = state_with_counter(store.clone()).await;
    let server = TestServer::spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    // The unwakeable run is the more overdue of the two, so the pass meets it
    // first and has to carry on past it to reach the other.
    seed_sleeping(
        &store,
        "sha256:never-registered-here",
        NOW - time::Duration::days(5),
    )
    .await;
    let wakeable = seed_sleeping(&store, &agent, NOW - time::Duration::hours(1)).await;

    assert_eq!(
        salvor_server::sweep(&state).await,
        vec![wakeable],
        "the run after the failure is still driven"
    );
}

/// `--wake-interval 0` on the state is the off switch: the loop never runs, so
/// a due run stays asleep until something else wakes it. Proven by leaving a
/// due run under a real server for longer than any interval a test would use
/// and finding it untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_interval_turns_the_sweeper_off() {
    let store = memory_store();
    let (state, builds, _model) = state_with_counter(store.clone()).await;
    let state = state.with_wake_interval(Duration::ZERO);
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;
    let builds_after_register = builds.load(Ordering::SeqCst);

    let run_id = seed_sleeping(&store, &agent, NOW - time::Duration::days(30)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        builds.load(Ordering::SeqCst),
        builds_after_register,
        "nothing rebuilt the agent, so nothing swept"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads").len(),
        2,
        "and the run is exactly as it was seeded"
    );
}

/// A sleeping run's stream ends rather than polling for the length of the nap,
/// and the end frame carries the deadline, so a client learns when to come back
/// instead of holding a connection open for hours.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sleeping_runs_stream_ends_and_reports_its_deadline() {
    let dir = tempdir().expect("tempdir");
    let store = open_store(&dir.path().join("salvor.db"));
    let (state, _builds, _model) = state_with_counter(store.clone()).await;
    // Off, so nothing wakes the run out from under the stream being read.
    let state = state.with_wake_interval(Duration::ZERO);
    let server = TestServer::spawn(state).await;

    let wake_at = NOW + time::Duration::days(7);
    let run_id = seed_sleeping(&store, "sha256:any", wake_at).await;

    let client = reqwest::Client::new();
    let mut stream = SseReader::open(
        &client,
        &server.base,
        &run_id.as_uuid().to_string(),
        None,
        None,
        None,
    )
    .await;
    let frames = stream.read_to_end().await;

    let end = frames.last().expect("the stream sends frames");
    assert!(end.is_end(), "the stream closes on a resting run");
    let status = &end.json()["status"];
    assert_eq!(status["state"], "sleeping", "the state it rested at");
    assert_eq!(
        status["wake_at"], "2026-07-17T12:00:00Z",
        "and the instant it may continue at"
    );
    assert_eq!(
        frames.len(),
        3,
        "both recorded events, then the end frame: {frames:?}"
    );
}

/// The resume endpoint answers a sleeping run by its deadline, not by its
/// state alone: an early resume is refused with the instant and the wait, and
/// the very same run past the same instant re-drives. Both go through
/// `classify`, so the two answers cannot come from two different rules.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_early_resume_is_refused_and_a_due_one_drives() {
    let store = memory_store();
    let (state, _builds, _model) = state_with_counter(store.clone()).await;
    // Off, so the refusal below is answered about a run nothing else touched.
    let state = state.with_wake_interval(Duration::ZERO);
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let early = seed_sleeping(&store, &agent, NOW + time::Duration::minutes(29)).await;
    let refusal = client
        .post(format!(
            "{}/v1/runs/{}/resume",
            server.base,
            early.as_uuid()
        ))
        .json(&json!({}))
        .send()
        .await
        .expect("the request is made");
    assert_eq!(refusal.status(), 409, "a state conflict, not a bad request");
    let body: serde_json::Value = refusal.json().await.expect("a json body");
    assert_eq!(body["error"]["code"], "still_sleeping");
    assert_eq!(
        body["error"]["details"]["wake_at"], "2026-07-10T12:29:00Z",
        "the deadline it is waiting for"
    );
    assert_eq!(
        body["error"]["details"]["remaining_seconds"], 1740,
        "and how long is left of the wait"
    );
    assert_eq!(
        store.read_log(early).await.expect("log reads").len(),
        2,
        "a refused resume records nothing"
    );

    // The same verb, the same state, a deadline that has passed: driven.
    let due = seed_sleeping(&store, &agent, NOW - time::Duration::minutes(1)).await;
    let accepted = client
        .post(format!("{}/v1/runs/{}/resume", server.base, due.as_uuid()))
        .json(&json!({}))
        .send()
        .await
        .expect("the request is made");
    assert_eq!(
        accepted.status(),
        202,
        "a due run is re-driven, which is the whole of waking"
    );
}
