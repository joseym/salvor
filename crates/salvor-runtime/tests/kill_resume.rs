//! Kill/resume: interrupt a run at chosen event boundaries, recover it with
//! a fresh runtime over the same store, and prove the recovered log equals
//! the uninterrupted control run's log exactly, with zero duplicate `Write`
//! executions.
//!
//! # How the kill works
//!
//! [`KillStore`] wraps the real store and starts failing appends after a
//! budgeted count. The event that fails is never persisted (the wrapped
//! store is not called), so the log ends at a chosen boundary and the drive
//! aborts with a store error, exactly as if the process had died with that
//! much history durable. Recovery then runs on the *unwrapped* store.
//!
//! # The reference run
//!
//! model r1 -> read tool -> model r2 -> write tool -> model r3 -> complete.
//! Its control log (15 events):
//!
//! ```text
//!  0 RunStarted           8 ModelCallCompleted (r2)
//!  1 NowObserved          9 ToolCallRequested  (write)
//!  2 ModelCallRequested  10 ToolCallCompleted
//!  3 ModelCallCompleted  11 NowObserved
//!  4 ToolCallRequested   12 ModelCallRequested
//!  5 ToolCallCompleted   13 ModelCallCompleted (r3)
//!  6 NowObserved         14 RunCompleted
//!  7 ToolCallRequested   ...
//! ```
//!
//! (seq 7 is the second model intent; the table reads column-wise.)
//!
//! Cut points, as append budgets:
//!
//! - **3**: the log ends at the model r1 intent (seq 2). The dangling model
//!   call is re-issued on recovery, by design; tools run exactly once.
//! - **5**: the log ends at the read intent (seq 4). The read executed once
//!   before the kill and re-executes on recovery, which is the specified
//!   `Read` semantics (re-execute if unrecorded), so its counter reads 2;
//!   the write still runs exactly once and the final log is identical.
//! - **11**: the log ends at the write *completion* (seq 10). Recovery
//!   replays the completed write and never re-executes it.
//! - **10** (the refusal case): the log ends at the write *intent* (seq 9).
//!   Recovery must refuse with `NeedsReconciliation` and execute nothing.
//!
//! The injected clock is constant and the run id fixed, so control and
//! recovered envelopes (run id, seq, schema version, timestamp, event) are
//! comparable with plain equality.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_use_response,
};
use salvor_core::{Effect, EventEnvelope, ReplayError, RunId};
use salvor_runtime::{Agent, RunOutcome, Runtime, RuntimeError};
use salvor_store::{EventStore, RunSummary, SqliteStore, StoreError};
use serde_json::json;
use wiremock::MockServer;

/// A store wrapper that allows a budgeted number of appends and then fails
/// every later one *without* touching the wrapped store, simulating a
/// process death at an exact event boundary.
struct KillStore {
    inner: Arc<dyn EventStore>,
    remaining: AtomicI64,
}

impl KillStore {
    fn new(inner: Arc<dyn EventStore>, allow: i64) -> Self {
        Self {
            inner,
            remaining: AtomicI64::new(allow),
        }
    }
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

    /// Counted against the same budget as `append`, because it is one: a kill
    /// budget that let settling appends through would cut the run somewhere
    /// other than where the test asked for.
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

/// The scripted three-turn conversation shared by every scenario.
async fn scripted_server() -> MockServer {
    ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_read", "lookup", json!({"q": "otters"}), 100, 10),
        ),
        (
            3,
            tool_use_response("tu_write", "publish", json!({"doc": "otters"}), 120, 12),
        ),
        (5, text_response("published", 140, 14)),
    ])
    .await
}

/// Builds the reference agent over a server, returning it with the two
/// execution counters.
fn reference_agent(server_uri: &str) -> (Agent, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (read_tool, read_calls) = TestTool::new("lookup", Effect::Read, ToolBehavior::Echo);
    let (write_tool, write_calls) = TestTool::new("publish", Effect::Write, ToolBehavior::Echo);
    let agent = agent_builder(server_uri)
        .tool_dyn(Box::new(read_tool))
        .tool_dyn(Box::new(write_tool))
        .build()
        .expect("agent builds");
    (agent, read_calls, write_calls)
}

