//! The happy path: a multi-step run (model, tool, model, complete) against
//! a scripted server, asserting the final output, the exact event shape,
//! and usage accumulation.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{Effect, Event, RunStatus, derive_state};
use salvor_runtime::{RunOutcome, Runtime};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;

#[tokio::test]
async fn multi_step_run_completes_with_recorded_shape_and_usage() {
    // Turn 1 (one message): call the echo tool. Turn 2 (three messages):
    // final text.
    let server = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "echo", json!({"q": "otters"}), 100, 20),
        ),
        (3, text_response("all done", 150, 30)),
    ])
    .await;

    let (tool, calls) = TestTool::new("echo", Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(1);

    let outcome = runtime
        .start_with_id(&agent, run_id, json!("research otters"))
        .await
        .expect("run completes");

    match outcome {
        RunOutcome::Completed { run_id: id, output } => {
            assert_eq!(id, run_id);
            assert_eq!(output, json!("all done"));
        }
        other => panic!("expected completion, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool executed once");

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
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted",
        ]
    );

    // Usage accumulates exactly the scripted numbers.
    let state = derive_state(&log);
    assert_eq!(state.usage.input_tokens, 250);
    assert_eq!(state.usage.output_tokens, 50);
    assert_eq!(
        state.status,
        RunStatus::Completed {
            output: json!("all done")
        }
    );

    // The recorded tool intent carries the input the model asked for, with
    // the Read effect and no idempotency key; the completion echoes it.
    let Event::ToolCallRequested {
        tool,
        input,
        effect,
        idempotency_key,
        ..
    } = &log[4].event
    else {
        panic!("expected a tool intent at seq 4");
    };
    assert_eq!(tool, "echo");
    assert_eq!(input, &json!({"q": "otters"}));
    assert_eq!(*effect, Effect::Read);
    assert_eq!(idempotency_key, &None);
    let Event::ToolCallCompleted { output, .. } = &log[5].event else {
        panic!("expected a tool completion at seq 5");
    };
    assert_eq!(output, &json!({"echo": {"q": "otters"}}));
}
