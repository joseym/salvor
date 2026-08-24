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

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{Effect, Event, RunStatus, SuspensionKind, derive_state};
use salvor_runtime::{ParkReason, Resumption, RunCtx, RunOutcome, Runtime, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::Suspension;
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
        matches!(
            derive_state(&parked).status,
            RunStatus::Suspended {
                ref reason,
                kind: Some(SuspensionKind::Signal),
                ..
            } if reason == "awaiting the carrier webhook"
        ),
        "a signal wait is suspended, and the status says what it waits on"
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

/// The same wait, declared by a tool instead of by hand. A tool returns
/// `ToolOutcome::Suspend` carrying a suspension built with `on_signal`, and
/// the discriminator survives every hop between there and the log: through the
/// erased `DynTool` boundary, into the completion sentinel, out of the
/// runtime's own decode of that sentinel, onto the `Suspended` event, and back
/// out through the park reason the caller is handed. A tool author who cannot
/// say "a webhook answers this" has no way to keep the run out of an approval
/// inbox, which is the whole point of the discriminator.
#[tokio::test]
async fn a_tool_can_declare_that_a_signal_answers_its_suspension() {
    let schema = payload_schema();
    let server = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "await_carrier", json!({}), 50, 5),
        ),
        (3, text_response("shipment tracked", 60, 6)),
    ])
    .await;

    let (tool, _calls) = TestTool::new(
        "await_carrier",
        Effect::Read,
        ToolBehavior::Suspend(
            Suspension::new("awaiting the carrier webhook", schema.clone()).on_signal(),
        ),
    );
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(81);

    let outcome = runtime
        .start_with_id(&agent, run_id, json!("ship it"))
        .await
        .expect("the drive itself succeeds");
    let RunOutcome::Parked {
        reason:
            ParkReason::Suspended {
                reason,
                input_schema,
                kind,
            },
        ..
    } = &outcome
    else {
        panic!("expected a suspension park, got {outcome:?}");
    };
    assert_eq!(reason, "awaiting the carrier webhook");
    assert_eq!(input_schema, &schema);
    assert_eq!(
        *kind,
        Some(SuspensionKind::Signal),
        "the park reason carries what the tool said it waits on"
    );

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "Suspended",
        ]
    );
    assert_eq!(
        log[6].event,
        Event::Suspended {
            reason: "awaiting the carrier webhook".to_owned(),
            input_schema: schema.clone(),
            kind: Some(SuspensionKind::Signal),
        },
        "the recorded event, not just the returned reason, names the signal"
    );
    assert!(
        matches!(
            derive_state(&log).status,
            RunStatus::Suspended {
                kind: Some(SuspensionKind::Signal),
                ..
            }
        ),
        "and the fold every surface reads hands it on"
    );

    // A second drive over the recorded log replays the tool completion, decodes
    // the sentinel back into a suspension, and asks the cursor for the same
    // discriminator. A `kind` dropped anywhere on that path would be a
    // divergence here rather than a silent downgrade to a human gate.
    let replayed = runtime
        .resume(&agent, run_id, json!({"tracking_number": "1Z999"}))
        .await;
    assert!(
        replayed.is_ok(),
        "the recorded signal suspension replays clean: {replayed:?}"
    );
}