/// Runs the reference agent to completion with no interruption and returns
/// its full log: the oracle every recovered log must equal.
async fn control_log(run_tag: u8) -> Vec<EventEnvelope> {
    let server = scripted_server().await;
    let (agent, read_calls, write_calls) = reference_agent(&server.uri());
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(run_tag);

    let outcome = runtime
        .start_with_id(&agent, run_id, json!("publish otters"))
        .await
        .expect("control run completes");
    assert!(matches!(
        outcome,
        RunOutcome::Completed { ref output, .. } if *output == json!("published")
    ));
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 15, "the reference run records 15 events");
    log
}

/// Drives one kill-at-`allow`-appends scenario and recovers it, returning
/// the final log and the two counters (accumulated across both phases).
async fn kill_then_recover(
    run_tag: u8,
    allow: i64,
) -> (Vec<EventEnvelope>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let server = scripted_server().await;
    let (agent, read_calls, write_calls) = reference_agent(&server.uri());
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(run_tag);

    // Phase 1: drive under the kill store until the simulated crash.
    let killed = Runtime::with_hooks(
        Arc::new(KillStore::new(store.clone(), allow)),
        fixed_clock(),
        fixed_random(),
    );
    let error = killed
        .start_with_id(&agent, run_id, json!("publish otters"))
        .await
        .expect_err("the kill store aborts the drive");
    assert!(matches!(error, RuntimeError::Store(_)), "{error}");
    let interrupted = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        interrupted.len(),
        usize::try_from(allow).unwrap(),
        "exactly the budgeted number of events persisted"
    );

    // Phase 2: a fresh runtime over the same store, no kill wrapper.
    let recovering = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let outcome = recovering
        .recover(&agent, run_id)
        .await
        .expect("recovery completes the run");
    assert!(matches!(
        outcome,
        RunOutcome::Completed { ref output, .. } if *output == json!("published")
    ));

    let log = store.read_log(run_id).await.expect("log reads");
    (log, read_calls, write_calls)
}

#[tokio::test]
async fn kill_after_model_intent_recovers_identically() {
    let control = control_log(10).await;
    let (log, read_calls, write_calls) = kill_then_recover(10, 3).await;
    assert_eq!(log, control, "the recovered log equals the control log");
    // The dangling model call was re-issued (that is safe by design); no
    // tool ran before the kill, so both counters read exactly one.
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn kill_after_read_intent_recovers_identically() {
    let control = control_log(11).await;
    let (log, read_calls, write_calls) = kill_then_recover(11, 5).await;
    assert_eq!(log, control, "the recovered log equals the control log");
    // The read executed before the kill (its completion is what failed to
    // persist) and re-executed on recovery: the specified Read semantics.
    // The write, the call that must never duplicate, ran exactly once.
    assert_eq!(read_calls.load(Ordering::SeqCst), 2);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn kill_after_write_completion_recovers_identically() {
    let control = control_log(12).await;
    let (log, read_calls, write_calls) = kill_then_recover(12, 11).await;
    assert_eq!(log, control, "the recovered log equals the control log");
    // Everything up to and including the write completed before the kill;
    // recovery replays it all. Zero duplicate executions of any tool.
    assert_eq!(read_calls.load(Ordering::SeqCst), 1);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn kill_after_write_intent_refuses_recovery() {
    let server = scripted_server().await;
    let (agent, _read_calls, write_calls) = reference_agent(&server.uri());
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(13);

    // Allow exactly through the write intent (seq 9): 10 appends.
    let killed = Runtime::with_hooks(
        Arc::new(KillStore::new(store.clone(), 10)),
        fixed_clock(),
        fixed_random(),
    );
    killed
        .start_with_id(&agent, run_id, json!("publish otters"))
        .await
        .expect_err("the kill store aborts the drive");
    assert_eq!(write_calls.load(Ordering::SeqCst), 1, "the write ran once");

    // Recovery must refuse: the write may or may not have landed, and only
    // a human may decide. The error carries the recorded intent as evidence.
    let recovering = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let error = recovering
        .recover(&agent, run_id)
        .await
        .expect_err("a dangling write intent refuses recovery");
    match error {
        RuntimeError::Replay(ReplayError::NeedsReconciliation { tool, input, .. }) => {
            assert_eq!(tool, "publish");
            assert_eq!(input, json!({"doc": "otters"}));
        }
        other => panic!("expected NeedsReconciliation, got {other:?}"),
    }
    assert_eq!(
        write_calls.load(Ordering::SeqCst),
        1,
        "recovery executed nothing"
    );
}
