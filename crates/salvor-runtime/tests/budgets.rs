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
use salvor_runtime::{
    AgentBuildError, Budgets, ParkReason, RunOutcome, Runtime, RuntimeError, budget_extensions,
    budget_observations,
};
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

/// The pure check, run over the recorded log, reproduces what the runtime
/// enforced: same dimension, same effective limit, same observed value.
///
/// This is the proof behind the browser-side budget check. `Budgets` and
/// `first_crossing` live in `salvor-replay` precisely so a page can evaluate
/// the rule the runtime enforces rather than approximate it, and
/// `budget_observations`/`budget_extensions` are what let a caller holding
/// nothing but the log arrive at the same inputs. If that reconstruction were
/// off by one model call, or absorbed an extension the loop did not, this test
/// is where it would show, because the recorded `BudgetExceeded` event is the
/// runtime's own answer written down.
///
/// The prefix matters and is the point: the loop checks BEFORE each model
/// call, so the observations behind a crossing at position `n` are the fold of
/// `log[..n]`, and folding one event further would count the crossing's own
/// aftermath.
#[tokio::test]
async fn the_pure_check_reproduces_what_the_runtime_recorded() {
    let server = ScriptedModel::mount(vec![
        (1, tool_use_response("tu_1", "echo", json!({"n": 1}), 30, 3)),
        (3, text_response("done under extension", 40, 4)),
    ])
    .await;
    let (tool, _calls) = TestTool::new("echo", Effect::Read, ToolBehavior::Echo);
    let declared = Budgets {
        max_steps: Some(1),
        ..Budgets::default()
    };
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .budgets(declared.clone())
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(32);

    runtime
        .start_with_id(&agent, run_id, json!("count things"))
        .await
        .expect("the drive itself succeeds");
    runtime
        .resume(&agent, run_id, json!({"extend": {"steps": 2}}))
        .await
        .expect("resume with an extension completes the run");
    let log = store.read_log(run_id).await.expect("log reads");

    // The crossing the runtime recorded, and where it recorded it.
    let (at, recorded_budget, recorded_observed) = log
        .iter()
        .enumerate()
        .find_map(|(i, envelope)| match &envelope.event {
            Event::BudgetExceeded { budget, observed } => Some((i, *budget, *observed)),
            _ => None,
        })
        .expect("the run crossed its step budget");

    // The same check, rebuilt from the log alone.
    let observations = budget_observations(&log[..at]);
    let extensions = budget_extensions(&log[..at]);
    let (budget, observed) = declared
        .first_crossing(&extensions, None, &observations)
        .expect("the reconstructed check fires where the runtime's did");
    assert_eq!(budget, recorded_budget, "same dimension and same limit");
    assert_eq!(observed, recorded_observed, "same observed value");
    assert_eq!(
        observations.steps, 1,
        "one completed model call had been recorded when the check fired"
    );

    // And past the recorded extension, the same check clears: the resume that
    // answered the crossing raised the effective limit, and the raise is read
    // out of the log rather than passed in.
    let after = budget_extensions(&log);
    assert_eq!(after.steps, 2, "the recorded resume granted two more steps");
    assert!(
        declared
            .first_crossing(&after, None, &budget_observations(&log))
            .is_none(),
        "two completed calls against a limit of 1 + 2 is not a crossing"
    );
}
