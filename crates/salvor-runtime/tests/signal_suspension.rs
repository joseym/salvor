//! Waiting on an external signal at the runtime edge: a webhook, not a
//! person, answers the suspension.
//!
//! A signal wait genuinely is "parked awaiting schema-validated input", so it
//! reuses `Suspended` and `Resumed` rather than inventing events of its own,
//! and it parks, validates, and resumes through the same code every gate uses.
//! The one thing it must not do is look like a gate to a surface that lists
//! work for people, which is what the recorded discriminator is for.

mod common;

use std::sync::Arc;

use common::{event_kinds, fixed_clock, fixed_random, fixed_run_id};
use salvor_core::{Event, RunStatus, SuspensionKind, derive_state};
use salvor_runtime::{Resumption, RunCtx, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};

const AGENT_HASH: &str = "sha256:shipment-flow-v1";

/// The shape the carrier's callback must post back.
fn payload_schema() -> Value {
    json!({"type": "object", "required": ["tracking_number"]})
}

/// How one drive of the shipment flow ended.
enum FlowOutcome {
    Parked,
    Completed(Value),
}

/// The user-written orchestration: begin, park until the carrier calls back,
/// complete with what it sent.
async fn shipment_flow(ctx: &mut RunCtx) -> Result<FlowOutcome, RuntimeError> {
    ctx.begin(AGENT_HASH, &json!({"order": "A-1"})).await?;
    ctx.suspend_for_signal("awaiting the carrier webhook", &payload_schema())
        .await?;
    match ctx.await_resume().await? {
        Resumption::Parked => Ok(FlowOutcome::Parked),
        Resumption::Resumed(payload) => {
            let output = json!({"shipped": payload});
            ctx.complete_run(&output).await?;
            Ok(FlowOutcome::Completed(output))
        }
    }
}

/// A signal wait parks and resumes exactly as a gate does, records the
/// discriminator that tells it apart, and derives to plain `Suspended`: the
/// run really is awaiting input, and the fold has nothing else to say about
/// it. What differs is who owes the answer, which is a routing fact a surface
/// reads off the event, not a state the run is in.
#[tokio::test]
async fn a_signal_wait_parks_records_its_kind_and_resumes() {
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(80);

    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over an empty log");
    let outcome = shipment_flow(&mut ctx)
        .await
        .expect("the first drive parks");
    assert!(matches!(outcome, FlowOutcome::Parked));
    drop(ctx);

    let parked = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&parked), ["RunStarted", "Suspended"]);
    assert_eq!(
        parked[1].event,
        Event::Suspended {
            reason: "awaiting the carrier webhook".to_owned(),
            input_schema: payload_schema(),
            kind: Some(SuspensionKind::Signal),
        },
        "the wait records what it is waiting on"
    );
    assert!(
        matches!(derive_state(&parked).status, RunStatus::Suspended { ref reason, .. }
            if reason == "awaiting the carrier webhook"),
        "a signal wait is suspended, with no status of its own"
    );

    // The callback arrives: the same resume path a gate uses.
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, parked, fixed_clock(), fixed_random())
        .expect("ctx builds over the recorded log");
    ctx.set_resume_input(json!({"tracking_number": "1Z999"}));
    let outcome = shipment_flow(&mut ctx)
        .await
        .expect("the resumed drive completes");
    let FlowOutcome::Completed(output) = outcome else {
        panic!("expected completion after the callback");
    };
    assert_eq!(output, json!({"shipped": {"tracking_number": "1Z999"}}));
    drop(ctx);

    let finished = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&finished),
        ["RunStarted", "Suspended", "Resumed", "RunCompleted"],
        "a signal resumes through the same events a gate does"
    );

    // A drive over the finished log replays it whole, discriminator included,
    // and appends nothing.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        finished.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over the finished log");
    shipment_flow(&mut ctx)
        .await
        .expect("the finished run replays");
    drop(ctx);
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        finished,
        "a replayed drive appends nothing"
    );
}

/// The other half: a gate recorded through the unchanged `suspend` carries no
/// discriminator at all, so its log is the one every earlier build wrote.
#[tokio::test]
async fn a_gate_records_no_discriminator() {
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(81);

    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over an empty log");
    ctx.begin(AGENT_HASH, &json!({"order": "A-1"}))
        .await
        .expect("the run begins");
    ctx.suspend("a human must approve the shipment", &payload_schema())
        .await
        .expect("the gate is recorded");
    drop(ctx);

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        log[1].event,
        Event::Suspended {
            reason: "a human must approve the shipment".to_owned(),
            input_schema: payload_schema(),
            kind: None,
        }
    );
    let wire = serde_json::to_string(&log[1]).expect("serialize");
    assert!(
        !wire.contains(r#""kind":"signal""#),
        "a gate must not emit the discriminator: {wire}"
    );
}
