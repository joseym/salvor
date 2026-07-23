//! Acceptance for the `fold` node's DEFERRED execution: the engine refuses
//! a fold node with a typed `UnsupportedNode` error, recorded BEFORE the node's
//! `NodeEntered`, so a lone fold leaves only the graph head in the log.
//! The document layer validates a fold as a legal graph, and its markers and
//! projection exist for client-driven runs to record against, but the server
//! engine does not drive the loop.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{event_kinds, fixed_clock, fixed_random, fixed_run_id};
use salvor_engine::{EngineError, run_graph};
use salvor_graph::{FoldBody, FoldJoin, FoldSpec, Graph, GraphBuilder, ToolSpec};
use salvor_runtime::RunCtx;
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::json;

/// A lone fold node (a subgraph body, so no sibling body node is walked): the
/// entry of a one-node graph. Mirrors the map-subgraph refusal fixture.
fn lone_fold_graph() -> Graph {
    let body = GraphBuilder::new()
        .tool(ToolSpec::new("inner", "inner_tool"))
        .build();
    GraphBuilder::new()
        .fold(FoldSpec::new(
            "refine",
            FoldBody::Subgraph(Box::new(body)),
            3,
            "score >= 0.85",
            FoldJoin::BestBy("score".into()),
        ))
        .build()
}

/// A fold node is refused with a typed `UnsupportedNode` naming the node and its
/// `fold` kind, BEFORE its `NodeEntered` is recorded, so only the graph head
/// lands in the log. No agents or tools are registered: the refusal is a pure
/// property of the node kind, not of a missing executable.
#[tokio::test]
async fn a_fold_node_is_refused_before_it_records_anything() {
    let agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    let graph = lone_fold_graph();

    let run_id = fixed_run_id(40);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({"score": 0.9}), &agents, &tools)
        .await
        .expect_err("a fold node must be refused while its execution is deferred");
    match error {
        EngineError::UnsupportedNode { node, kind } => {
            assert_eq!(node, "refine");
            assert_eq!(kind, "fold");
        }
        other => panic!("expected UnsupportedNode, got {other:?}"),
    }

    // Only the graph head was recorded: nothing for the fold, and no terminal.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        ["GraphRunStarted"],
        "the refusal leaves only the head; no NodeEntered for the fold, no terminal"
    );
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            salvor_core::Event::NodeEntered { node } if node == "refine"
        )),
        "the fold must not have been entered"
    );
}
