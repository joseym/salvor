//! Nothing happens twice **across** independent runs, not only within one
//! run's kill and resume.
//!
//! The scenario is the one a field tester reproduced against a real store: a
//! payout agent with one naive `pay_claim` tool that has no deduplication of
//! its own, run twice as two separate `salvor run` invocations over the same
//! store. Both runs completed, and the provider's ledger ended up with two
//! lines and two charge ids. The money left twice.
//!
//! What closes it is the idempotency key, arbitrated by the store: the tool
//! declares that a call for claim `wreck-9931` *is* the payout for claim
//! `wreck-9931`, and the store lets exactly one run execute that. The tests
//! here pin the whole shape of that promise, including its edges:
//!
//! - two sequential runs pay once, and the second one's log says whose payment
//!   it is reporting;
//! - two racing runs pay once;
//! - a write with no declared key is left exactly as it was, because a call
//!   with no identity cannot be deduplicated and pretending otherwise would be
//!   a lie;
//! - a recorded log replays with no store lookup of any kind;
//! - a run killed between a deduplicated intent and its copy recovers, while a
//!   run killed mid-payment still parks for a human and blocks the next run.
//!
//! The ledger in these tests is a `Vec<String>` playing the part of the
//! payment processor's own record: one line per payout that actually left the
//! building. Every assertion about "how many times did this happen" is made
//! against that, never against salvor's own log, because the log is the thing
//! under test.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{
    ScriptedModel, agent_builder, fixed_clock, fixed_random, fixed_run_id, text_response,
    tool_use_response,
};
use salvor_core::{
    DedupOrigin, Effect, Event, EventEnvelope, RunId, RunStatus, RunSummary, derive_state,
};
use salvor_runtime::{Agent, RunOutcome, Runtime, RuntimeError};
use salvor_store::{CallClaim, CallClaimant, CallCommitment, EventStore, SqliteStore, StoreError};
use salvor_tools::{DynTool, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use wiremock::MockServer;

/// The claim the whole file pays out, taken from the field tester's input.
const CLAIM: &str = "wreck-9931";

/// The payout the agent is asked to make.
fn payout_input() -> Value {
    json!({"claim_id": CLAIM, "amount_cents": 483_200, "currency": "USD"})
}

/// The payment processor's own ledger: one line per payout that really left
/// the building, exactly like the `provider-ledger.jsonl` the field tester
/// ended up with two lines in.
#[derive(Default)]
struct Ledger {
    lines: Mutex<Vec<Value>>,
    charges: AtomicUsize,
}

impl Ledger {
    /// Records a payout and mints a fresh charge id, the way a processor does:
    /// a second execution is a second charge, distinguishable from the first.
    fn charge(&self, input: &Value) -> Value {
        let charge_id = format!("po_{}", self.charges.fetch_add(1, Ordering::SeqCst));
        let line = json!({
            "claim_id": input.get("claim_id").cloned().unwrap_or(Value::Null),
            "amount_cents": input.get("amount_cents").cloned().unwrap_or(Value::Null),
            "charge_id": charge_id,
        });
        self.lines.lock().expect("ledger lock").push(line.clone());
        line
    }

    fn len(&self) -> usize {
        self.lines.lock().expect("ledger lock").len()
    }
}

/// The field tester's tool: a `Write` that moves money and has no
/// deduplication logic of its own.
///
/// `declares_key` is the one thing that varies. With it on, the tool names the
/// effect a call is (`pay_claim:wreck-9931`) and the store can hold the line.
/// With it off, the tool is exactly the first draft the field tester wrote, and
/// the tests prove that draft's behavior is unchanged.
struct PayClaim {
    ledger: Arc<Ledger>,
    declares_key: bool,
}

#[async_trait]
impl DynTool for PayClaim {
    fn name(&self) -> &str {
        "pay_claim"
    }
    fn description(&self) -> &str {
        "wires a salvage payout to the claimant on file"
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn idempotency_key(&self, input: &Value) -> Option<String> {
        if !self.declares_key {
            return None;
        }
        let claim = input.get("claim_id")?.as_str()?;
        Some(format!("pay_claim:{claim}"))
    }

    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        Ok(ToolOutcome::Output(self.ledger.charge(&input)))
    }
}

/// A two-turn script: the model asks for the payout, then confirms it.
async fn payout_model() -> MockServer {
    ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_pay", "pay_claim", payout_input(), 100, 10),
        ),
        (3, text_response("paid", 120, 12)),
    ])
    .await
}

