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
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::Effect;
use salvor_engine::{EngineError, GraphOutcome, run_graph};
use salvor_graph::{BranchCondition, BranchSpec, GateSpec, Graph, GraphBuilder, ToolSpec};
use salvor_replay::{NodeState, derive_graph_projection};
use salvor_runtime::{ANSWER_TOOL, Agent, ParkReason, RunCtx};
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
///
/// Both agent nodes in this fixture declare an `output_schema` of `{draft}`, so
/// both answer through the runtime's forced `salvor_answer` call rather than in
/// prose. What the gate parks on and what the resume publishes is unchanged;
/// only the shape of a scripted reply is.
#[tokio::test]
async fn flagship_gate_fixture_parks_resumes_and_completes() {
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
    let review_server = ScriptedModel::mount(vec![(
        1,
        tool_use_response(
            "tu_review",
            ANSWER_TOOL,
            json!({"draft": "reviewed: publish it"}),
            4,
            2,
        ),
    )])
    .await;
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

/// A gate's `approval_schema` is ENFORCED, not merely advertised.
///
/// The schema here is the one that used to let everything through: it names
/// `required` and `properties` but never says `type`, so plain JSON Schema
/// semantics leave `null`, `42`, and `"nope"` vacuously conforming. Each of the
/// four is refused now, naming the gate and every violation; each refusal
/// appends nothing, leaves the log byte-identical, and leaves the run parked at
/// the same gate. A conforming approval then completes the run, and re-driving
/// the whole recorded log is a free, byte-identical replay: the recorded
/// `Resumed` is trusted, never re-judged.
#[tokio::test]
async fn a_gate_refuses_every_nonconforming_approval_and_stays_parked() {
    let (assess, assess_calls) = ConstTool::new("assess", Effect::Read, json!({"score": 0.9}));
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("assess".to_owned(), Box::new(assess));
    tools.insert("http_post".to_owned(), Box::new(publish));
    let agents: HashMap<String, Agent> = HashMap::new();

    let graph = GraphBuilder::new()
        .tool(ToolSpec::new("assess", "assess"))
        .gate(
            GateSpec::new(
                "approve",
                json!({
                    "required": ["approved"],
                    "properties": {"approved": {"type": "boolean"}}
                }),
            )
            .prompt("Approve this draft for publication?"),
        )
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("assess", "approve")
        .edge("approve", "publish")
        .build();

    let input = json!({"topic": "otters"});
    let run_id = fixed_run_id(15);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // --- Drive 1: run the read tool, then park at the gate. ---
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let parked = run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph parks at the gate");
    assert!(
        matches!(&parked, GraphOutcome::Parked { node, .. } if node == "approve"),
        "expected a park at the gate, got {parked:?}"
    );
    let parked_log = store.read_log(run_id).await.expect("log reads");
    let parked_bytes = serde_json::to_string(&parked_log).expect("the log encodes");

    // --- Each of the four non-conforming approvals is refused for free. ---
    for bad in [json!(null), json!(42), json!("nope"), json!({})] {
        let mut refuse_ctx = RunCtx::with_hooks(
            store.clone(),
            run_id,
            parked_log.clone(),
            fixed_clock(),
            fixed_random(),
        )
        .expect("resume ctx builds");
        refuse_ctx.set_resume_input(bad.clone());
        let error = run_graph(&mut refuse_ctx, &graph, &input, &agents, &tools)
            .await
            .expect_err("a non-conforming approval is refused");
        match error {
            EngineError::ApprovalSchemaViolation { node, violations } => {
                assert_eq!(node, "approve", "the refusal names the gate");
                assert!(!violations.is_empty(), "the refusal lists what was wrong");
            }
            other => panic!("expected ApprovalSchemaViolation for {bad}, got {other:?}"),
        }
        // Nothing was appended, and the run is parked exactly as it was.
        let after = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            serde_json::to_string(&after).expect("the log encodes"),
            parked_bytes,
            "the refusal of {bad} must leave the log untouched"
        );
        assert_eq!(
            publish_calls.load(Ordering::SeqCst),
            0,
            "publish still waits"
        );
    }

    // --- The conforming approval behaves exactly as it always did. ---
    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        parked_log,
        fixed_clock(),
        fixed_random(),
    )
    .expect("resume ctx builds");
    ctx2.set_resume_input(json!({"approved": true}));
    let done = run_graph(&mut ctx2, &graph, &input, &agents, &tools)
        .await
        .expect("a conforming approval drives the run to completion");
    assert!(matches!(done, GraphOutcome::Completed { .. }));

    let live_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&live_log),
        [
            "GraphRunStarted",
            "NodeEntered", // assess
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited",  // assess
            "NodeEntered", // approve (gate)
            "Suspended",
            "Resumed",
            "NodeExited",  // approve
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ],
        "the four refusals left no trace between the Suspended and the Resumed"
    );

    // --- The replay proof: the recorded approval is trusted, not re-judged. ---
    let (assess_before, publish_before) = (
        assess_calls.load(Ordering::SeqCst),
        publish_calls.load(Ordering::SeqCst),
    );
    assert_eq!((assess_before, publish_before), (1, 1));
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
        .expect("the approved log replays");
    assert!(matches!(replayed, GraphOutcome::Completed { .. }));
    assert!(!replay_ctx.is_replaying(), "history fully consumed");
    assert_eq!(assess_calls.load(Ordering::SeqCst), assess_before);
    assert_eq!(publish_calls.load(Ordering::SeqCst), publish_before);
    assert_eq!(
        serde_json::to_string(&store.read_log(run_id).await.expect("log reads"))
            .expect("the log encodes"),
        serde_json::to_string(&live_log).expect("the log encodes"),
        "replaying an approved gate rewrites nothing"
    );
}

