//! Acceptance coverage for gate and branch nodes.
//!
//! - the flagship `research -> review -> gate -> tool` fixture parks at the
//!   gate, resumes through the same runtime machinery `salvor resume` uses, and
//!   completes (a loop a purely linear graph cannot express);
//! - a graph with a gate AND an expression branch drives live (park, resume,
//!   complete), then re-drives over the recorded log with zero live model calls,
//!   an unchanged tool counter, and a byte-identical log;
//! - the projection over that run shows the taken branch case, the skipped node,
//!   and the gate's park/resume coherently;
//! - an expression branch that matches no case refuses before recording its
//!   entry; a model-decision branch drives its agent and maps the reply to a
//!   case, refusing when the reply names none.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    ConstTool, EchoTool, ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response,
};
use salvor_core::Effect;
use salvor_engine::{EngineError, GraphOutcome, run_graph};
use salvor_graph::{BranchCondition, BranchSpec, Graph, GraphBuilder, ToolSpec};
use salvor_replay::{NodeState, derive_graph_projection};
use salvor_runtime::{Agent, ParkReason, RunCtx};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::json;

const RESEARCH_HASH: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const REVIEW_HASH: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const DECIDE_HASH: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// Loads a committed graph fixture by file name.
fn fixture(name: &str) -> Graph {
    let path = format!(
        "{}/../../examples/graphs/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path} reads: {e}"));
    serde_json::from_str(&text).expect("fixture parses")
}

/// The flagship `research -> review -> approve(gate) -> publish` fixture: a first
/// drive parks at the gate, a resume through the runtime's own machinery appends
/// the approval, and a second drive completes. This is the kill-at-the-gate loop
/// a purely linear graph cannot express.
#[tokio::test]
async fn flagship_gate_fixture_parks_resumes_and_completes() {
    let research_server =
        ScriptedModel::mount(vec![(1, text_response("a draft about otters", 5, 3))]).await;
    let review_server =
        ScriptedModel::mount(vec![(1, text_response("reviewed: publish it", 4, 2))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        RESEARCH_HASH.to_owned(),
        agent_builder(&research_server.uri()).build().unwrap(),
    );
    agents.insert(
        REVIEW_HASH.to_owned(),
        agent_builder(&review_server.uri()).build().unwrap(),
    );

    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("http_post".to_owned(), Box::new(publish));

    let graph = fixture("research-review-publish.json");
    let input = json!({"topic": "otters"});
    let run_id = fixed_run_id(10);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // --- Drive 1: runs the two agents, then parks at the gate. ---
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph drives to a park");
    match &outcome {
        GraphOutcome::Parked {
            node,
            reason: ParkReason::Suspended { input_schema, .. },
        } => {
            assert_eq!(node, "approve");
            // The recorded suspension schema is the gate's approval schema.
            assert_eq!(
                input_schema["properties"]["approved"]["type"],
                json!("boolean")
            );
        }
        other => panic!("expected a park at the gate, got {other:?}"),
    }

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
            "NodeEntered", // approve (gate)
            "Suspended",
        ],
        "the log ends exactly at the gate's suspension"
    );
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        0,
        "publish waits behind the gate"
    );

    // --- The operator resume: the SAME runtime machinery `salvor resume` uses.
    // `Runtime::resume` validates the input, calls `RunCtx::set_resume_input`,
    // and re-drives; a graph run has no resume verb yet, so this reuses that
    // exact primitive (set the input, drive again). The `Resumed` event is
    // appended by the cursor's live `await_resume`, not hand-crafted here. ---
    let approval = json!({"approved": true, "draft": "reviewed: publish it"});
    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log1.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("resume ctx builds");
    ctx2.set_resume_input(approval.clone());
    let resumed = run_graph(&mut ctx2, &graph, &input, &agents, &tools)
        .await
        .expect("graph completes after resume");
    let GraphOutcome::Completed { output } = resumed else {
        panic!("expected completion after resume, got {resumed:?}");
    };
    // The resume value passed through the gate and into the publish tool.
    assert_eq!(output, json!({"published": approval}));
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        1,
        "publish ran once, after approval"
    );

    let log2 = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        &event_kinds(&log2)[13..],
        [
            "Resumed",
            "NodeExited",  // approve
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ],
        "the resume records the approval and drives the gate's successor to completion"
    );
}