/// The payout agent, over a ledger the test can inspect.
fn payout_agent(server_uri: &str, ledger: &Arc<Ledger>, declares_key: bool) -> Agent {
    agent_builder(server_uri)
        .tool_dyn(Box::new(PayClaim {
            ledger: Arc::clone(ledger),
            declares_key,
        }))
        .build()
        .expect("agent builds")
}

/// The `ToolCallCompleted` in a log, with its recorded dedup origin.
fn recorded_completion(log: &[EventEnvelope]) -> (Value, Option<DedupOrigin>) {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallCompleted {
                output,
                deduplicated_from,
                ..
            } => Some((output.clone(), *deduplicated_from)),
            _ => None,
        })
        .expect("the log records a tool completion")
}

/// The correlation sequence of a log's `ToolCallRequested`.
fn intent_seq(log: &[EventEnvelope]) -> u64 {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallRequested { seq, .. } => Some(seq.get()),
            _ => None,
        })
        .expect("the log records a tool intent")
}

fn output_of(outcome: &RunOutcome) -> &Value {
    match outcome {
        RunOutcome::Completed { output, .. } => output,
        RunOutcome::Parked { reason, .. } => panic!("expected a completed run, parked: {reason:?}"),
    }
}

/// The field tester's repro, as a test. Two independent runs against one
/// store, same agent, same input, one after the other.
///
/// Before this mechanism existed both runs completed and the provider ledger
/// grew two lines with two charge ids. Now the second run completes too, which
/// matters (refusing would have been a different product), but it completes by
/// reporting the payment the first run made, and its log says so.
#[tokio::test]
async fn two_independent_runs_pay_the_claim_once() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, true);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random());

    let first_run = fixed_run_id(1);
    let second_run = fixed_run_id(2);

    let first = runtime
        .start_with_id(&agent, first_run, payout_input())
        .await
        .expect("the first run drives");
    let second = runtime
        .start_with_id(&agent, second_run, payout_input())
        .await
        .expect("the second run drives");

    assert_eq!(
        ledger.len(),
        1,
        "the provider's ledger must hold exactly one payout for one claim"
    );
    assert!(matches!(first, RunOutcome::Completed { .. }));
    assert!(
        matches!(second, RunOutcome::Completed { .. }),
        "the second run must complete, not fail: it asked for something that is already true"
    );
    assert_eq!(
        output_of(&first),
        output_of(&second),
        "both runs must report the same payment"
    );

    let first_log = store.read_log(first_run).await.expect("first log reads");
    let second_log = store.read_log(second_run).await.expect("second log reads");

    let (first_output, first_origin) = recorded_completion(&first_log);
    let (second_output, second_origin) = recorded_completion(&second_log);
    assert_eq!(
        first_origin, None,
        "the run that actually paid must not claim to have copied anyone"
    );
    assert_eq!(
        first_output, second_output,
        "the copied output must be the recorded one, charge id and all"
    );
    assert_eq!(
        second_origin,
        Some(DedupOrigin {
            run_id: first_run,
            seq: salvor_core::SequenceNumber::new(intent_seq(&first_log)),
        }),
        "the second run's completion must name the run whose payment it is reporting"
    );

    // The second run still recorded an intent. It did ask; the honest record is
    // that it asked and that the answer was already settled.
    assert_eq!(
        second_log
            .iter()
            .filter(|e| matches!(e.event, Event::ToolCallRequested { .. }))
            .count(),
        1,
        "a deduplicated call still records its write-ahead intent"
    );
}

