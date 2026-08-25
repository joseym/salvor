//! The graph control plane over real loopback HTTP.
//!
//! The headline is the design claim proven end to end: a graph run is an
//! ordinary run with a richer log, so `GET /v1/runs/{id}`, `/v1/runs` (the
//! enriched list), and (the strongest evidence) `POST /v1/runs/{id}/resume`
//! all work on a graph run through their EXISTING code. The flagship test
//! submits an `agent -> gate -> tool` graph, starts a graph run, watches it
//! park at the gate, resumes it through the very same resume endpoint an agent
//! run uses, and sees it complete with the write tool having run exactly once.
//!
//! The rest pins the new surface: submit (strict, idempotent), list, get,
//! validate-only, the per-node projection, and every new error code
//! (`invalid_graph`, `unknown_graph`, `unknown_tool`, `unknown_agent`,
//! `not_a_graph_run`), and the terminal a PERMANENT engine refusal earns: the
//! driver records `RunFailed`, so a run that will refuse identically forever
//! reads `failed` on both `GET /v1/runs/{id}` and the enriched list rather than
//! sitting there looking like it is still going.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    ScriptedModel, TestServer, agent_factory, counter, get_json, memory_store, post, post_json,
    register_agent, sample_toml, text_response,
};
use reqwest::StatusCode;
use salvor_core::Effect;
use salvor_llm::Config;
use salvor_runtime::Agent;
use salvor_server::{AgentFactory, AppState, BuiltAgent, ToolRegistry};
use salvor_tools::{DynTool, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};

/// A minimal write tool with a shared execution counter, so a graph run can
/// prove its terminal write ran exactly once across the park and resume.
struct PublishTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DynTool for PublishTool {
    fn name(&self) -> &str {
        "publish"
    }
    fn description(&self) -> &str {
        "a publish test tool"
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Output(json!({ "published": input })))
    }
}

/// A factory that builds a model-only agent (no tools) pointed at `model_uri`.
/// Every build is identical, so the agent's hash is stable across register and
/// the per-run rebuild a resume does.
fn model_only_factory(model_uri: String) -> AgentFactory {
    Arc::new(move |_definition| {
        let model_uri = model_uri.clone();
        Box::pin(async move {
            let agent = Agent::builder()
                .model(
                    Config::new().with_base_url(&model_uri).with_max_retries(0),
                    "test-model",
                )
                .system_prompt("You are a test agent.")
                .build()
                .map_err(|error| error.to_string())?;
            Ok(BuiltAgent {
                agent,
                servers: vec![],
            })
        })
    })
}

/// Server state with the fixed hooks, a short poll, and a tool registry holding
/// the given tools.
fn graph_state(factory: AgentFactory, registry: ToolRegistry) -> AppState {
    AppState::new(memory_store(), factory)
        .with_hooks(common::fixed_clock(), common::fixed_random())
        .with_poll_interval(Duration::from_millis(10))
        .with_tool_registry(Arc::new(registry))
}

