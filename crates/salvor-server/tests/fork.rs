//! The fork control plane over real loopback HTTP: refuse-then-record end to end.
//!
//! The differentiator on the wire. A `research (agent) -> publish (write tool)`
//! graph run completes; forking it past the recorded write is REFUSED with `409
//! write_replay_hazard` listing exactly that write, and only an acknowledgement
//! that covers the hazard lets the fork proceed. The child then re-executes the
//! write once, the origin is untouched, the child projects its `forked_from`,
//! and `GET /v1/runs/{id}/forks` finds the child as derived data. The structural
//! refusals (`not_a_graph_run`, `invalid_fork_node`, `unknown_run`) and the
//! `dry_run` preview round it out, and `GET /v1/capabilities` advertises the
//! feature the dashboard gates on.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    ScriptedModel, TestServer, get_json, memory_store, post_json, register_agent, sample_toml,
    text_response,
};
use reqwest::StatusCode;
use salvor_core::Effect;
use salvor_llm::Config;
use salvor_runtime::Agent;
use salvor_server::{AgentFactory, AppState, BuiltAgent, ToolRegistry};
use salvor_tools::{DynTool, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};

/// A write tool with a shared execution counter, so a fork can prove its
/// re-walked write executes exactly once in the child.
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

/// A model-only agent factory pointed at `model_uri`. Every build is identical,
/// so the agent hash is stable across register and the per-run rebuild.
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

fn graph_state(factory: AgentFactory, registry: ToolRegistry) -> AppState {
    AppState::new(memory_store(), factory)
        .with_hooks(common::fixed_clock(), common::fixed_random())
        .with_poll_interval(Duration::from_millis(10))
        .with_tool_registry(Arc::new(registry))
}

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