/// A store wrapper that holds every claim at a barrier, so two drivers are
/// provably at the arbitration edge at the same instant.
///
/// This is what makes the race a race without a sleep anywhere: the barrier
/// releases both tasks together, and whatever the store does next is genuine
/// contention rather than a scheduling accident.
struct GatedStore {
    inner: Arc<dyn EventStore>,
    gate: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl EventStore for GatedStore {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        self.inner.append(envelope).await
    }
    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        self.inner.read_log(run_id).await
    }
    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        self.inner.list_runs().await
    }
    async fn claim_call(&self, claimant: CallClaimant<'_>) -> Result<CallClaim, StoreError> {
        self.gate.wait().await;
        self.inner.claim_call(claimant).await
    }
    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<CallCommitment>, StoreError> {
        self.inner.lookup_call(tool, idempotency_key).await
    }
    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        self.inner.append_settling_call(envelope, claimant).await
    }
}

/// Two live drivers racing for the same claim. The money moves once.
///
/// Exactly one run is granted the identity and executes. The other meets the
/// winner either finished (and copies it) or still working (and refuses, having
/// recorded nothing, which is the only honest answer while the outcome is
/// unknown). Both endings are asserted, because which one a given scheduling
/// produces is not something a test should pretend to control; what is not
/// negotiable, and is asserted unconditionally, is that the ledger holds one
/// line.
///
/// A refused run is then simply run again, and finishes as the duplicate it
/// was, so both logs end up coherent and the loser's completion names the
/// winner either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_racing_runs_pay_the_claim_once() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let inner: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let gated: Arc<dyn EventStore> = Arc::new(GatedStore {
        inner: Arc::clone(&inner),
        gate: Arc::new(tokio::sync::Barrier::new(2)),
    });

    let runs = [fixed_run_id(11), fixed_run_id(12)];
    let mut handles = Vec::new();
    for run_id in runs {
        let uri = server.uri();
        let ledger = Arc::clone(&ledger);
        let store = Arc::clone(&gated);
        handles.push(tokio::spawn(async move {
            let agent = payout_agent(&uri, &ledger, true);
            let runtime = Runtime::with_hooks(store, fixed_clock(), fixed_random());
            (
                run_id,
                runtime.start_with_id(&agent, run_id, payout_input()).await,
            )
        }));
    }

    let mut refused = Vec::new();
    let mut completed = Vec::new();
    for handle in handles {
        let (run_id, result) = handle.await.expect("driver task joined");
        match result {
            Ok(RunOutcome::Completed { .. }) => completed.push(run_id),
            Err(RuntimeError::CallInFlight { holder, .. }) => refused.push((run_id, holder)),
            other => panic!("unexpected driver outcome for {run_id:?}: {other:?}"),
        }
    }

    assert_eq!(
        ledger.len(),
        1,
        "two runs racing for one claim must produce exactly one payout"
    );
    assert_eq!(
        completed.len() + refused.len(),
        2,
        "both drivers must reach a definite answer"
    );

    // Whatever the schedule produced, one run executed. Finish any run that was
    // refused while the winner was still working; it now has a settled
    // commitment to copy.
    for (run_id, holder) in &refused {
        assert!(
            completed.contains(holder),
            "a refusal must name the run that went on to complete"
        );
        assert!(
            inner
                .read_log(*run_id)
                .await
                .expect("log reads")
                .iter()
                .all(|e| !matches!(e.event, Event::ToolCallRequested { .. })),
            "a refused run must not have recorded an intent for a call it never made"
        );
        let agent = payout_agent(&server.uri(), &ledger, true);
        let runtime = Runtime::with_hooks(Arc::clone(&inner), fixed_clock(), fixed_random());
        runtime
            .recover(&agent, *run_id)
            .await
            .expect("a refused run finishes once the holder has settled");
    }

    assert_eq!(
        ledger.len(),
        1,
        "finishing the refused run must not pay a second time"
    );

    // Exactly one log witnessed the payment; the other copied it and says so.
    let mut witnessed = Vec::new();
    let mut copied = Vec::new();
    for run_id in runs {
        let log = inner.read_log(run_id).await.expect("log reads");
        match recorded_completion(&log).1 {
            None => witnessed.push(run_id),
            Some(origin) => copied.push((run_id, origin.run_id)),
        }
    }
    assert_eq!(witnessed.len(), 1, "exactly one run may have witnessed it");
    assert_eq!(copied.len(), 1);
    assert_eq!(
        copied[0].1, witnessed[0],
        "the copy must point at the run that made the payment"
    );
}