/// A graph with a gate AND an expression branch: it drives live (parking at the
/// gate, resuming, completing), and a re-drive over the whole recorded log is a
/// free, byte-identical replay. The projection then shows the taken case, the
/// skipped node, and the gate's park/resume.
#[tokio::test]
async fn branch_and_gate_run_replay_and_project() {
    let research_server =
        ScriptedModel::mount(vec![(1, text_response("a draft about otters", 5, 3))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        RESEARCH_HASH.to_owned(),
        agent_builder(&research_server.uri()).build().unwrap(),
    );

    // `assess` injects a structured routed value the branch reads; `publish`
    // echoes the approval. `reject` is never resolved: it is on the low route.
    let (assess, assess_calls) = ConstTool::new("assess", Effect::Read, json!({"score": 0.9}));
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("assess".to_owned(), Box::new(assess));
    tools.insert("http_post".to_owned(), Box::new(publish));

    let graph = fixture("branch-review.json");
    let input = json!({"topic": "otters"});
    let run_id = fixed_run_id(11);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // --- Drive 1: research, assess, route(high), then park at the gate. ---
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let parked = run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph parks at the gate");
    assert!(
        matches!(&parked, GraphOutcome::Parked { node, .. } if node == "approve"),
        "expected a park at the gate, got {parked:?}"
    );

    // --- Resume the gate, completing the run. ---
    let log_parked = store.read_log(run_id).await.expect("log reads");
    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log_parked,
        fixed_clock(),
        fixed_random(),
    )
    .expect("resume ctx builds");
    ctx2.set_resume_input(json!({"approved": true}));
    let done = run_graph(&mut ctx2, &graph, &input, &agents, &tools)
        .await
        .expect("graph completes");
    assert!(matches!(done, GraphOutcome::Completed { .. }));

    let live_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&live_log),
        [
            "GraphRunStarted",
            "NodeEntered", // research
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited",  // research
            "NodeEntered", // assess
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited",  // assess
            "NodeEntered", // route (branch)
            "BranchTaken",
            "NodeExited",  // route
            "NodeEntered", // approve (gate)
            "Suspended",
            "Resumed",
            "NodeExited",  // approve
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited",  // publish
            "NodeSkipped", // reject (low route)
            "RunCompleted",
        ]
    );
    // The recorded route and skip are the branch's authority.
    assert!(live_log.iter().any(|e| matches!(
        &e.event,
        salvor_core::Event::BranchTaken { node, case } if node == "route" && case == "high"
    )));
    assert!(live_log.iter().any(|e| matches!(
        &e.event,
        salvor_core::Event::NodeSkipped { node, .. } if node == "reject"
    )));

    // --- The replay proof: a re-drive over the whole log is free. ---
    let assess_before = assess_calls.load(Ordering::SeqCst);
    let publish_before = publish_calls.load(Ordering::SeqCst);
    let model_reqs_before = research_server.received_requests().await.unwrap().len();
    assert_eq!(
        (assess_before, publish_before),
        (1, 1),
        "each tool ran once live"
    );

    let mut replay_ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        live_log.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("replay ctx builds");
    let replayed = run_graph(&mut replay_ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph replays");
    assert!(matches!(replayed, GraphOutcome::Completed { .. }));
    assert!(!replay_ctx.is_replaying(), "history fully consumed");

    // Zero live calls: no tool re-executed, the model was not re-called.
    assert_eq!(assess_calls.load(Ordering::SeqCst), assess_before);
    assert_eq!(publish_calls.load(Ordering::SeqCst), publish_before);
    assert_eq!(
        research_server.received_requests().await.unwrap().len(),
        model_reqs_before,
        "the model must not be re-called on replay"
    );

    // Byte-identical log.
    let replay_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        serde_json::to_string(&live_log).unwrap(),
        serde_json::to_string(&replay_log).unwrap(),
        "the replay produced a byte-identical log"
    );

    // --- The projection: taken case, skipped node, gate coherent. ---
    let projection = derive_graph_projection(&replay_log);
    assert_eq!(projection.current_node, None);
    assert_eq!(
        projection.node("route").unwrap().branch_case.as_deref(),
        Some("high"),
        "the branch node carries the fired case"
    );
    assert!(
        matches!(
            projection.node("reject").unwrap().state,
            NodeState::Skipped { .. }
        ),
        "the non-taken node is Skipped, not absent"
    );
    assert_eq!(
        projection.node("approve").unwrap().state,
        NodeState::Exited,
        "the gate parked and resumed, ending Exited"
    );
    assert_eq!(projection.node("publish").unwrap().state, NodeState::Exited);
}

