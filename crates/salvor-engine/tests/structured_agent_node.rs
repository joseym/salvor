//! An `agent` node's declared `output_schema`, at runtime.
//!
//! The field used to be documentation the engine only read at load time, for
//! the edge type-compatibility check. Now it also decides how the node's loop
//! ends: with a schema, the runtime offers the model a forced answer call
//! carrying that schema and validates the answer, so the node's output is a
//! structured object the rest of the graph can read fields from. Without one,
//! nothing changes: the output is the model's reply text, exactly as before.
//!
//! Both halves are here side by side, over the same shape of graph, because
//! the difference between them is the whole feature.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    EchoTool, ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_use_response,
};
use salvor_core::Effect;
use salvor_engine::{GraphOutcome, run_graph};
use salvor_graph::{AgentSpec, BranchCondition, BranchSpec, Graph, GraphBuilder, ToolSpec};
use salvor_runtime::{ANSWER_TOOL, Agent, RunCtx};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};

/// The agent hash both graphs register their `rate` node under.
const RATE_HASH: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

/// The schema the structured graph declares on its `rate` node.
fn declared_schema() -> Value {
    json!({
        "type": "object",
        "required": ["score"],
        "properties": {"score": {"type": "number"}, "verdict": {"type": "string"}}
    })
}

/// `rate` -> `route` -> `publish`: an agent node, a branch whose expression
/// reads a FIELD of the agent's output, and a tool that receives whatever the
/// branch routed. `output_schema` is the only thing that varies.
fn rating_graph(output_schema: Option<Value>) -> Graph {
    let mut rate = AgentSpec::new("rate", RATE_HASH);
    if let Some(schema) = output_schema {
        rate = rate.output_schema(schema);
    }
    GraphBuilder::new()
        .agent(rate)
        .branch(
            BranchSpec::new("route")
                .case("high", BranchCondition::Expression("score >= 0.8".into())),
        )
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("rate", "route")
        .labeled_edge("route", "publish", "high")
        .build()
}

#[tokio::test]
async fn a_declared_output_schema_makes_the_node_output_a_structured_object() {
    let server = ScriptedModel::mount(vec![(
        1,
        tool_use_response(
            "tu_answer",
            ANSWER_TOOL,
            json!({"score": 0.87, "verdict": "ship it"}),
            5,
            3,
        ),
    )])
    .await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        RATE_HASH.to_owned(),
        agent_builder(&server.uri()).build().expect("agent builds"),
    );
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("http_post".to_owned(), Box::new(publish));

    let run_id = fixed_run_id(80);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(
        &mut ctx,
        &rating_graph(Some(declared_schema())),
        &json!({"draft": "otters"}),
        &agents,
        &tools,
    )
    .await
    .expect("the graph drives");
    let GraphOutcome::Completed { output } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };

    // The agent node's output is the validated answer object, so the branch's
    // expression could read `score` from it and the tool downstream received
    // the object rather than a sentence about it.
    assert_eq!(
        output,
        json!({"published": {"score": 0.87, "verdict": "ship it"}})
    );
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered", // rate
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited",  // rate
            "NodeEntered", // route
            "BranchTaken",
            "NodeExited",  // route
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ],
        "the structured node records the same events an agent node always did"
    );

    // The declared schema reached the provider as the answer tool's own input
    // schema, with a tool call required.
    let requests = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    let offered = body["tools"].as_array().expect("the request offers tools");
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0]["name"], json!(ANSWER_TOOL));
    assert_eq!(offered[0]["input_schema"], declared_schema());
    assert_eq!(body["tool_choice"], json!({"type": "any"}));
}

#[tokio::test]
async fn a_node_without_an_output_schema_still_answers_in_text() {
    let server = ScriptedModel::mount(vec![(1, text_response("score: about 0.9", 5, 3))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        RATE_HASH.to_owned(),
        agent_builder(&server.uri()).build().expect("agent builds"),
    );
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("http_post".to_owned(), Box::new(publish));

    // Same graph without the schema, and with the branch removed: a text reply
    // has no `score` for an expression to read, which is exactly the state of
    // affairs this feature exists to change and which must be left intact for
    // every node that does not opt in.
    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("rate", RATE_HASH))
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("rate", "publish")
        .build();

    let run_id = fixed_run_id(81);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(
        &mut ctx,
        &graph,
        &json!({"draft": "otters"}),
        &agents,
        &tools,
    )
    .await
    .expect("the graph drives");
    let GraphOutcome::Completed { output } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    assert_eq!(output, json!({"published": "score: about 0.9"}));
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);

    // No answer tool, no forced call: a node that declares nothing sends the
    // request it always sent.
    let requests = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
}