/// The control: a write with no declared key behaves exactly as it always did.
///
/// This is the honest boundary of the mechanism, and it is a test rather than a
/// paragraph because it is the part a reader will most want to disbelieve. A
/// call with no idempotency key has no identity, and there is nothing to
/// deduplicate on. Two runs pay twice, exactly as the field tester saw, and no
/// commitment is created behind anyone's back.
#[tokio::test]
async fn a_write_with_no_declared_key_is_unchanged() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, false);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random());

    let first_run = fixed_run_id(21);
    let second_run = fixed_run_id(22);
    runtime
        .start_with_id(&agent, first_run, payout_input())
        .await
        .expect("the first run drives");
    runtime
        .start_with_id(&agent, second_run, payout_input())
        .await
        .expect("the second run drives");

    assert_eq!(
        ledger.len(),
        2,
        "a keyless write cannot be deduplicated, and must not silently be"
    );
    for run_id in [first_run, second_run] {
        let log = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            recorded_completion(&log).1,
            None,
            "a keyless call's completion carries no origin"
        );
        assert!(
            log.iter().any(|envelope| matches!(
                &envelope.event,
                Event::ToolCallRequested {
                    idempotency_key: None,
                    ..
                }
            )),
            "the recorded intent must still carry no key"
        );
    }
    assert_eq!(
        store
            .lookup_call("pay_claim", "pay_claim:wreck-9931")
            .await
            .expect("lookup"),
        None,
        "a keyless call must not have created a commitment"
    );
}

/// A store that answers reads and refuses to be asked anything about
/// deduplication, by panicking.
///
/// Replay purity is a claim about what is *not* consulted, and the only way to
/// test a negative is to make the forbidden call impossible to make quietly.
struct NoCommitmentStore(Arc<dyn EventStore>);

#[async_trait]
impl EventStore for NoCommitmentStore {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        self.0.append(envelope).await
    }
    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        self.0.read_log(run_id).await
    }
    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        self.0.list_runs().await
    }
    async fn claim_call(&self, _claimant: CallClaimant<'_>) -> Result<CallClaim, StoreError> {
        panic!("replay claimed a call identity");
    }
    async fn lookup_call(
        &self,
        _tool: &str,
        _idempotency_key: &str,
    ) -> Result<Option<CallCommitment>, StoreError> {
        panic!("replay looked up a call commitment");
    }
    async fn append_settling_call(
        &self,
        _envelope: &EventEnvelope,
        _claimant: CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        panic!("replay settled a call commitment");
    }
}