/// Polls `GET /v1/runs/{id}` until its status state matches `want`, or panics
/// after a generous timeout.
async fn wait_for_state(client: &reqwest::Client, base: &str, run: &str, want: &str) -> Value {
    for _ in 0..200 {
        let (_, body) = get_json(client, &format!("{base}/v1/runs/{run}"), None).await;
        if body["status"]["state"] == want {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    panic!("run {run} never reached state {want}");
}

/// The flagship: `agent -> gate -> tool` parks at the gate, resumes through the
/// EXISTING resume endpoint, and completes with the write tool run exactly once.
/// Also proves the graph run shows up in the enriched run list and projects its
/// per-node progress.
#[tokio::test]
async fn graph_run_parks_resumes_through_the_existing_endpoint_and_completes() {
    // The agent makes one model call (1 message) and returns text.
    let model = ScriptedModel::mount(vec![(1, text_response("reviewed", 5, 3), None)]).await;
    let publish_calls = counter();
    let registry = ToolRegistry::new().with_tool(Arc::new(PublishTool {
        calls: publish_calls.clone(),
    }));
    let state = graph_state(model_only_factory(model.uri()), registry);
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    // Register the agent to learn its stable hash, then author a graph that
    // references it.
    let agent_hash = register_agent(&client, &server.base, sample_toml(), None).await;
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "agent", "payload": { "id": "work", "agent_hash": agent_hash } },
            { "kind": "gate", "payload": { "id": "approve", "approval_schema": {
                "type": "object",
                "properties": { "approved": { "type": "boolean" } }
            } } },
            { "kind": "tool", "payload": { "id": "publish", "tool": "publish" } }
        ],
        "edges": [ { "from": "work", "to": "approve" }, { "from": "approve", "to": "publish" } ]
    });

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let graph_hash = body["graph"].as_str().expect("graph hash").to_owned();
    assert_eq!(body["created"], true);

    // Start the graph run.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": graph_hash }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "graph-run: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();

    // It runs the agent, then parks at the gate. The write has NOT fired.
    wait_for_state(&client, &server.base, &run, "suspended").await;
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        0,
        "the write waits behind the gate"
    );

    // The projection shows the gate as the current node, the agent exited.
    let (_, projection) = get_json(
        &client,
        &format!("{}/v1/runs/{run}/graph", server.base),
        None,
    )
    .await;
    assert_eq!(projection["graph_hash"], graph_hash);
    assert_eq!(projection["current_node"], "approve");

    // Resume through the SAME endpoint an agent run uses. No graph-specific
    // route, no new verb: the existing resume drives the graph to completion.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/resume", server.base),
        json!({ "input": { "approved": true } }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "resume: {body}");

    let completed = wait_for_state(&client, &server.base, &run, "completed").await;
    assert_eq!(
        completed["status"]["output"],
        json!({ "published": { "approved": true } })
    );
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        1,
        "the write ran exactly once, after approval"
    );

    // The graph run appears in the enriched run list, with the existing field
    // semantics intact: status/usage/step_count present, agent_def_hash absent
    // (a graph run has no single RunStarted agent hash: the honest absence).
    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .find(|entry| entry["run"] == run)
        .expect("the graph run is listed");
    assert_eq!(entry["status"]["state"], "completed");
    assert!(entry["usage"]["input_tokens"].is_number());
    assert!(entry["step_count"].as_u64().expect("step_count") >= 1);
    assert!(
        entry.get("agent_def_hash").is_none(),
        "a graph run has no single agent_def_hash to claim"
    );

    // The final projection: every node exited, nothing current.
    let (_, projection) = get_json(
        &client,
        &format!("{}/v1/runs/{run}/graph", server.base),
        None,
    )
    .await;
    assert!(projection.get("current_node").is_none());
    let nodes = projection["nodes"].as_array().expect("nodes");
    for id in ["work", "approve", "publish"] {
        let node = nodes.iter().find(|n| n["node"] == id).expect(id);
        assert_eq!(node["state"], "exited", "node {id} exited");
    }
}

/// Submit is strict and idempotent: a valid document stores under its hash and
/// re-submitting the identical document reports `created: false` with the same
/// hash.
#[tokio::test]
async fn submit_is_strict_and_idempotent() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    // The gate carries an optional display `name`, so this test doubles as the
    // server's coverage that a named node round-trips byte-faithfully end to
    // end: submitted, stored content-addressed, and returned unchanged.
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "gate", "payload": {
                "id": "approve", "name": "Approve the draft", "approval_schema": { "type": "object" }
            } }
        ],
        "edges": []
    });

    let (status, first) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["created"], true);
    let hash = first["graph"].as_str().expect("hash").to_owned();

    let (status, second) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(second["created"], false, "re-submit is idempotent");
    assert_eq!(second["graph"], hash, "same document, same hash");

    // It is retrievable and listed.
    let (status, got) = get_json(&client, &format!("{}/v1/graphs/{hash}", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["graph"], hash);
    assert_eq!(got["document"]["schema_version"], 1);
    assert_eq!(
        got["document"]["nodes"][0]["payload"]["name"], "Approve the draft",
        "the node's optional display name round-trips byte-faithfully"
    );

    let (_, list) = get_json(&client, &format!("{}/v1/graphs", server.base), None).await;
    let entry = list["graphs"]
        .as_array()
        .expect("graphs array")
        .iter()
        .find(|entry| entry["graph"] == hash)
        .expect("the graph is listed");
    assert_eq!(entry["node_count"], 1);
    assert_eq!(entry["entry_nodes"], json!(["approve"]));
}

