//! A graph `tool` node whose tool asks to park the run on a durable timer.
//!
//! The engine handles it exactly as it handles a tool suspension: the node
//! stays entered with no `NodeExited`, the walk stops, and a later drive
//! re-enters the node and continues from the recorded sleep. What differs is
//! what ends the park. A suspension waits for an input a person supplies; this
//! waits for an instant, and takes nothing.
//!
//! The claim ordering is the same one the runtime tier pins: the call's
//! completion is recorded before `SleepStarted`, so a node asleep for a week
//! holds no idempotency claim.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{EchoTool, NappingTool, TestClock, event_kinds, fixed_random, fixed_run_id};
use salvor_core::{Effect, Event, RunStatus, derive_state};
use salvor_engine::{GraphOutcome, run_graph};
use salvor_graph::{Graph, GraphBuilder, ToolSpec};
use salvor_replay::{NodeState, ParkReason, derive_graph_projection};
use salvor_runtime::RunCtx;
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;

/// The instant the run starts at, and the instant its tool parks it until.
const START: OffsetDateTime = datetime!(2026-08-14 08:00:00 UTC);
const WAKE_AT: OffsetDateTime = datetime!(2026-08-14 09:00:00 UTC);

/// Two tool nodes: the first parks the run on a timer, the second runs after
/// it wakes. The second node is what proves the walk actually continues rather
/// than merely stopping politely.
fn napping_graph() -> Graph {
    GraphBuilder::new()
        .tool(ToolSpec::new("hold", "hold_tool"))
        .tool(ToolSpec::new("publish", "publish_tool"))
        .edge("hold", "publish")
        .build()
}

/// A graph `tool` node parks its run on the timer its tool asked for, and a
/// later drive past the deadline re-enters the node, records the wake, and
/// walks on to the end.
#[tokio::test]
async fn a_tool_node_parks_on_a_timer_and_the_walk_continues() {
    let graph = napping_graph();
    let run_id = fixed_run_id(70);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let clock = TestClock::new(START);
    let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();

    let (hold, hold_calls) = NappingTool::new("hold_tool", Effect::Read, WAKE_AT);
    let (publish, publish_calls) = EchoTool::new("publish_tool", Effect::Read);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("hold_tool".to_owned(), Box::new(hold));
    tools.insert("publish_tool".to_owned(), Box::new(publish));

    // --- Drive 1: reaches the sleep and parks. ---
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        vec![],
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &json!({"order": "A-1"}), &agents, &tools)
        .await
        .expect("the graph drives");
    match outcome {
        GraphOutcome::Parked {
            node,
            reason: ParkReason::Sleeping { wake_at },
        } => {
            assert_eq!(node, "hold", "the node the run is parked at");
            assert_eq!(wake_at, WAKE_AT, "the instant its tool asked for");
        }
        other => panic!("expected a timer park, got {other:?}"),
    }

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered", // hold
            "ToolCallRequested",
            "ToolCallCompleted",
            "SleepStarted",
        ],
        "no NodeExited: the node is still the one the run is in"
    );
    // THE ORDERING, read off the log: the call settles, then the sleep starts.
    assert!(
        matches!(log[3].event, Event::ToolCallCompleted { .. })
            && matches!(log[4].event, Event::SleepStarted { .. })
    );
    assert_eq!(
        derive_state(&log).status,
        RunStatus::Sleeping { wake_at: WAKE_AT }
    );
    let projection = derive_graph_projection(&log);
    assert_eq!(
        projection.current_node.as_deref(),
        Some("hold"),
        "the projection shows the run sitting inside the node"
    );
    assert_eq!(
        projection
            .nodes
            .iter()
            .find(|node| node.node == "hold")
            .map(|node| node.state.clone()),
        Some(NodeState::Entered),
        "entered, never exited"
    );

    // --- Drive 2: still early. Records nothing, however often it is asked. ---
    clock.set(WAKE_AT - time::Duration::minutes(1));
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log.clone(),
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let early = run_graph(&mut ctx, &graph, &json!({"order": "A-1"}), &agents, &tools)
        .await
        .expect("an early drive is not an error");
    assert!(
        matches!(
            early,
            GraphOutcome::Parked {
                reason: ParkReason::Sleeping { .. },
                ..
            }
        ),
        "still asleep: {early:?}"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads").len(),
        5,
        "and nothing was appended"
    );

    // --- Drive 3: at the deadline. The wake is recorded and the walk ends. ---
    clock.set(WAKE_AT);
    let log = store.read_log(run_id).await.expect("log reads");
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, clock.injected(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &json!({"order": "A-1"}), &agents, &tools)
        .await
        .expect("the woken graph drives");
    let GraphOutcome::Completed { output } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    // The parked node's own output is derived from the wake instant its
    // completion recorded, and it is what the next node was handed.
    assert_eq!(
        output,
        json!({"published": {"slept_until": "2026-08-14T09:00:00Z"}})
    );

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered", // hold
            "ToolCallRequested",
            "ToolCallCompleted",
            "SleepStarted",
            "SleepCompleted",
            "NodeExited",  // hold
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ]
    );
    assert_eq!(
        hold_calls.load(Ordering::SeqCst),
        1,
        "the napping tool ran once, on the drive that recorded its completion"
    );
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);

    // --- Drive 4: a full replay over the finished log executes nothing and
    // appends nothing, wake included. ---
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log.clone(),
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let replayed = run_graph(&mut ctx, &graph, &json!({"order": "A-1"}), &agents, &tools)
        .await
        .expect("the replay is divergence free");
    assert!(matches!(replayed, GraphOutcome::Completed { .. }));
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        log,
        "a replay appends nothing"
    );
    assert_eq!(hold_calls.load(Ordering::SeqCst), 1);
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);
}