/// The determinism guardrail stated as its own claim: a `Resumed` that is
/// already in the log is history, and the accept edge does not get to reopen it.
///
/// The setup is the one that would bite if validation ever moved to the wrong
/// side of that edge: a log whose recorded approval is `42`, which the gate's
/// schema plainly refuses. That log is exactly what an OLD run looks like once
/// this validator (or a future, stricter one) is in place. Replaying it must
/// still complete. If it did not, shipping a stricter validator would silently
/// break every run approved before it, which is the property durable execution
/// exists to sell.
#[tokio::test]
async fn a_recorded_approval_is_never_re_judged_on_replay() {
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("http_post".to_owned(), Box::new(publish));
    let agents: HashMap<String, Agent> = HashMap::new();

    let schema = json!({
        "type": "object",
        "required": ["approved"],
        "properties": {"approved": {"type": "boolean"}}
    });
    let graph = GraphBuilder::new()
        .gate(GateSpec::new("approve", schema.clone()))
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("approve", "publish")
        .build();

    let input = json!({"topic": "otters"});
    let run_id = fixed_run_id(16);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("the graph parks at the gate");

    // The approval this gate would refuse today, recorded as if an older,
    // laxer Salvor had already accepted it.
    let stale_approval = json!(42);
    assert!(
        !salvor_engine::approval_violations(&stale_approval, &schema).is_empty(),
        "the accept edge really would refuse this approval"
    );
    let mut old_log = store.read_log(run_id).await.expect("log reads");
    let next_seq = salvor_core::SequenceNumber::new(old_log.len() as u64);
    old_log.push(salvor_core::EventEnvelope::new(
        run_id,
        next_seq,
        old_log
            .last()
            .expect("the parked log is not empty")
            .recorded_at,
        salvor_core::Event::Resumed {
            input: stale_approval,
            caller: None,
        },
    ));

    // Replay that log from its first byte. The gate must feed the recorded
    // approval straight through.
    let fresh = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut replay_ctx = RunCtx::with_hooks(
        fresh.clone(),
        run_id,
        old_log,
        fixed_clock(),
        fixed_random(),
    )
    .expect("replay ctx builds");
    let replayed = run_graph(&mut replay_ctx, &graph, &input, &agents, &tools)
        .await
        .expect("a recorded Resumed is never re-validated into a refusal");
    match replayed {
        GraphOutcome::Completed { output } => {
            assert_eq!(
                output,
                json!({"published": 42}),
                "the recorded approval passed through the gate unchanged"
            );
        }
        other => panic!("expected completion, got {other:?}"),
    }
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        1,
        "the run continued past the gate on its recorded approval"
    );
}
