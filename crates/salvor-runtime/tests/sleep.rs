//! Durable timers at the runtime edge: a user-written flow sleeps between two
//! completed tool calls, the driver drops while it sleeps, and a later drive
//! picks the run up exactly where it left off.
//!
//! The flow below is deliberately not the built-in loop. Sleeping is a
//! control-flow decision an orchestration makes, so the proof belongs where a
//! caller would write it: `RunCtx::sleep_for`, `RunCtx::await_wake`, and
//! nothing else.
//!
//! # The clock
//!
//! [`TestClock`] is the injected clock and the test moves it by hand. That is
//! the whole apparatus: a sleeping run continues when its deadline arrives,
//! and "arrives" here means the test said so. Nothing sleeps in real time, and
//! no test waits on one.
//!
//! # Where the sleep sits
//!
//! Between two *completed* calls, never inside one. That is the rule
//! `RunCtx`'s module docs state and cannot enforce: a sleep recorded between a
//! claimed call's intent and its completion holds the claim for the length of
//! the sleep and strands a dangling write if the process dies. The flow here
//! is written the way callers must write theirs.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{TestTool, ToolBehavior, event_kinds, fixed_random, fixed_run_id};
use salvor_core::{Effect, Event, EventEnvelope, RunId, RunStatus, derive_state};
use salvor_runtime::{ClockFn, RunCtx, RuntimeError, ToolCallResult, Waking};
use salvor_store::{EventStore, RunSummary, SqliteStore, StoreError};
use salvor_tools::DynTool;
use serde_json::{Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

/// The instant every run below starts at.
const START: OffsetDateTime = datetime!(2026-07-09 12:00:00 UTC);

/// How long the flow sleeps. Half an hour: long enough that no wall clock
/// could cross it during a test run, so a passing wake can only come from the
/// test moving the clock.
const NAP: Duration = Duration::minutes(30);

const AGENT_HASH: &str = "sha256:nap-flow-v1";

/// A clock the test sets by hand, shared with every [`RunCtx`] a scenario
/// builds so the whole scenario reads one time.
#[derive(Clone)]
struct TestClock {
    now: Arc<Mutex<OffsetDateTime>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(START)),
        }
    }

    /// The injected clock function: envelope timestamps and live `now`
    /// observations both read it.
    fn injected(&self) -> ClockFn {
        let now = self.now.clone();
        Arc::new(move || *now.lock().expect("clock is not poisoned"))
    }

    fn read(&self) -> OffsetDateTime {
        *self.now.lock().expect("clock is not poisoned")
    }

    fn set(&self, instant: OffsetDateTime) {
        *self.now.lock().expect("clock is not poisoned") = instant;
    }
}

/// How one drive of the napping flow ended.
#[derive(Debug)]
enum FlowOutcome {
    /// The run is parked on its timer and may continue at this instant.
    Sleeping(OffsetDateTime),
    Completed(Value),
}

/// The user-written orchestration: begin, poll a tool, sleep, poll again,
/// complete. `nap` is a parameter only so one test can re-drive a recorded run
/// with a different duration and prove the divergence; every other caller
/// passes [`NAP`].
async fn nap_flow(
    ctx: &mut RunCtx,
    poll: &dyn DynTool,
    nap: Duration,
) -> Result<FlowOutcome, RuntimeError> {
    ctx.begin(AGENT_HASH, &json!({"order": "A-1"})).await?;

    let before = tool_output(
        ctx.tool_call(poll, &json!({"phase": "before"}), None)
            .await?,
    );

    // The sleep sits here, between a completed call and the next one.
    let wake_at = ctx.sleep_for(nap).await?;
    match ctx.await_wake().await? {
        Waking::Asleep { wake_at } => return Ok(FlowOutcome::Sleeping(wake_at)),
        Waking::Woken => {}
    }

    let after = tool_output(
        ctx.tool_call(poll, &json!({"phase": "after"}), None)
            .await?,
    );
    let output = json!({"before": before, "after": after, "woke_at": wake_at.to_string()});
    ctx.complete_run(&output).await?;
    Ok(FlowOutcome::Completed(output))
}

/// Unwraps the echo tool's output; anything else is a test bug.
fn tool_output(result: ToolCallResult) -> Value {
    match result {
        ToolCallResult::Output(output) => output,
        other => panic!("the echo tool must produce output, got {other:?}"),
    }
}

/// Builds a context over whatever the store already holds and drives the flow
/// once, exactly as a fresh process would.
async fn drive_once(
    store: Arc<dyn EventStore>,
    run_id: RunId,
    clock: &TestClock,
    poll: &dyn DynTool,
    nap: Duration,
) -> Result<FlowOutcome, RuntimeError> {
    let log = store.read_log(run_id).await?;
    let mut ctx = RunCtx::with_hooks(store, run_id, log, clock.injected(), fixed_random())?;
    nap_flow(&mut ctx, poll, nap).await
}