/// An expression branch that matches no case refuses deterministically, before
/// recording the branch's `NodeEntered`, so nothing lands in the log past it.
#[tokio::test]
async fn expression_branch_with_no_matching_case_refuses() {
    let agents: HashMap<String, Agent> = HashMap::new();
    // The routed value scores 0.5, so neither `score >= 0.8` nor a second high
    // guard fires.
    let (assess, _calls) = ConstTool::new("assess", Effect::Read, json!({"score": 0.5}));
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("assess".to_owned(), Box::new(assess));

    let graph = GraphBuilder::new()
        .tool(ToolSpec::new("assess", "assess"))
        .branch(
            BranchSpec::new("route")
                .case("high", BranchCondition::Expression("score >= 0.8".into()))
                .case(
                    "higher",
                    BranchCondition::Expression("score >= 0.95".into()),
                ),
        )
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("assess", "route")
        .labeled_edge("route", "publish", "high")
        .build();

    let run_id = fixed_run_id(12);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({}), &agents, &tools)
        .await
        .expect_err("no case matches, so the branch refuses");
    assert!(matches!(error, EngineError::NoBranchCaseMatched { node } if node == "route"));

    // The assess tool ran; the branch never recorded an entry, and no terminal.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered", // assess
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // assess
        ],
        "the log ends at the refusal, before the branch's NodeEntered"
    );
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            salvor_core::Event::NodeEntered { node } if node == "route"
        )),
        "the branch must not have been entered"
    );
}

/// A model-decision branch drives its agent with the routed value and maps the
/// reply to a case name: the named case is recorded `BranchTaken`, its edge is
/// followed, and the other route is skipped.
#[tokio::test]
async fn model_decision_branch_routes_by_the_agents_reply() {
    // The decision agent replies `high` (one message in, one text out).
    let decide_server = ScriptedModel::mount(vec![(1, text_response("high", 3, 1))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        DECIDE_HASH.to_owned(),
        agent_builder(&decide_server.uri()).build().unwrap(),
    );

    let (win, win_calls) = EchoTool::new("win", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("win".to_owned(), Box::new(win));

    let graph = GraphBuilder::new()
        .branch(
            BranchSpec::new("decide")
                .agent_hash(DECIDE_HASH)
                .case("high", BranchCondition::ModelDecision)
                .case("low", BranchCondition::ModelDecision),
        )
        .tool(ToolSpec::new("win", "win"))
        .tool(ToolSpec::new("lose", "lose"))
        .labeled_edge("decide", "win", "high")
        .labeled_edge("decide", "lose", "low")
        .build();

    let run_id = fixed_run_id(13);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let outcome = run_graph(
        &mut ctx,
        &graph,
        &json!({"topic": "otters"}),
        &agents,
        &tools,
    )
    .await
    .expect("graph drives");
    assert!(matches!(outcome, GraphOutcome::Completed { .. }));
    assert_eq!(win_calls.load(Ordering::SeqCst), 1, "the high route ran");

    let log = store.read_log(run_id).await.expect("log reads");
    assert!(log.iter().any(|e| matches!(
        &e.event,
        salvor_core::Event::BranchTaken { node, case } if node == "decide" && case == "high"
    )));
    assert!(
        log.iter().any(|e| matches!(
            &e.event,
            salvor_core::Event::NodeSkipped { node, .. } if node == "lose"
        )),
        "the low route is skipped"
    );
}

/// A model-decision branch whose agent names no case refuses with a typed error
/// that lists the cases; it arrives after the branch's entry and the model's own
/// events, since the model had to run to produce the reply.
#[tokio::test]
async fn model_decision_branch_with_unknown_reply_refuses() {
    let decide_server = ScriptedModel::mount(vec![(1, text_response("maybe", 3, 1))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        DECIDE_HASH.to_owned(),
        agent_builder(&decide_server.uri()).build().unwrap(),
    );
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let graph = GraphBuilder::new()
        .branch(
            BranchSpec::new("decide")
                .agent_hash(DECIDE_HASH)
                .case("high", BranchCondition::ModelDecision)
                .case("low", BranchCondition::ModelDecision),
        )
        .tool(ToolSpec::new("win", "win"))
        .labeled_edge("decide", "win", "high")
        .build();

    let run_id = fixed_run_id(14);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &json!({}), &agents, &tools)
        .await
        .expect_err("the reply names no case");
    match error {
        EngineError::BranchDecisionUnmatched { node, reply, cases } => {
            assert_eq!(node, "decide");
            assert_eq!(reply, "maybe");
            assert_eq!(cases, vec!["high".to_owned(), "low".to_owned()]);
        }
        other => panic!("expected BranchDecisionUnmatched, got {other:?}"),
    }

    // The branch WAS entered and the model ran; there is just no BranchTaken.
    let log = store.read_log(run_id).await.expect("log reads");
    assert!(log.iter().any(|e| matches!(
        &e.event,
        salvor_core::Event::NodeEntered { node } if node == "decide"
    )));
    assert!(
        !log.iter()
            .any(|e| matches!(&e.event, salvor_core::Event::BranchTaken { .. })),
        "no route was recorded"
    );
}
