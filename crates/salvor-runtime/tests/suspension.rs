//! Suspension as a tool return value: the run parks durably, resume input
//! is validated against the recorded schema, the model receives that input
//! as the tool result, and a replay of the completed log is divergence
//! free.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_result_contents, tool_use_response,
};
use salvor_core::{Effect, Event};
use salvor_runtime::{ParkReason, RunOutcome, Runtime, RuntimeError, SUSPEND_SENTINEL_KEY};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::Suspension;
use serde_json::json;

#[tokio::test]
async fn suspension_parks_validates_resume_input_and_replays_cleanly() {
    let schema = json!({"type": "object", "required": ["approved"]});
    let server = ScriptedModel::mount(vec![
        (1, tool_use_response("tu_1", "approve", json!({}), 50, 5)),
        (3, text_response("approved and done", 60, 6)),
    ])
    .await;

    let (tool, calls) = TestTool::new(
        "approve",
        Effect::Read,
        ToolBehavior::Suspend(Suspension::new("awaiting human approval", schema.clone())),
    );
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(20);

    // The run parks. The returned reason carries the recorded schema.
    let outcome = runtime
        .start_with_id(&agent, run_id, json!("request approval"))
        .await
        .expect("the drive itself succeeds");
    match &outcome {
        RunOutcome::Parked {
            reason:
                ParkReason::Suspended {
                    reason,
                    input_schema,
                    kind,
                },
            ..
        } => {
            assert_eq!(reason, "awaiting human approval");
            assert_eq!(input_schema, &schema);
            // A tool that named nothing parks a human gate, and the park
            // report says so by saying nothing.
            assert_eq!(*kind, None);
        }
        other => panic!("expected a suspension park, got {other:?}"),
    }

    // The parked log: the tool call completed with the suspension sentinel,
    // then Suspended, and nothing after.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        vec![
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "Suspended",
        ]
    );
    let Event::ToolCallCompleted { output, .. } = &log[5].event else {
        panic!("expected the sentinel completion at seq 5");
    };
    assert_eq!(
        output,
        &json!({
            SUSPEND_SENTINEL_KEY: {
                "reason": "awaiting human approval",
                "input_schema": schema,
            }
        }),
        "the suspension sentinel wire shape is a contract"
    );

    // Input that violates the recorded schema is rejected before anything
    // is recorded.
    let rejected = runtime
        .resume(&agent, run_id, json!({"note": "missing the field"}))
        .await
        .expect_err("wrong-shaped input is rejected");
    assert!(matches!(rejected, RuntimeError::ResumeInputRejected(_)));
    assert_eq!(
        store.read_log(run_id).await.expect("log reads").len(),
        7,
        "a rejected resume records nothing"
    );

    // Valid input resumes and completes the run.
    let outcome = runtime
        .resume(&agent, run_id, json!({"approved": true}))
        .await
        .expect("resume completes the run");
    assert!(matches!(
        outcome,
        RunOutcome::Completed { ref output, .. } if *output == json!("approved and done")
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the suspending tool is not re-executed on resume"
    );

    // The model received the resume input as the tool result content.
    let requests = server.received_requests().await.expect("requests recorded");
    let final_request = requests
        .iter()
        .rev()
        .find(|request| !tool_result_contents(&request.body).is_empty())
        .expect("a request carrying a tool result");
    assert_eq!(
        tool_result_contents(&final_request.body),
        vec![r#"{"approved":true}"#.to_owned()],
        "the recorded resume input, canonically rendered, is what the model sees"
    );

    // Replaying the completed log end to end is divergence free.
    let replayed = runtime
        .recover(&agent, run_id)
        .await
        .expect("replay of the completed log is divergence free");
    assert!(matches!(
        replayed,
        RunOutcome::Completed { ref output, .. } if *output == json!("approved and done")
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "replay executes nothing at all"
    );
}