/// Drives until the run completes, moving the clock to the recorded wake
/// instant whenever a drive reports the run is asleep. This is the whole of
/// what a wake sweeper will later do for real; here the test is the sweeper.
async fn drive_to_completion(
    store: Arc<dyn EventStore>,
    run_id: RunId,
    clock: &TestClock,
    poll: &dyn DynTool,
) -> Result<Value, RuntimeError> {
    // A sleeping drive always advances the clock, and one advance is enough
    // for one sleep, so this bound cannot be reached by a correct flow.
    for _ in 0..8 {
        match drive_once(store.clone(), run_id, clock, poll, NAP).await? {
            FlowOutcome::Completed(output) => return Ok(output),
            FlowOutcome::Sleeping(wake_at) => {
                assert!(
                    clock.read() < wake_at,
                    "a run reported asleep past its own deadline"
                );
                clock.set(wake_at);
            }
        }
    }
    panic!("the flow neither completed nor made progress");
}

/// The echo tool the flow polls, with its execution counter.
fn poll_tool() -> (TestTool, Arc<AtomicUsize>) {
    TestTool::new("poll", Effect::Read, ToolBehavior::Echo)
}

/// A fresh in-memory store.
fn store() -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::in_memory().expect("store opens"))
}

/// The log a completed run records, and the shape every scenario below is
/// measured against.
const COMPLETED_KINDS: [&str; 9] = [
    "RunStarted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "NowObserved",
    "SleepStarted",
    "SleepCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "RunCompleted",
];

/// A run that reaches its sleep parks there: the log ends at `SleepStarted`,
/// the fold says sleeping, and the driver can drop the context and stop. Then
/// the other half of the same rule: a driver that comes back too early finds
/// the run still asleep and records nothing, so no amount of re-driving can
/// wake a run before its deadline.
#[tokio::test]
async fn a_run_sleeps_parks_and_cannot_be_woken_early() {
    let store = store();
    let run_id = fixed_run_id(60);
    let clock = TestClock::new();
    let (poll, calls) = poll_tool();

    let outcome = drive_once(store.clone(), run_id, &clock, &poll, NAP)
        .await
        .expect("the first drive parks");
    let FlowOutcome::Sleeping(wake_at) = outcome else {
        panic!("expected the run to park on its timer");
    };
    assert_eq!(wake_at, START + NAP, "the deadline is the recorded reading");

    let parked = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&parked),
        [
            "RunStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "NowObserved",
            "SleepStarted"
        ],
        "nothing is recorded past the started sleep"
    );
    assert_eq!(
        derive_state(&parked).status,
        RunStatus::Sleeping { wake_at }
    );

    // Re-drive with the clock unmoved: still asleep, still nothing recorded.
    let outcome = drive_once(store.clone(), run_id, &clock, &poll, NAP)
        .await
        .expect("an early drive parks again");
    assert!(matches!(outcome, FlowOutcome::Sleeping(again) if again == wake_at));
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        parked,
        "an early drive appends nothing at all"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the replayed poll never re-executed"
    );
}

/// Past the deadline the run continues, and the wake is recorded once. A
/// further drive over the finished log replays the whole thing, wake
/// included, and appends nothing: the recorded completion is what makes the
/// run carry on, not a second look at the clock.
#[tokio::test]
async fn a_recorded_wake_continues_the_run_and_replays() {
    let store = store();
    let run_id = fixed_run_id(61);
    let clock = TestClock::new();
    let (poll, calls) = poll_tool();

    let output = drive_to_completion(store.clone(), run_id, &clock, &poll)
        .await
        .expect("the run completes once its deadline passes");
    assert_eq!(output["before"], json!({"echo": {"phase": "before"}}));
    assert_eq!(output["after"], json!({"echo": {"phase": "after"}}));

    let finished = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&finished), COMPLETED_KINDS);
    assert!(matches!(
        derive_state(&finished).status,
        RunStatus::Completed { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "each poll executed once");

    // A drive over the finished log, with the clock moved somewhere else
    // entirely, replays every step and changes nothing.
    clock.set(START + Duration::days(30));
    let replayed = drive_once(store.clone(), run_id, &clock, &poll, NAP)
        .await
        .expect("the finished run replays");
    assert!(matches!(replayed, FlowOutcome::Completed(ref again) if *again == output));
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        finished,
        "a replayed drive appends nothing"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "and executes nothing");
}

/// `sleep_for` is exactly `now() + duration`, recorded: the reading lands in
/// the log as a `NowObserved` and the wake instant is derived from it, so a
/// replay under a completely different ambient clock reproduces the same
/// instant, byte for byte on the wire.
#[tokio::test]
async fn sleep_for_derives_its_instant_from_the_recorded_reading() {
    let store = store();
    let run_id = fixed_run_id(62);
    let clock = TestClock::new();
    let (poll, _calls) = poll_tool();

    drive_to_completion(store.clone(), run_id, &clock, &poll)
        .await
        .expect("the run completes");
    let finished = store.read_log(run_id).await.expect("log reads");

    assert_eq!(
        finished[3].event,
        Event::NowObserved { now: START },
        "the reading the instant is derived from is itself recorded"
    );
    assert_eq!(
        finished[4].event,
        Event::SleepStarted {
            wake_at: START + NAP
        },
        "and the recorded instant is that reading plus the duration"
    );

    let before = serde_json::to_string(&finished).expect("serialize");
    clock.set(START - Duration::days(365));
    drive_once(store.clone(), run_id, &clock, &poll, NAP)
        .await
        .expect("the recorded run replays under a different clock");
    let after = serde_json::to_string(&store.read_log(run_id).await.expect("log reads"))
        .expect("serialize");
    assert_eq!(before, after, "the replayed log is byte for byte the same");
}

