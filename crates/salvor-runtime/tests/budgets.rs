//! Budget enforcement: a `max_steps` crossing parks the run with
//! `BudgetExceeded`, a resume carrying an extension continues it, a replay
//! of the full log re-fires the check identically (no divergence), and a
//! cost budget without pricing fails at build time (covered in the crate's
//! unit tests; re-asserted here at the integration surface).

mod common;

use std::sync::Arc;

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{BudgetKind, Effect, Event};
use salvor_runtime::{AgentBuildError, Budgets, ParkReason, RunOutcome, Runtime, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;

#[tokio::test]
async fn max_steps_crossing_parks_extends_and_replays_identically() {
    // Iteration 0 runs (steps = 0 < 1); iteration 1 crosses (steps = 1).
    let server = ScriptedModel::mount(vec![
        (1, tool_use_response("tu_1", "echo", json!({"n": 1}), 30, 3)),
        (3, text_response("done under extension", 40, 4)),
    ])
    .await;
    let (tool, _calls) = TestTool::new("echo", Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .budgets(Budgets {
            max_steps: Some(1),
            ..Budgets::default()
        })
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(30);

    // The crossing parks the run rather than killing it.
    let outcome = runtime
        .start_with_id(&agent, run_id, json!("count things"))
        .await
        .expect("the drive itself succeeds");
    match &outcome {
        RunOutcome::Parked {
            reason: ParkReason::BudgetExceeded { budget, observed },
            ..
        } => {
            assert_eq!(budget.kind, BudgetKind::Steps);
            assert_eq!(budget.limit, 1.0);
            assert_eq!(*observed, 1.0);
        }
        other => panic!("expected a budget park, got {other:?}"),
    }
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
            "BudgetExceeded",
        ]
    );
    assert!(matches!(
        &log[7].event,
        Event::BudgetExceeded { budget, observed }
            if budget.kind == BudgetKind::Steps && *observed == 1.0
    ));

    // A malformed extension is rejected before anything is recorded.
    let rejected = runtime
        .resume(&agent, run_id, json!({"extend": {"stepz": 2}}))
        .await
        .expect_err("an unknown extension key is rejected");
    assert!(matches!(rejected, RuntimeError::ResumeInputRejected(_)));

    // A valid extension raises the effective limit and the run completes.
    let outcome = runtime
        .resume(&agent, run_id, json!({"extend": {"steps": 2}}))
        .await
        .expect("resume with an extension completes the run");
    assert!(matches!(
        outcome,
        RunOutcome::Completed { ref output, .. } if *output == json!("done under extension")
    ));
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
            "BudgetExceeded",
            "Resumed",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted",
        ]
    );

    // Replaying the full log re-fires the check identically: the recomputed
    // crossing matches the recorded BudgetExceeded, the recorded extension
    // raises the limit again, and the run replays to completion with no
    // divergence.
    let replayed = runtime
        .recover(&agent, run_id)
        .await
        .expect("replay re-fires the budget check without divergence");
    assert!(matches!(
        replayed,
        RunOutcome::Completed { ref output, .. } if *output == json!("done under extension")
    ));
}

#[tokio::test]
async fn token_budget_crossing_uses_recorded_usage() {
    // The first call reports 30 + 3 = 33 tokens; the 20-token budget
    // crosses at the second iteration's check.
    let server = ScriptedModel::mount(vec![(
        1,
        tool_use_response("tu_1", "echo", json!({"n": 1}), 30, 3),
    )])
    .await;
    let (tool, _calls) = TestTool::new("echo", Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .budgets(Budgets {
            max_tokens: Some(20),
            ..Budgets::default()
        })
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let outcome = runtime
        .start_with_id(&agent, fixed_run_id(31), json!("count tokens"))
        .await
        .expect("the drive itself succeeds");
    match outcome {
        RunOutcome::Parked {
            reason: ParkReason::BudgetExceeded { budget, observed },
            ..
        } => {
            assert_eq!(budget.kind, BudgetKind::Tokens);
            assert_eq!(budget.limit, 20.0);
            assert_eq!(observed, 33.0, "observed is the recorded usage total");
        }
        other => panic!("expected a token budget park, got {other:?}"),
    }
}

#[test]
fn cost_budget_without_pricing_is_a_build_error() {
    let result = agent_builder("http://localhost:9")
        .budgets(Budgets {
            max_cost_usd: Some(2.0),
            ..Budgets::default()
        })
        .build();
    assert!(matches!(
        result,
        Err(AgentBuildError::CostBudgetWithoutPricing)
    ));
}