/// A branch case with no outbound edge realizing it is refused with
/// `400 invalid_graph`, naming both the branch node and the unrouted case, so a
/// misspelled edge label (`lst` for the case `lost`) is caught at submit rather
/// than silently skipping the intended route at run time.
#[tokio::test]
async fn submit_rejects_a_branch_case_with_no_matching_edge() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "tool", "payload": { "id": "assess", "tool": "assess" } },
            { "kind": "branch", "payload": { "id": "route", "on": "assess.outcome", "cases": [
                { "name": "won", "when": { "kind": "expression", "value": "outcome == \"won\"" } },
                { "name": "lost", "when": { "kind": "expression", "value": "outcome == \"lost\"" } }
              ] } },
            { "kind": "tool", "payload": { "id": "celebrate", "tool": "notify" } }
        ],
        "edges": [
            { "from": "assess", "to": "route" },
            { "from": "route", "to": "celebrate", "label": "won" },
            { "from": "route", "to": "celebrate", "label": "lst" }
        ]
    });

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_graph");
    let errors = body["error"]["details"]["errors"]
        .as_array()
        .expect("error list");
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "branch_case_without_edge"
                && e["node"] == "route"
                && e["case"] == "lost"),
        "the unrouted case is named node/case-precise: {errors:?}"
    );
}

/// An outbound edge from a branch labeled with a name no case declares is
/// refused with `400 invalid_graph`, naming both the branch node and the
/// offending label: the mirror of
/// `submit_rejects_a_branch_case_with_no_matching_edge`, from the edge's side
/// of the same typo (`lst` for the case `lost`).
#[tokio::test]
async fn submit_rejects_a_branch_edge_labeled_with_no_matching_case() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "tool", "payload": { "id": "assess", "tool": "assess" } },
            { "kind": "branch", "payload": { "id": "route", "on": "assess.outcome", "cases": [
                { "name": "lost", "when": { "kind": "expression", "value": "outcome == \"lost\"" } },
                { "name": "paid", "when": { "kind": "expression", "value": "outcome == \"paid\"" } }
              ] } },
            { "kind": "tool", "payload": { "id": "close", "tool": "notify" } },
            { "kind": "tool", "payload": { "id": "celebrate", "tool": "notify" } }
        ],
        "edges": [
            { "from": "assess", "to": "route" },
            { "from": "route", "to": "close", "label": "lst" },
            { "from": "route", "to": "celebrate", "label": "paid" }
        ]
    });

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_graph");
    let errors = body["error"]["details"]["errors"]
        .as_array()
        .expect("error list");
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "branch_edge_without_case"
                && e["node"] == "route"
                && e["label"] == "lst"),
        "the mislabeled edge is named node/label-precise: {errors:?}"
    );
}

/// An outbound edge from a branch that carries no label at all is refused
/// with `400 invalid_graph`, naming both the branch node and the edge's
/// target: it can never fire either, by the same engine rule as a
/// mismatched label, so it gets the same submit-time treatment.
#[tokio::test]
async fn submit_rejects_a_branch_edge_with_no_label() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "tool", "payload": { "id": "assess", "tool": "assess" } },
            { "kind": "branch", "payload": { "id": "route", "on": "assess.outcome", "cases": [
                { "name": "paid", "when": { "kind": "expression", "value": "outcome == \"paid\"" } },
                { "name": "lost", "when": { "kind": "expression", "value": "outcome == \"lost\"" } }
              ] } },
            { "kind": "tool", "payload": { "id": "celebrate", "tool": "notify" } },
            { "kind": "tool", "payload": { "id": "close", "tool": "notify" } }
        ],
        "edges": [
            { "from": "assess", "to": "route" },
            { "from": "route", "to": "celebrate", "label": "paid" },
            { "from": "route", "to": "close" }
        ]
    });

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_graph");
    let errors = body["error"]["details"]["errors"]
        .as_array()
        .expect("error list");
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "branch_edge_without_label"
                && e["node"] == "route"
                && e["to"] == "close"),
        "the unlabelled edge is named node/target-precise: {errors:?}"
    );
}

/// A document with a dangling edge is refused with `400 invalid_graph` carrying
/// the complete, node/edge-precise error list.
#[tokio::test]
async fn submit_rejects_an_invalid_document_with_the_full_error_list() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } }
        ],
        "edges": [ { "from": "approve", "to": "ghost" } ]
    });

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_graph");
    let errors = body["error"]["details"]["errors"]
        .as_array()
        .expect("error list");
    assert!(
        errors.iter().any(|e| e["code"] == "dangling_edge"
            && e["edge"]["to"] == "ghost"
            && e["missing"] == "ghost"),
        "the dangling edge is named node/edge-precise: {errors:?}"
    );
}

