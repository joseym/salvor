//! Write-ahead proof: the tool intent is durable in the store *before* the
//! tool executes.
//!
//! The probe tool inspects the store from inside its own execution: if the
//! write-ahead ordering holds, the log it reads mid-call already ends with
//! its own `ToolCallRequested`. The tool then fails, which also pins down
//! that a failed call still records a completion carrying the full error.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{
    ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_use_response,
};
use salvor_core::{Effect, Event, RunId};
use salvor_runtime::{ERROR_SENTINEL_KEY, RunOutcome, Runtime};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::{DynTool, HandlerError, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};

/// A `Write`-effect tool that, when executed, checks the store for its own
/// intent event and then fails.
struct IntentProbe {
    store: Arc<SqliteStore>,
    run_id: RunId,
    /// Set to true when, at execution time, the log already ended with this
    /// tool's intent.
    saw_own_intent: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl DynTool for IntentProbe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "asserts write-ahead ordering from inside its own execution"
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        let log = self.store.read_log(self.run_id).await.expect("log reads");
        let last_is_own_intent = matches!(
            log.last().map(|envelope| &envelope.event),
            Some(Event::ToolCallRequested { tool, .. }) if tool == "probe"
        );
        self.saw_own_intent
            .store(last_is_own_intent, Ordering::SeqCst);
        Err(ToolError::Handler {
            tool: "probe".to_owned(),
            source: HandlerError::message("deliberate failure after the ordering check"),
        })
    }
}

#[tokio::test]
async fn tool_intent_is_persisted_before_execution() {
    let server = ScriptedModel::mount(vec![
        (1, tool_use_response("tu_1", "probe", json!({}), 10, 5)),
        (3, text_response("finished despite the tool failure", 20, 5)),
    ])
    .await;

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(2);
    let saw_own_intent = Arc::new(AtomicBool::new(false));

    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(IntentProbe {
            store: store.clone(),
            run_id,
            saw_own_intent: saw_own_intent.clone(),
        }))
        .build()
        .expect("agent builds");

    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let outcome = runtime
        .start_with_id(&agent, run_id, json!("go"))
        .await
        .expect("run completes");
    assert!(matches!(outcome, RunOutcome::Completed { .. }));

    assert!(
        saw_own_intent.load(Ordering::SeqCst),
        "at execution time the store must already hold the tool's intent"
    );

    // The failed call still recorded a completion, and the completion holds
    // the full structured error (a Write tool is never auto-retried, so
    // exactly one attempt).
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
    let Event::ToolCallCompleted { output, .. } = &log[5].event else {
        panic!("expected the failed call's completion at seq 5");
    };
    let error = output
        .get(ERROR_SENTINEL_KEY)
        .expect("the completion output is the structured error object");
    assert_eq!(error["is_error"], json!(true));
    assert_eq!(error["kind"], json!("handler"));
    assert_eq!(error["attempts"], json!(1));
    let message = error["message"].as_str().expect("message is a string");
    assert!(
        message.contains("deliberate failure after the ordering check"),
        "the full error text is recorded: {message}"
    );
}