/// Replaying a fully recorded run touches no cross-run state at all, and the
/// tool never runs.
///
/// The log is written by a keyed run, so it is exactly the kind of log that
/// *could* tempt a lookup, and then it is replayed through a store that
/// panics if anything asks it about a commitment. Recovering an already
/// completed run walks the whole log, which is the walk under test.
#[tokio::test]
async fn a_recorded_log_replays_without_consulting_the_store() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, true);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(31);

    let live = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random());
    let recorded = live
        .start_with_id(&agent, run_id, payout_input())
        .await
        .expect("the run drives");
    let recorded_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(ledger.len(), 1);

    let sealed: Arc<dyn EventStore> = Arc::new(NoCommitmentStore(Arc::clone(&store)));
    let replaying = Runtime::with_hooks(sealed, fixed_clock(), fixed_random());
    let replayed = replaying
        .recover(&agent, run_id)
        .await
        .expect("the recorded run replays");

    assert_eq!(
        output_of(&recorded),
        output_of(&replayed),
        "replay must reproduce the recorded output"
    );
    assert_eq!(
        ledger.len(),
        1,
        "replay must not execute the tool a second time"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        recorded_log,
        "replaying a complete log must append nothing"
    );
}

/// A store wrapper that allows a budgeted number of appends and then fails
/// every later one, simulating a process death at an exact event boundary.
///
/// Settling appends count against the same budget, because they are appends: a
/// kill that let them through would cut the run somewhere other than where the
/// test asked for.
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
    async fn claim_call(&self, claimant: CallClaimant<'_>) -> Result<CallClaim, StoreError> {
        self.inner.claim_call(claimant).await
    }
    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<CallCommitment>, StoreError> {
        self.inner.lookup_call(tool, idempotency_key).await
    }
    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Backend("simulated crash".to_owned()));
        }
        self.inner.append_settling_call(envelope, claimant).await
    }
}

/// A run killed between recording a deduplicated intent and recording its copy
/// resumes and finishes, without asking a human anything.
///
/// This is the one dangling write intent that is not a reconciliation hazard,
/// and the reason is provable rather than assumed: this run never held the
/// call's identity, so it never held the right to execute, so it did not.
/// Parking it would ask a human to go and check whether an effect happened that
/// could not have.
#[tokio::test]
async fn a_kill_between_a_deduplicated_intent_and_its_copy_recovers() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, true);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));

    let payer = fixed_run_id(41);
    let copier = fixed_run_id(42);
    Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random())
        .start_with_id(&agent, payer, payout_input())
        .await
        .expect("the paying run drives");
    assert_eq!(ledger.len(), 1);

    // Five appends get the second run as far as its own intent: RunStarted, the
    // loop's first clock observation, the model intent, the model completion,
    // and the tool intent. The copied completion is the sixth, and never lands.
    let killed: Arc<dyn EventStore> = Arc::new(KillStore {
        inner: Arc::clone(&store),
        remaining: AtomicI64::new(5),
    });
    let crash = Runtime::with_hooks(killed, fixed_clock(), fixed_random())
        .start_with_id(&agent, copier, payout_input())
        .await
        .expect_err("the second run dies before recording its copy");
    assert!(
        matches!(crash, RuntimeError::Store(_)),
        "the simulated crash must surface as a store failure, got {crash:?}"
    );

    let half_written = store.read_log(copier).await.expect("log reads");
    assert!(
        matches!(
            derive_state(&half_written).status,
            RunStatus::NeedsReconciliation
        ),
        "the log alone cannot tell this apart from a dangling payment"
    );

    // The store can tell them apart, so the resume finishes rather than parks.
    let resumed = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random())
        .recover(&agent, copier)
        .await
        .expect("the interrupted copy finishes without human help");
    assert!(matches!(resumed, RunOutcome::Completed { .. }));
    assert_eq!(ledger.len(), 1, "recovery must not pay a second time");

    let payer_log = store.read_log(payer).await.expect("log reads");
    let copier_log = store.read_log(copier).await.expect("log reads");
    assert_eq!(
        recorded_completion(&copier_log).1,
        Some(DedupOrigin {
            run_id: payer,
            seq: salvor_core::SequenceNumber::new(intent_seq(&payer_log)),
        }),
        "the recovered completion must still name what it copied"
    );
    assert_eq!(
        recorded_completion(&payer_log).0,
        recorded_completion(&copier_log).0
    );
}