/// Submits `research (agent) -> publish (write)`, runs it to completion, and
/// returns `(base, client, run_id, graph_hash, publish_calls)`.
async fn completed_write_run() -> (
    TestServer,
    reqwest::Client,
    String,
    String,
    Arc<AtomicUsize>,
) {
    let model = ScriptedModel::mount(vec![(1, text_response("a draft", 5, 3), None)]).await;
    let publish_calls = counter();
    let registry = ToolRegistry::new().with_tool(Arc::new(PublishTool {
        calls: publish_calls.clone(),
    }));
    let state = graph_state(model_only_factory(model.uri()), registry);
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let agent_hash = register_agent(&client, &server.base, sample_toml(), None).await;
    let document = json!({
        "schema_version": 1,
        "nodes": [
            { "kind": "agent", "payload": { "id": "research", "agent_hash": agent_hash } },
            { "kind": "tool", "payload": { "id": "publish", "tool": "publish" } }
        ],
        "edges": [ { "from": "research", "to": "publish" } ]
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

    let completed = wait_for_state(&client, &server.base, &run, "completed").await;
    assert_eq!(
        completed["status"]["output"],
        json!({ "published": "a draft" })
    );
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        1,
        "the write ran once live"
    );

    (server, client, run, graph_hash, publish_calls)
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// The whole fork contract on the wire: refuse past the write, acknowledge, then
/// the child re-executes it once, the origin is untouched, the child projects
/// its fork origin, and the derived forks listing finds it.
#[tokio::test]
async fn fork_refuses_then_records_and_the_child_runs() {
    let (server, client, run, _graph_hash, publish_calls) = completed_write_run().await;

    // The origin's event count, for the immutability proof.
    let (_, before) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    let origin_events_before = before["event_count"].as_u64().expect("event_count");

    // --- Acceptance 1: forking past the write REFUSES, listing exactly it. ---
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/fork", server.base),
        json!({ "from_node": "publish" }),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "fork without ack refuses: {body}"
    );
    assert_eq!(body["error"]["code"], "write_replay_hazard");
    let writes = body["error"]["details"]["writes"]
        .as_array()
        .expect("writes list");
    assert_eq!(writes.len(), 1, "exactly the one downstream write");
    let write = &writes[0];
    assert_eq!(write["tool"], "publish");
    assert_eq!(
        write["effect"].as_str(),
        None,
        "write intent carries no effect key here"
    );
    assert_eq!(
        write["input"],
        json!("a draft"),
        "the recorded input is present verbatim"
    );
    assert!(
        write["idempotency_key"].is_null(),
        "a write carries no idempotency key"
    );
    assert!(
        write["recorded_at"].is_string(),
        "the recorded time is present"
    );
    let hazard_seq = write["seq"].as_u64().expect("the write's seq");

    // A partial acknowledgement (wrong seq) still refuses.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/fork", server.base),
        json!({ "from_node": "publish", "acknowledge_writes": [hazard_seq + 999] }),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a non-covering ack refuses: {body}"
    );
    assert_eq!(body["error"]["code"], "write_replay_hazard");

    // --- dry_run: preview the hazard, create nothing. ---
    let (status, preview) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/fork", server.base),
        json!({ "from_node": "publish", "dry_run": true }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dry run is a preview: {preview}");
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["would_proceed"], false);
    assert_eq!(preview["writes"].as_array().expect("writes").len(), 1);
    assert_eq!(preview["from_node"], "publish");

    // --- Acceptance: with acknowledgement the fork proceeds. ---
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/fork", server.base),
        json!({ "from_node": "publish", "acknowledge_writes": [hazard_seq] }),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "acknowledged fork proceeds: {body}"
    );
    let child = body["run"].as_str().expect("child run id").to_owned();
    assert_eq!(body["forked_from"]["run_id"], run);
    assert_eq!(body["forked_from"]["from_node"], "publish");
    assert_eq!(
        body["forked_from"]["acknowledged_writes"],
        json!([hazard_seq])
    );

    // The child drives from the fork node and completes.
    let completed = wait_for_state(&client, &server.base, &child, "completed").await;
    assert_eq!(
        completed["status"]["output"],
        json!({ "published": "a draft" })
    );

    // --- Acceptance 3: the write executed EXACTLY once more (in the child). ---
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        2,
        "the child re-executed the write exactly once"
    );

    // --- Acceptance 4: the ORIGIN's event count is identical before and after. ---
    let (_, after) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(
        after["event_count"].as_u64().expect("event_count"),
        origin_events_before,
        "the origin log is unchanged by the fork"
    );

    // --- Acceptance 6: the child's graph projection shows forked_from. ---
    let (_, projection) = get_json(
        &client,
        &format!("{}/v1/runs/{child}/graph", server.base),
        None,
    )
    .await;
    assert_eq!(projection["forked_from"]["run_id"], run);
    assert_eq!(projection["forked_from"]["from_node"], "publish");
    assert_eq!(
        projection["forked_from"]["acknowledged_writes"],
        json!([hazard_seq])
    );

    // --- The forks listing is DERIVED and finds the child. ---
    let (_, listing) = get_json(
        &client,
        &format!("{}/v1/runs/{run}/forks", server.base),
        None,
    )
    .await;
    assert_eq!(
        listing["derived"], true,
        "the forks listing is labeled derived"
    );
    let forks = listing["forks"].as_array().expect("forks array");
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["run"], child);
    assert_eq!(forks[0]["from_node"], "publish");

    // The parent (immutable) records no forward pointer: its own projection has
    // no forked_from, and it did not gain one.
    let (_, parent_projection) = get_json(
        &client,
        &format!("{}/v1/runs/{run}/graph", server.base),
        None,
    )
    .await;
    assert!(
        parent_projection.get("forked_from").is_none(),
        "the origin never points at its forks"
    );
}

/// The structural refusals, each typed and precise.
#[tokio::test]
async fn fork_refusals_are_typed() {
    let (server, client, run, _graph_hash, _publish_calls) = completed_write_run().await;

    // A node the run never entered.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/fork", server.base),
        json!({ "from_node": "ghost" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "invalid_fork_node");

    // An unknown run.
    let missing = uuid::Uuid::new_v4();
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{missing}/fork", server.base),
        json!({ "from_node": "publish" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_run");

    // Forking an ordinary AGENT run is not_a_graph_run. Start one and let it
    // complete (the model-only agent returns text).
    let agent_hash = register_agent(&client, &server.base, sample_toml(), None).await;
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent_hash, "input": {} }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let agent_run = body["run"].as_str().expect("agent run id").to_owned();
    wait_for_state(&client, &server.base, &agent_run, "completed").await;

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{agent_run}/fork", server.base),
        json!({ "from_node": "publish" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "not_a_graph_run");
}

/// The server advertises the fork capability the dashboard gates on.
#[tokio::test]
async fn capabilities_advertises_fork() {
    let (server, client, _run, _graph_hash, _publish_calls) = completed_write_run().await;
    let (status, body) = get_json(&client, &format!("{}/v1/capabilities", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["capabilities"]["fork"], true);
}
