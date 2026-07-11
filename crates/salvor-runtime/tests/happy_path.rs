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

/// Recording the request body is opt-in and never touches the hash. The same
/// run driven with recording on and with recording off records the identical
/// `request_hash`; only the presence of `request_body` differs. When present
/// the body is the request that was sent, which is the same value the hash is
/// computed over. Covers the opt-in behavior and the hash invariant.
#[tokio::test]
async fn recording_prompts_stores_body_without_changing_the_hash() {
    // Drives a one-model-call run and returns the recorded `ModelCallRequested`
    // event. The inputs (agent, input, clock, random) are identical across
    // calls, so the request, and therefore its hash, is identical too.
    async fn drive_once(record_prompts: bool, tag: u8) -> Event {
        let server = ScriptedModel::mount(vec![(1, text_response("done", 10, 5))]).await;
        let agent = agent_builder(&server.uri()).build().expect("agent builds");
        let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
        let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random())
            .with_record_prompts(record_prompts);
        let run_id = fixed_run_id(tag);
        runtime
            .start_with_id(&agent, run_id, json!("hello"))
            .await
            .expect("run completes");
        let log = store.read_log(run_id).await.expect("log reads");
        log.into_iter()
            .find_map(|envelope| match envelope.event {
                event @ Event::ModelCallRequested { .. } => Some(event),
                _ => None,
            })
            .expect("a model intent was recorded")
    }

    let recorded_on = drive_once(true, 40).await;
    let recorded_off = drive_once(false, 41).await;

    let Event::ModelCallRequested {
        request_hash: hash_on,
        request_body: body_on,
        ..
    } = &recorded_on
    else {
        panic!("expected a model intent from the recording-on run");
    };
    let Event::ModelCallRequested {
        request_hash: hash_off,
        request_body: body_off,
        ..
    } = &recorded_off
    else {
        panic!("expected a model intent from the recording-off run");
    };

    // The hash is identical whether or not the body is recorded.
    assert_eq!(
        hash_on, hash_off,
        "recording the body must not change the request hash"
    );

    // Recording off stores no body; recording on stores the request itself.
    assert!(body_off.is_none(), "recording off must not store a body");
    let body = body_on.as_ref().expect("recording on stores the body");
    assert_eq!(
        body["model"],
        json!("test-model"),
        "the recorded body is the real request that was hashed"
    );
    assert!(
        body["messages"].is_array(),
        "the recorded body carries the request messages"
    );
}