/// A run killed mid-payment still parks for a human, and a second run for the
/// same claim refuses rather than paying again.
///
/// This is the case the whole design bends around. The first run held the
/// identity and was executing when it died, so nobody, in the store or outside
/// it, can say whether the money moved. The first run parks, exactly as it
/// always has. The second run is refused, and refused *before recording
/// anything*, so once a human has reconciled the first run the second can
/// simply be run again.
#[tokio::test]
async fn a_kill_mid_payment_parks_and_blocks_the_next_run() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, true);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));

    let payer = fixed_run_id(51);
    let follower = fixed_run_id(52);

    // Five appends get it to the intent; the payment then happens for real and
    // the settling completion is the append that dies.
    let killed: Arc<dyn EventStore> = Arc::new(KillStore {
        inner: Arc::clone(&store),
        remaining: AtomicI64::new(5),
    });
    Runtime::with_hooks(killed, fixed_clock(), fixed_random())
        .start_with_id(&agent, payer, payout_input())
        .await
        .expect_err("the paying run dies after the money moved");
    assert_eq!(ledger.len(), 1, "the payment really happened");

    let payer_log = store.read_log(payer).await.expect("log reads");
    assert!(
        matches!(
            derive_state(&payer_log).status,
            RunStatus::NeedsReconciliation
        ),
        "a write left dangling by a crash must still park for a human"
    );

    // Resuming it changes nothing: this is the human's problem, as before.
    let resumed = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random())
        .recover(&agent, payer)
        .await
        .expect_err("a dangling payment cannot resume itself");
    assert!(
        matches!(
            resumed,
            RuntimeError::Replay(salvor_core::ReplayError::NeedsReconciliation { .. })
        ),
        "the dangling write must still demand reconciliation, got {resumed:?}"
    );

    // And a second run for the same claim refuses rather than guessing.
    let blocked = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random())
        .start_with_id(&agent, follower, payout_input())
        .await
        .expect_err("a second run must not proceed past an unresolved payment");
    match blocked {
        RuntimeError::CallInFlight { holder, tool, .. } => {
            assert_eq!(holder, payer, "the refusal must name the run to go and fix");
            assert_eq!(tool, "pay_claim");
        }
        other => panic!("expected CallInFlight, got {other:?}"),
    }
    assert_eq!(ledger.len(), 1, "the refused run must have paid nobody");
    assert!(
        store
            .read_log(follower)
            .await
            .expect("log reads")
            .iter()
            .all(|e| !matches!(e.event, Event::ToolCallRequested { .. })),
        "the refused run must record no intent for a call it never made"
    );
}

/// Reconciling a dangling payment by hand releases its key, so the next run for
/// that claim can proceed.
///
/// Without this, a human doing exactly the right thing would leave the claim
/// wedged: the commitment would still name a run that is finished, every later
/// run under that key would be refused, and nothing anywhere would say why. The
/// human's completion is the moment the payment stops being in flight, so it is
/// the completion that settles the commitment.
#[tokio::test]
async fn resolving_a_dangling_payment_releases_its_key() {
    let server = payout_model().await;
    let ledger = Arc::new(Ledger::default());
    let agent = payout_agent(&server.uri(), &ledger, true);
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));

    let payer = fixed_run_id(61);
    let follower = fixed_run_id(62);

    let killed: Arc<dyn EventStore> = Arc::new(KillStore {
        inner: Arc::clone(&store),
        remaining: AtomicI64::new(5),
    });
    Runtime::with_hooks(killed, fixed_clock(), fixed_random())
        .start_with_id(&agent, payer, payout_input())
        .await
        .expect_err("the paying run dies after the money moved");
    assert_eq!(ledger.len(), 1);

    // The human checks the processor, finds the charge, and records it.
    let runtime = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random());
    let observed = json!({"claim_id": CLAIM, "amount_cents": 483_200, "charge_id": "po_by_hand"});
    runtime
        .resolve(payer, observed.clone())
        .await
        .expect("a human records what the write did");
    runtime
        .recover(&agent, payer)
        .await
        .expect("the reconciled run finishes");

    let commitment = store
        .lookup_call("pay_claim", "pay_claim:wreck-9931")
        .await
        .expect("lookup")
        .expect("claimed");
    assert!(
        commitment.completion_seq.is_some(),
        "the hand-recorded completion must settle the commitment it belongs to"
    );

    // The next run for the same claim now deduplicates against what the human
    // recorded, rather than being refused forever or paying again.
    runtime
        .start_with_id(&agent, follower, payout_input())
        .await
        .expect("a later run proceeds once the payment is reconciled");
    assert_eq!(ledger.len(), 1, "and it must not pay a second time");
    let follower_log = store.read_log(follower).await.expect("log reads");
    let (output, origin) = recorded_completion(&follower_log);
    assert_eq!(
        output, observed,
        "it must report the payment the human recorded"
    );
    assert_eq!(
        origin.map(|o| o.run_id),
        Some(payer),
        "and say whose payment it is reporting"
    );
}

