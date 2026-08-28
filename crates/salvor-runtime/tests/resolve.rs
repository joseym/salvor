//! Reconciliation resolution: [`Runtime::resolve`] records, by hand, the
//! completion of a dangling `Write` intent after a human has verified what the
//! write did.
//!
//! The verb is the one sanctioned way past `NeedsReconciliation`, so it is
//! held to the same bar as `resume`: it validates the state before writing
//! anything, records **exactly one** `ToolCallCompleted` correlated to the
//! intent, executes nothing, and refuses every state that is not a dangling
//! write. After it runs, an ordinary `recover` continues the run.
//!
//! # How the dangling write is produced
//!
//! [`KillStore`] allows a budgeted number of appends and then fails, so the
//! log ends exactly at the write intent (its completion is the append that
//! fails). The write's handler ran once before that failed append, which is
//! the real hazard `resolve` exists for: the side effect may have happened,
//! but its completion was never recorded.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_use_response,
};
use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber, SettledBy};
use salvor_runtime::{Agent, RunOutcome, Runtime, RuntimeError};
use salvor_store::{EventStore, RunSummary, SqliteStore, StoreError};
use serde_json::json;
use time::macros::datetime;
use wiremock::MockServer;

/// A store that allows `allow` appends and then fails every later one without
/// touching the wrapped store, simulating a crash at an exact boundary.
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

/// A two-turn script: the model asks for one write, then answers.
async fn scripted_server() -> MockServer {
    ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_write", "publish", json!({"doc": "otters"}), 100, 10),
        ),
        (3, text_response("published", 120, 12)),
    ])
    .await
}

/// The reference agent: one write tool, echoing its input.
fn reference_agent(server_uri: &str) -> (Agent, Arc<AtomicUsize>) {
    let (write_tool, write_calls) = TestTool::new("publish", Effect::Write, ToolBehavior::Echo);
    let agent = agent_builder(server_uri)
        .tool_dyn(Box::new(write_tool))
        .build()
        .expect("agent builds");
    (agent, write_calls)
}

/// Drives the reference agent under a kill store that fails the write's
/// completion append, leaving a dangling write intent. Returns the store, the
/// agent, the run id, and the write execution counter.
async fn dangling_write() -> (Arc<dyn EventStore>, Agent, RunId, Arc<AtomicUsize>) {
    let server = scripted_server().await;
    let (agent, write_calls) = reference_agent(&server.uri());
    // Leak the mock server so it stays up for the later recover call.
    Box::leak(Box::new(server));
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(40);

    // Allow through the write intent (5 appends: RunStarted, NowObserved,
    // model request, model completion, write intent); the write's completion
    // is the 6th append and fails.
    let killed = Runtime::with_hooks(
        Arc::new(KillStore::new(store.clone(), 5)),
        fixed_clock(),
        fixed_random(),
    );
    killed
        .start_with_id(&agent, run_id, json!("publish otters"))
        .await
        .expect_err("the kill store aborts the drive");
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 5, "the log ends at the write intent");
    assert!(
        matches!(
            log[4].event,
            Event::ToolCallRequested {
                effect: Effect::Write,
                ..
            }
        ),
        "the last event is the dangling write intent",
    );
    assert_eq!(write_calls.load(Ordering::SeqCst), 1, "the write ran once");
    (store, agent, run_id, write_calls)
}