/// The validate-only endpoint never stores and answers the question either way:
/// `valid: true` + summary for a good document, `valid: false` + the error list
/// for a bad one.
#[tokio::test]
async fn validate_only_reports_validity_without_storing() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();

    let good = json!({
        "schema_version": 1,
        "nodes": [ { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } } ],
        "edges": []
    });
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs/validate", server.base),
        good,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], true);
    assert_eq!(body["summary"]["node_count"], 1);
    let hash = body["graph"].as_str().expect("hash").to_owned();

    // Nothing was stored: fetching that hash is a 404.
    let (status, _) = get_json(&client, &format!("{}/v1/graphs/{hash}", server.base), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "validate never stores");

    let bad = json!({
        "schema_version": 1,
        "nodes": [ { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } } ],
        "edges": [ { "from": "approve", "to": "ghost" } ]
    });
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs/validate", server.base),
        bad,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], false);
    assert!(!body["errors"].as_array().expect("errors").is_empty());
}

/// Starting a run of an unknown graph hash is `404 unknown_graph`.
#[tokio::test]
async fn graph_run_of_unknown_graph_is_404() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": "sha256:deadbeef" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_graph");
}

/// A graph whose `tool` node names a tool the server's registry does not hold is
/// refused at submit of the run with `404 unknown_tool`, naming the node. On a
/// stock (empty-registry) server this is EVERY tool node: tools resolve
/// through the existing registry and nothing invents a new registration
/// surface.
#[tokio::test]
async fn graph_run_with_an_unregistered_tool_is_404_unknown_tool() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        // Registry holds "publish"; the graph names "missing".
        ToolRegistry::new().with_tool(Arc::new(PublishTool { calls: counter() })),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [ { "kind": "tool", "payload": { "id": "step", "tool": "missing" } } ],
        "edges": []
    });
    let (_, submit) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    let hash = submit["graph"].as_str().expect("hash");

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": hash }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_tool");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("step"),
        "the error names the node"
    );
}

/// A graph whose `agent` node references an unregistered hash is refused at
/// submit of the run with `404 unknown_agent`.
#[tokio::test]
async fn graph_run_with_an_unregistered_agent_is_404_unknown_agent() {
    let server = TestServer::spawn(graph_state(
        agent_factory(
            String::new(),
            "unused",
            Effect::Read,
            common::CountBehavior::Record,
            counter(),
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let document = json!({
        "schema_version": 1,
        "nodes": [ { "kind": "agent", "payload": {
            "id": "work",
            "agent_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
        } } ],
        "edges": []
    });
    let (_, submit) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    let hash = submit["graph"].as_str().expect("hash");

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": hash }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_agent");
}

/// The per-run graph projection refuses an ordinary agent run with `409
/// not_a_graph_run`, mirroring the existing 409 error shape.
#[tokio::test]
async fn graph_projection_of_an_agent_run_is_409_not_a_graph_run() {
    let calls = counter();
    let model = ScriptedModel::mount(vec![(1, text_response("done", 4, 2), None)]).await;
    let server = TestServer::spawn(graph_state(
        agent_factory(
            model.uri(),
            "noop",
            Effect::Read,
            common::CountBehavior::Record,
            calls,
        ),
        ToolRegistry::new(),
    ))
    .await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    // Start an ordinary agent run.
    let (status, body) = post(
        &client,
        &format!("{}/v1/runs", server.base),
        "application/json",
        json!({ "agent": agent, "input": {} }).to_string(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    wait_for_state(&client, &server.base, &run, "completed").await;

    let (status, body) = get_json(
        &client,
        &format!("{}/v1/runs/{run}/graph", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "not_a_graph_run");
}

/// A gate's `approval_schema` is ENFORCED at the resume endpoint, not merely
/// recorded and advertised.
///
/// The schema is the one that used to let everything through: `required` and
/// `properties` with no `type`, which plain JSON Schema semantics leave
/// vacuously satisfied by any non-object. All four of `null`, `42`, `"nope"`,
/// and `{}` come back as `400 approval_schema_violation` naming the gate node
/// and listing every violation, the log is byte-identical after each refusal,
/// and the run is still parked at the gate. A conforming approval then resumes
/// it and the write runs exactly once.
#[tokio::test]
async fn resume_refuses_an_approval_that_violates_the_gates_schema() {
    let publish_calls = counter();
    let registry = ToolRegistry::new().with_tool(Arc::new(PublishTool {
        calls: publish_calls.clone(),
    }));
    // The store is held here, not hidden inside `graph_state`, so the test can
    // read the raw log and prove a refusal appended nothing.
    let store = memory_store();
    let state = AppState::new(
        store.clone(),
        model_only_factory("http://model.invalid".to_owned()),
    )
    .with_hooks(common::fixed_clock(), common::fixed_random())
    .with_poll_interval(Duration::from_millis(10))
    .with_tool_registry(Arc::new(registry));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    // A gate-entry graph: no agent, so no model is ever called.
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "gate", "payload": { "id": "approve", "approval_schema": {
                "required": ["approved"],
                "properties": { "approved": { "type": "boolean" } }
            } } },
            { "kind": "tool", "payload": { "id": "publish", "tool": "publish" } }
        ],
        "edges": [ { "from": "approve", "to": "publish" } ]
    });
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let graph_hash = body["graph"].as_str().expect("graph hash").to_owned();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": graph_hash }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "graph-run: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    let run_id = salvor_core::RunId::from_uuid(run.parse().expect("the run id is a uuid"));

    wait_for_state(&client, &server.base, &run, "suspended").await;
    let parked = common::read_log(&store, run_id).await;
    let parked_bytes = serde_json::to_string(&parked).expect("the log encodes");

    for bad in [json!(null), json!(42), json!("nope"), json!({})] {
        let (status, body) = post_json(
            &client,
            &format!("{}/v1/runs/{run}/resume", server.base),
            json!({ "input": bad }),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "resuming with {bad} must be refused: {body}"
        );
        assert_eq!(body["error"]["code"], "approval_schema_violation", "{body}");
        assert_eq!(
            body["error"]["details"]["node"], "approve",
            "the refusal names the gate: {body}"
        );
        let violations = body["error"]["details"]["violations"]
            .as_array()
            .unwrap_or_else(|| panic!("a violation list: {body}"));
        assert!(!violations.is_empty(), "{body}");
        assert!(violations[0]["path"].is_string(), "{body}");
        assert!(violations[0]["message"].is_string(), "{body}");

        // Nothing was appended and nothing ran.
        assert_eq!(
            serde_json::to_string(&common::read_log(&store, run_id).await)
                .expect("the log encodes"),
            parked_bytes,
            "the refusal of {bad} must leave the log untouched"
        );
        assert_eq!(publish_calls.load(Ordering::SeqCst), 0);
        let (_, current) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
        assert_eq!(
            current["status"]["state"], "suspended",
            "the run is still parked at the gate"
        );
    }

    // The conforming approval behaves exactly as it always did.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/resume", server.base),
        json!({ "input": { "approved": true } }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "resume: {body}");
    let completed = wait_for_state(&client, &server.base, &run, "completed").await;
    assert_eq!(
        completed["status"]["output"],
        json!({ "published": { "approved": true } })
    );
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);
}