/// A run that crashed inside its own keyed call comes back and finishes it,
/// rather than being told it lost the identity to itself.
///
/// The claim is re-entrant on purpose, and this is the case that needs it: an
/// idempotent call left dangling by a crash re-executes on resume (that is what
/// idempotent means), and re-executing means claiming again. A store that
/// answered "held" to the holder would wedge the run permanently.
#[tokio::test]
async fn a_run_reclaims_its_own_unfinished_call() {
    let server = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_sync", "sync_claim", payout_input(), 100, 10),
        ),
        (3, text_response("synced", 120, 12)),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(SyncClaim {
            calls: Arc::clone(&calls),
        }))
        .build()
        .expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run = fixed_run_id(71);

    // The idempotent path also records the derived attempt key's
    // `RandomObserved`, so six appends get this run to its own intent and the
    // completion is the seventh.
    let killed: Arc<dyn EventStore> = Arc::new(KillStore {
        inner: Arc::clone(&store),
        remaining: AtomicI64::new(6),
    });
    Runtime::with_hooks(killed, fixed_clock(), fixed_random())
        .start_with_id(&agent, run, payout_input())
        .await
        .expect_err("the run dies before recording its completion");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the call ran once before the crash"
    );
    let mid_crash = store
        .lookup_call("sync_claim", "sync_claim:wreck-9931")
        .await
        .expect("lookup")
        .expect("claimed");
    assert_eq!(mid_crash.run_id, run, "the crashed run still holds the key");
    assert_eq!(mid_crash.completion_seq, None, "and has not finished it");

    let resumed = Runtime::with_hooks(Arc::clone(&store), fixed_clock(), fixed_random())
        .recover(&agent, run)
        .await
        .expect("the run finishes the call it started");
    assert!(matches!(resumed, RunOutcome::Completed { .. }));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "an idempotent call left dangling re-executes, which is what it is for"
    );

    let settled = store
        .lookup_call("sync_claim", "sync_claim:wreck-9931")
        .await
        .expect("lookup")
        .expect("claimed");
    assert_eq!(settled.run_id, run);
    assert!(
        settled.completion_seq.is_some(),
        "and finishing it settles the commitment the same run held"
    );
}

/// An `Idempotent` tool that declares a key: safe to re-execute, and it says
/// which effect it is.
struct SyncClaim {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DynTool for SyncClaim {
    fn name(&self) -> &str {
        "sync_claim"
    }
    fn description(&self) -> &str {
        "brings the claim record up to date"
    }
    fn effect(&self) -> Effect {
        Effect::Idempotent
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn idempotency_key(&self, input: &Value) -> Option<String> {
        Some(format!("sync_claim:{}", input.get("claim_id")?.as_str()?))
    }
    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Output(json!({"synced": true})))
    }
}
