//! The library-first proof: a user-written orchestration function, built
//! against nothing but this crate's public API, gets the same durability
//! guarantees as the built-in loop.
//!
//! `approval_flow` below is deliberately not the built-in loop: it makes
//! one model call, sends the draft to an approval tool, parks on the
//! tool's suspension, and completes with the approval once resumed. The
//! test drives it, drops everything, constructs a fresh `RunCtx` over the
//! stored log, and re-runs the *same function* to resume: recorded steps
//! replay (the model server is hit exactly once across both drives), the
//! resume input lands, and the run completes.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    ScriptedModel, TestTool, ToolBehavior, fixed_clock, fixed_random, fixed_run_id, text_response,
};
use salvor_core::{Effect, RunStatus, derive_state};
use salvor_llm::{Client, Config, Message, MessageRequest};
use salvor_runtime::{Resumption, RunCtx, RuntimeError, ToolCallResult, content_string};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::{DynTool, Suspension};
use serde_json::{Value, json};

/// How one drive of the custom flow ended.
enum FlowOutcome {
    Parked,
    Completed(Value),
}

/// The user-written orchestration: begin, one model call, one approval
/// tool call, park on its suspension, complete with the approval.
///
/// Everything it touches is public `salvor-runtime` API (plus the public
/// `salvor-llm` and `salvor-tools` types it composes). Run it once to park;
/// run it again over the recorded log to resume.
async fn approval_flow(
    ctx: &mut RunCtx,
    client: &Client,
    approve: &dyn DynTool,
) -> Result<FlowOutcome, RuntimeError> {
    let input = ctx
        .begin("sha256:custom-approval-flow-v1", &json!("draft a plan"))
        .await?;

    let request =
        MessageRequest::new("test-model", 256).push_message(Message::user(content_string(&input)));
    let turn = ctx.model_call(client, &request).await?;
    let draft = turn.response.text();

    match ctx
        .tool_call(approve, &json!({"draft": draft}), None)
        .await?
    {
        ToolCallResult::Suspended(Suspension {
            reason,
            input_schema,
        }) => {
            ctx.suspend(&reason, &input_schema).await?;
            match ctx.await_resume().await? {
                Resumption::Parked => Ok(FlowOutcome::Parked),
                Resumption::Resumed(approval) => {
                    let output = json!({"draft": draft, "approval": approval});
                    ctx.complete_run(&output).await?;
                    Ok(FlowOutcome::Completed(output))
                }
            }
        }
        ToolCallResult::Output(output) => {
            ctx.complete_run(&output).await?;
            Ok(FlowOutcome::Completed(output))
        }
        ToolCallResult::Failed(failure) => {
            ctx.fail_run(&failure.message).await?;
            Err(RuntimeError::ResumeInputRejected(failure.message))
        }
    }
}

#[tokio::test]
async fn a_user_written_loop_runs_parks_and_resumes_on_the_public_api() {
    let server =
        ScriptedModel::mount(vec![(1, text_response("the plan: study otters", 25, 8))]).await;
    let client = Client::new(
        Config::new()
            .with_base_url(server.uri())
            .with_max_retries(0),
    )
    .expect("client builds");

    let (approve, calls) = TestTool::new(
        "approve",
        Effect::Read,
        ToolBehavior::Suspend(Suspension {
            reason: "plan needs sign-off".to_owned(),
            input_schema: json!({"type": "object", "required": ["approved"]}),
        }),
    );

    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(50);

    // Drive 1: fresh run, parks at the suspension.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over an empty log");
    let outcome = approval_flow(&mut ctx, &client, &approve)
        .await
        .expect("the first drive succeeds");
    assert!(matches!(outcome, FlowOutcome::Parked));
    drop(ctx);

    let log = store.read_log(run_id).await.expect("log reads");
    assert!(matches!(
        derive_state(&log).status,
        RunStatus::Suspended { ref reason, .. } if reason == "plan needs sign-off"
    ));

    // Drive 2: a fresh context over the recorded log, resume input set,
    // the SAME user function re-run. Recorded steps replay; the resume
    // lands live.
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, fixed_clock(), fixed_random())
        .expect("ctx builds over the recorded log");
    ctx.set_resume_input(json!({"approved": true, "by": "josey"}));
    let outcome = approval_flow(&mut ctx, &client, &approve)
        .await
        .expect("the resumed drive succeeds");
    let FlowOutcome::Completed(output) = outcome else {
        panic!("expected completion after resume");
    };
    assert_eq!(
        output,
        json!({
            "draft": "the plan: study otters",
            "approval": {"approved": true, "by": "josey"},
        })
    );

    // Durability held: the model was called once across both drives (the
    // second drive replayed it), the tool executed once, and the final log
    // derives to completed.
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        1,
        "the replayed model call never hit the server"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool executed once");
    let final_log = store.read_log(run_id).await.expect("log reads");
    assert!(matches!(
        derive_state(&final_log).status,
        RunStatus::Completed { .. }
    ));
}
