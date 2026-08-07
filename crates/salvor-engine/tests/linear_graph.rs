//! The linear-graph contract: a linear graph runs to completion against a mock model,
//! a second drive over the recorded log replays with zero live calls and a
//! byte-identical log, and the graph projection shows the nodes in order. The two
//! map-refusal tests here were repurposed when the engine gained map fan-out:
//! map nodes are no longer refused wholesale, so they now cover the typed refusals
//! that remain: a `map` whose `over` reference resolves to a non-list, and a
//! `map` whose body is an (unsupported) embedded subgraph. The full fan-out
//! behaviour lives in `map_graph.rs`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    EchoTool, ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_use_response,
};
use salvor_core::{Effect, EventEnvelope};
use salvor_engine::{EngineError, GraphOutcome, graph_hash, run_graph};
use salvor_graph::{AgentSpec, Graph, GraphBuilder, MapBody, MapSpec, ToolSpec};
use salvor_replay::{NodeState, derive_graph_projection};
use salvor_runtime::{ANSWER_TOOL, RunCtx};
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
    // are scripted independently (both carry one message). `research` declares
    // an `output_schema` in the fixture and so answers through the runtime's
    // forced `salvor_answer` call; `review` declares none and answers in prose,
    // which is what the two shapes below are.
    let research_server = ScriptedModel::mount(vec![(
        1,
        tool_use_response(
            "tu_research",
            ANSWER_TOOL,
            json!({"draft": "a draft about otters"}),
            5,
            3,
        ),
    )])
    .await;
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

/// Repurposed (was `a_map_node_is_refused_before_it_records_anything`): a `map`
/// whose `over` reference does not resolve to a list is a typed
/// `MapOverNotAList`, refused BEFORE the map's `NodeEntered`, so nothing lands in
/// the log past the refusal. The upstream agent still runs, proving the refusal is
/// at the map, not the graph head. The worker body node is never walked (it is
/// map-owned) and never reached (the map refused).
#[tokio::test]
async fn a_map_over_a_non_list_is_refused_before_it_records_anything() {
    // research (agent) -> fanout (map). research produces a string, so `over`
    // ("items") resolves to a missing path (not a list) and the map refuses.
    let research_server = ScriptedModel::mount(vec![(1, text_response("a draft", 3, 2))]).await;
    let research_agent = agent_builder(&research_server.uri()).build().unwrap();
    let mut agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    agents.insert(RESEARCH_HASH.to_owned(), research_agent);
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .map(MapSpec::new(
            "fanout",
            "items",
            2,
            MapBody::Node("worker".into()),
        ))
        .tool(ToolSpec::new("worker", "worker_tool"))
        .edge("research", "fanout")
        .build();

    let run_id = fixed_run_id(2);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({}), &agents, &tools)
        .await
        .expect_err("a map over a non-list must be refused");
    match error {
        EngineError::MapOverNotAList { node, over } => {
            assert_eq!(node, "fanout");
            assert_eq!(over, "items");
        }
        other => panic!("expected MapOverNotAList, got {other:?}"),
    }

    // The research node ran (its own events are present), but nothing for the
    // map and no terminal was written: the log ends exactly at the refusal.
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
        "no NodeEntered for the map, and no terminal"
    );
    // Doubly explicit: neither the map nor its worker body ever appears entered.
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            salvor_core::Event::NodeEntered { node } if node == "fanout" || node == "worker"
        )),
        "the map (and its map-owned worker) must not have been entered"
    );
}

/// A `map`
/// whose body is an embedded subgraph is a typed `UnsupportedMapBody`, refused
/// BEFORE the map's `NodeEntered`, so a lone such map leaves only the graph head
/// in the log. (The node-body variant ships; the subgraph body does not.)
#[tokio::test]
async fn a_map_with_a_subgraph_body_refuses_with_only_the_head_recorded() {
    let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    // A minimal (unused) embedded subgraph body; the engine refuses before it
    // would ever be walked.
    let subgraph = GraphBuilder::new()
        .tool(ToolSpec::new("inner", "inner_tool"))
        .build();
    let graph = GraphBuilder::new()
        .map(MapSpec::new(
            "fanout",
            "items",
            1,
            MapBody::Subgraph(Box::new(subgraph)),
        ))
        .build();

    let run_id = fixed_run_id(3);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({"items": [1, 2]}), &agents, &tools)
        .await
        .expect_err("a subgraph body must be refused");
    match error {
        EngineError::UnsupportedMapBody { node, .. } => assert_eq!(node, "fanout"),
        other => panic!("expected UnsupportedMapBody, got {other:?}"),
    }

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&log), ["GraphRunStarted"]);
}

/// An idempotent `tool` node records a key that is a pure function of the call's
/// POSITION in the graph, not of drawn randomness: two independent drives of the
/// same document (under DIFFERENT run ids and with a different random source
/// each) record the IDENTICAL `idempotency_key`, and neither records a
/// `RandomObserved` for it. This is the engine-level proof of the fork-safe key:
/// a fork re-walking this node live presents the same key its origin recorded,
/// so the provider collapses the duplicate and only `Write` needs acknowledging.
#[tokio::test]
async fn an_idempotent_tool_node_key_is_position_derived_not_random() {
    let graph = GraphBuilder::new()
        .tool(salvor_graph::ToolSpec::new("notify", "notify_tool"))
        .build();
    let input = json!({"to": "ops"});

    // Two random sources that would disagree if the key were drawn from them.
    let drive = |run_id, random: salvor_runtime::RandomFn| {
        let graph = graph.clone();
        let input = input.clone();
        async move {
            let (notify, _calls) = EchoTool::new("notify_tool", Effect::Idempotent);
            let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
            tools.insert("notify_tool".to_owned(), Box::new(notify));
            let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
            let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
            let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), random)
                .expect("ctx builds");
            run_graph(&mut ctx, &graph, &input, &agents, &tools)
                .await
                .expect("graph drives");
            store.read_log(run_id).await.expect("log reads")
        }
    };

    let log_a = drive(fixed_run_id(20), Arc::new(|| 11)).await;
    let log_b = drive(fixed_run_id(21), Arc::new(|| 999)).await;

    let key_of = |log: &[EventEnvelope]| -> Option<String> {
        log.iter().find_map(|e| match &e.event {
            salvor_core::Event::ToolCallRequested {
                idempotency_key, ..
            } => idempotency_key.clone(),
            _ => None,
        })
    };
    let key_a = key_of(&log_a).expect("an idempotent tool records a key");
    let key_b = key_of(&log_b).expect("an idempotent tool records a key");
    assert_eq!(
        key_a, key_b,
        "the idempotent key must be identical across independent runs (position-derived, not random)"
    );
    assert!(
        key_a.starts_with("sha256:"),
        "the derived key is a canonical hash: {key_a}"
    );

    // The key no longer costs a RandomObserved: nothing draws randomness here.
    assert!(
        !log_a
            .iter()
            .any(|e| matches!(&e.event, salvor_core::Event::RandomObserved { .. })),
        "a position-derived key must not record a RandomObserved"
    );
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
