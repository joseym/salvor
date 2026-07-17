//! The linear-graph contract: a linear graph runs to completion against a mock model,
//! a second drive over the recorded log replays with zero live calls and a
//! byte-identical log, the unsupported-node refusal is typed and writes nothing
//! past the refusal, and the graph projection shows the nodes in order.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    EchoTool, ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random, fixed_run_id,
    text_response,
};
use salvor_core::Effect;
use salvor_engine::{EngineError, GraphOutcome, graph_hash, run_graph};
use salvor_graph::{AgentSpec, GateSpec, Graph, GraphBuilder};
use salvor_replay::{NodeState, derive_graph_projection};
use salvor_runtime::RunCtx;
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};

const RESEARCH_HASH: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const REVIEW_HASH: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// Loads the committed linear fixture, the same document `salvor graph validate`
/// accepts.
fn linear_fixture() -> Graph {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/graphs/linear-research-publish.json"
    );
    let text = std::fs::read_to_string(path).expect("fixture reads");
    serde_json::from_str(&text).expect("fixture parses")
}

/// The whole linear story on one fixture: drive to completion, then prove the
/// replay is free and byte-identical, then project the produced log.
#[tokio::test]
async fn linear_graph_runs_replays_and_projects() {
    // Each agent points at its own mock server, so the two single-turn calls
    // are scripted independently (both carry one message).
    let research_server =
        ScriptedModel::mount(vec![(1, text_response("a draft about otters", 5, 3))]).await;
    let review_server =
        ScriptedModel::mount(vec![(1, text_response("reviewed: publish it", 4, 2))]).await;

    let research_agent = agent_builder(&research_server.uri()).build().unwrap();
    let review_agent = agent_builder(&review_server.uri()).build().unwrap();
    let mut agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    agents.insert(RESEARCH_HASH.to_owned(), research_agent);
    agents.insert(REVIEW_HASH.to_owned(), review_agent);

    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("http_post".to_owned(), Box::new(publish));

    let graph = linear_fixture();
    let input = json!({"topic": "otters"});
    let run_id = fixed_run_id(1);

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // --- Drive 1: live, records the walk into the log. ---
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph drives");
    let GraphOutcome::Completed { output } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    assert_eq!(output, json!({"published": "reviewed: publish it"}));

    let log1 = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log1),
        [
            "GraphRunStarted",
            "NodeEntered", // research
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited",  // research
            "NodeEntered", // review
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited",  // review
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ]
    );

    // The head records the reproducible graph hash.
    let expected_hash = graph_hash(&graph).unwrap();
    match &log1[0].event {
        salvor_core::Event::GraphRunStarted { graph_hash, .. } => {
            assert_eq!(graph_hash, &expected_hash);
        }
        other => panic!("head is not GraphRunStarted: {other:?}"),
    }

    // --- Acceptance 4: the projection shows the nodes in order, all exited. ---
    let projection = derive_graph_projection(&log1);
    assert_eq!(
        projection.graph_hash.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(projection.current_node, None);
    let ids: Vec<&str> = projection.nodes.iter().map(|n| n.node.as_str()).collect();
    assert_eq!(ids, ["research", "review", "publish"]);
    for node in &projection.nodes {
        assert_eq!(
            node.state,
            NodeState::Exited,
            "node {} not exited",
            node.node
        );
    }

    // --- Acceptance 2: a second drive over the recorded log is free. ---
    let calls_before = publish_calls.load(Ordering::SeqCst);
    let research_reqs_before = research_server.received_requests().await.unwrap().len();
    let review_reqs_before = review_server.received_requests().await.unwrap().len();
    assert_eq!(calls_before, 1, "the write tool executed once live");

    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log1.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("replay ctx builds");
    let replayed = run_graph(&mut ctx2, &graph, &input, &agents, &tools)
        .await
        .expect("graph replays");
    let GraphOutcome::Completed {
        output: replayed_output,
    } = replayed
    else {
        panic!("expected completion on replay");
    };
    assert_eq!(replayed_output, output, "replay reproduces the output");

    // Zero live calls: neither model server saw a new request, and the write
    // tool did not execute again.
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        calls_before,
        "the write tool must not re-execute on replay"
    );
    assert_eq!(
        research_server.received_requests().await.unwrap().len(),
        research_reqs_before,
        "the research model must not be re-called on replay"
    );
    assert_eq!(
        review_server.received_requests().await.unwrap().len(),
        review_reqs_before,
        "the review model must not be re-called on replay"
    );

    // The replay consumed the whole recorded history, appending nothing.
    assert!(!ctx2.is_replaying(), "history fully consumed");

    // Byte-identical log: serialize both and compare bytes, not counts.
    let log2 = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        serde_json::to_string(&log1).unwrap(),
        serde_json::to_string(&log2).unwrap(),
        "the replay produced a byte-identical log"
    );
}

/// Acceptance 3: a graph with a gate node hits the typed unsupported-node
/// error, and no events are written past the refusal point.
#[tokio::test]
async fn a_gate_node_is_refused_before_it_records_anything() {
    // research (agent) -> approve (gate). The agent runs; the gate is refused.
    let research_server = ScriptedModel::mount(vec![(1, text_response("a draft", 3, 2))]).await;
    let research_agent = agent_builder(&research_server.uri()).build().unwrap();
    let mut agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    agents.insert(RESEARCH_HASH.to_owned(), research_agent);
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .gate(GateSpec::new("approve", json!({"type": "object"})))
        .edge("research", "approve")
        .build();

    let run_id = fixed_run_id(2);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({}), &agents, &tools)
        .await
        .expect_err("a gate node must be refused");
    match error {
        EngineError::UnsupportedNode { node, kind } => {
            assert_eq!(node, "approve");
            assert_eq!(kind, "gate");
        }
        other => panic!("expected UnsupportedNode, got {other:?}"),
    }

    // The research node ran (its own events are present), but nothing for the
    // gate and no terminal was written: the log ends exactly at the refusal.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered", // research
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited", // research
        ],
        "no NodeEntered for the gate, and no terminal"
    );
    // Doubly explicit: the gate node never appears in the log.
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            salvor_core::Event::NodeEntered { node } if node == "approve"
        )),
        "the gate must not have been entered"
    );
}

/// A single gate node (no agent ahead of it) is refused immediately, with only
/// the graph head in the log.
#[tokio::test]
async fn a_lone_gate_refuses_with_only_the_head_recorded() {
    let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    let graph = GraphBuilder::new()
        .gate(GateSpec::new("approve", json!({"type": "object"})))
        .build();

    let run_id = fixed_run_id(3);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({}), &agents, &tools)
        .await
        .expect_err("a lone gate must be refused");
    assert!(matches!(error, EngineError::UnsupportedNode { .. }));

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&log), ["GraphRunStarted"]);
}

/// An agent node whose hash the resolver cannot supply is a typed error.
#[tokio::test]
async fn an_unresolved_agent_is_a_typed_error() {
    let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .build();

    let run_id = fixed_run_id(4);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools)
        .await
        .expect_err("an unresolved agent must error");
    assert!(matches!(error, EngineError::UnknownAgent { node, .. } if node == "research"));
}