/// A PERMANENT engine refusal on the server ends the run: the spawned driver
/// records the terminal `RunFailed`, so `GET /v1/runs/{id}` reports `failed`
/// and the enriched list agrees, rather than showing a run that will never
/// move again as though it were still going.
///
/// Nothing in the status vocabulary or the dispatch needed changing for this:
/// `RunFailed` is an event `derive_state` has always mapped to `failed`. What
/// changed is that a refusal the engine will reproduce forever now writes one.
/// The graph here refuses offline: an entry `branch` whose two cases both read
/// a `score` the input does not carry, so no case matches and no model or tool
/// is ever reached.
#[tokio::test]
async fn a_permanent_engine_refusal_leaves_the_graph_run_failed_not_running() {
    let model = ScriptedModel::mount(vec![]).await;
    let state = graph_state(model_only_factory(model.uri()), ToolRegistry::new());
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "branch", "payload": {
                "id": "route",
                "on": "score",
                "cases": [
                    { "name": "high", "when": { "kind": "expression", "value": "score >= 0.8" } },
                    { "name": "low", "when": { "kind": "expression", "value": "score < 0.8" } }
                ]
            } },
            { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } },
            { "kind": "gate", "payload": { "id": "reject", "approval_schema": { "type": "object" } } }
        ],
        "edges": [
            { "from": "route", "to": "approve", "label": "high" },
            { "from": "route", "to": "reject", "label": "low" }
        ]
    });
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graphs", server.base),
        document,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let graph_hash = body["graph"].as_str().expect("graph hash").to_owned();

    // The document resolves (no agents, no tools), so the run really starts.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/graph-runs", server.base),
        json!({ "graph_hash": graph_hash, "input": {"topic": "otters"} }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "graph-run: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();

    // The driver's terminal, read back through the endpoints that always
    // existed.
    let body = wait_for_state(&client, &server.base, &run, "failed").await;
    let error = body["status"]["error"].as_str().expect("a failure message");
    assert!(
        error.contains("branch node `route`"),
        "the recorded failure names the node that refused: {error}"
    );

    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .find(|entry| entry["run"] == run)
        .expect("the refused run is listed");
    assert_eq!(entry["status"]["state"], "failed");
}