#[tokio::test]
async fn resolve_records_one_completion_then_recovers() {
    let (store, agent, run_id, write_calls) = dangling_write().await;

    // Recovery refuses the dangling write: only a human may decide.
    let recovering = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    recovering
        .recover(&agent, run_id)
        .await
        .expect_err("a dangling write refuses automatic recovery");
    assert_eq!(
        write_calls.load(Ordering::SeqCst),
        1,
        "recovery ran nothing"
    );

    // Resolve records the completion the crashed process never wrote.
    let resolver = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    resolver
        .resolve(run_id, json!({"echo": {"doc": "otters"}}))
        .await
        .expect("resolve records the completion");

    // Exactly one event was appended, and it is the correlated completion.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 6, "resolve appends exactly one event");
    match &log[5].event {
        Event::ToolCallCompleted {
            seq,
            output,
            settled_by,
            ..
        } => {
            assert_eq!(*seq, SequenceNumber::new(4), "correlates to the intent seq");
            assert_eq!(*output, json!({"echo": {"doc": "otters"}}));
            // A human's report of an effect this process never witnessed, and
            // the completion says so rather than passing for an ordinary one.
            assert_eq!(*settled_by, Some(SettledBy::Operator));
        }
        other => panic!("expected a tool completion, got {other:?}"),
    }
    assert_eq!(
        write_calls.load(Ordering::SeqCst),
        1,
        "resolve executed nothing"
    );

    // Resolving again refuses: the run is no longer awaiting reconciliation.
    let again = resolver.resolve(run_id, json!({"echo": 2})).await;
    assert!(
        matches!(again, Err(RuntimeError::NotReconcilable { .. })),
        "second resolve refuses, got {again:?}"
    );

    // A plain recover now continues the run to completion, replaying the
    // hand-recorded completion and never re-running the write.
    let outcome = recovering
        .recover(&agent, run_id)
        .await
        .expect("recovery completes after resolution");
    assert!(matches!(
        outcome,
        RunOutcome::Completed { ref output, .. } if *output == json!("published")
    ));
    assert_eq!(
        write_calls.load(Ordering::SeqCst),
        1,
        "the write is never re-executed"
    );
}

/// Appends a handcrafted log to a fresh in-memory store, for the wrong-state
/// refusals (no model or tool needed to reach these states).
async fn store_with_log(events: Vec<Event>) -> (Arc<dyn EventStore>, RunId) {
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(41);
    for (i, event) in events.into_iter().enumerate() {
        let envelope = EventEnvelope::new(
            run_id,
            SequenceNumber::new(i as u64),
            datetime!(2026-07-09 12:00:00 UTC),
            event,
        );
        store.append(&envelope).await.expect("append seeds the log");
    }
    (store, run_id)
}

fn started() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:agent".into(),
        input: json!("go"),
        labels: None,
        driven_by: None,
    }
}

#[tokio::test]
async fn resolve_refuses_every_non_reconciliation_state() {
    // A completed run has nothing to resolve.
    let (store, run_id) =
        store_with_log(vec![started(), Event::RunCompleted { output: json!(1) }]).await;
    let runtime = Runtime::with_hooks(store, fixed_clock(), fixed_random());
    assert!(matches!(
        runtime.resolve(run_id, json!(1)).await,
        Err(RuntimeError::NotReconcilable { .. })
    ));

    // A parked (suspended) run is not a dangling write.
    let (store, run_id) = store_with_log(vec![
        started(),
        Event::Suspended {
            reason: "awaiting approval".into(),
            input_schema: json!({"type": "object"}),
            kind: None,
        },
    ])
    .await;
    let runtime = Runtime::with_hooks(store, fixed_clock(), fixed_random());
    assert!(matches!(
        runtime.resolve(run_id, json!(1)).await,
        Err(RuntimeError::NotReconcilable { .. })
    ));

    // A dangling READ derives to awaiting-tool, which recovers on its own and
    // is not a write to reconcile.
    let (store, run_id) = store_with_log(vec![
        started(),
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "search".into(),
            input: json!({"q": "otters"}),
            effect: Effect::Read,
            idempotency_key: None,
            performed_by: None,
        },
    ])
    .await;
    let runtime = Runtime::with_hooks(store, fixed_clock(), fixed_random());
    assert!(matches!(
        runtime.resolve(run_id, json!(1)).await,
        Err(RuntimeError::NotReconcilable { .. })
    ));

    // An unknown run is reported as such, not as a reconciliation refusal.
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store, fixed_clock(), fixed_random());
    assert!(matches!(
        runtime.resolve(fixed_run_id(99), json!(1)).await,
        Err(RuntimeError::UnknownRun { .. })
    ));
}