/// A re-drive whose recomputed wake instant differs from the recorded one
/// diverges. The instant is a derivation, and a derivation that changed would
/// put the run under a deadline the log does not hold.
#[tokio::test]
async fn a_recomputed_wake_instant_diverges() {
    let store = store();
    let run_id = fixed_run_id(63);
    let clock = TestClock::new();
    let (poll, _calls) = poll_tool();

    drive_once(store.clone(), run_id, &clock, &poll, NAP)
        .await
        .expect("the first drive parks");
    let parked = store.read_log(run_id).await.expect("log reads");

    let error = drive_once(
        store.clone(),
        run_id,
        &clock,
        &poll,
        NAP + Duration::nanoseconds(1),
    )
    .await
    .expect_err("a different derivation must not replay");
    assert!(
        matches!(error, RuntimeError::Replay(_)),
        "expected a divergence, got {error}"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        parked,
        "a diverging drive records nothing"
    );
}

/// A store wrapper that allows a budgeted number of appends and then fails
/// every later one without touching the wrapped store, simulating a process
/// death at an exact event boundary. The same device
/// `kill_resume.rs` uses, kept local because that test's reference run is a
/// different one.
struct KillStore {
    inner: Arc<dyn EventStore>,
    remaining: AtomicI64,
}

#[async_trait]
impl EventStore for KillStore {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Backend("simulated crash".to_owned()));
        }
        self.inner.append(envelope).await
    }

    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        self.inner.read_log(run_id).await
    }

    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        self.inner.list_runs().await
    }

    async fn claim_call(
        &self,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<salvor_store::CallClaim, StoreError> {
        self.inner.claim_call(claimant).await
    }

    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<salvor_store::CallCommitment>, StoreError> {
        self.inner.lookup_call(tool, idempotency_key).await
    }

    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Backend("simulated crash".to_owned()));
        }
        self.inner.append_settling_call(envelope, claimant).await
    }
}

/// Killed at every boundary of a run that sleeps mid-way, recovery produces
/// the control run's log exactly: same events, same positions, same
/// timestamps. The sleep is a boundary like any other, and the two boundaries
/// only a timer has (a started sleep with no wake, and the wake itself) are
/// covered by the sweep along with the rest.
///
/// Timestamps compare because the clock moves on a recorded condition rather
/// than on its own: it advances when a drive reports the run asleep, which
/// happens at the same point of the same log in every phase of every
/// scenario. Events before the wake carry the start instant and events from
/// the wake onward carry the wake instant, whichever drive persisted them.
#[tokio::test]
async fn a_kill_at_every_boundary_recovers_the_same_log() {
    let control_store = store();
    let control_clock = TestClock::new();
    let (poll, _calls) = poll_tool();
    drive_to_completion(
        control_store.clone(),
        fixed_run_id(64),
        &control_clock,
        &poll,
    )
    .await
    .expect("the control run completes");
    let control = control_store
        .read_log(fixed_run_id(64))
        .await
        .expect("log reads");
    assert_eq!(control.len(), COMPLETED_KINDS.len());

    for allow in 1..control.len() {
        let store = store();
        let run_id = fixed_run_id(70 + u8::try_from(allow).expect("small"));
        let clock = TestClock::new();
        let (poll, _calls) = poll_tool();

        let killed: Arc<dyn EventStore> = Arc::new(KillStore {
            inner: store.clone(),
            remaining: AtomicI64::new(i64::try_from(allow).expect("small")),
        });
        let error = drive_to_completion(killed, run_id, &clock, &poll)
            .await
            .expect_err("the kill store aborts the drive");
        assert!(
            matches!(error, RuntimeError::Store(_)),
            "cut at {allow}: {error}"
        );
        assert_eq!(
            store.read_log(run_id).await.expect("log reads").len(),
            allow,
            "cut at {allow}: exactly the budgeted number of events persisted"
        );

        drive_to_completion(store.clone(), run_id, &clock, &poll)
            .await
            .expect("recovery completes the run");
        let recovered = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            recovered.len(),
            control.len(),
            "cut at {allow}: the recovered run records the same events"
        );
        for (index, (got, want)) in recovered.iter().zip(control.iter()).enumerate() {
            assert_eq!(got.seq, want.seq, "cut at {allow}, event {index}");
            assert_eq!(got.event, want.event, "cut at {allow}, event {index}");
            assert_eq!(
                got.recorded_at, want.recorded_at,
                "cut at {allow}, event {index}"
            );
        }
    }
}
