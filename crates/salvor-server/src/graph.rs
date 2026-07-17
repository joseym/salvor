//! The graph control plane: submit and inspect graph documents, start graph
//! runs, and project a graph run's per-node progress.
//!
//! # A graph run is an ordinary run with a richer log
//!
//! This module adds no new durability story. A graph run's log opens with
//! `GraphRunStarted` instead of `RunStarted`, and its nodes narrate the walk
//! with `NodeEntered`/`NodeExited`/`BranchTaken`/…, but every model call, tool
//! call, suspension, and completion inside it is recorded through the exact
//! same [`RunCtx`](salvor_runtime::RunCtx) machinery an agent run uses. So the
//! pure-read endpoints ([`GET /v1/runs/{id}`](crate::runs::get),
//! [`/replay`](crate::runs::replay), [`/events`](crate::sse::stream)) and even
//! [`POST /v1/runs/{id}/resume`](crate::runs::resume) work on a graph run with
//! their existing code: they fold or drive the log, and a graph run is just a
//! log. The one place resume needs help is re-driving: the log carries only the
//! graph's *hash*, so resume looks the document back up here and drives it with
//! the engine rather than the built-in loop. See [`drive_resume`].
//!
//! # Strict at submit, honest at resolution
//!
//! `POST /v1/graphs` validates a document all at once (collect-all,
//! node/edge-precise) and refuses the whole thing with `400 invalid_graph` on
//! any error — the strict-at-submit posture a control document earns. Starting
//! a run then resolves what the document references against the server's live
//! inventory, synchronously, before the run is spawned: every `agent` node's
//! hash must be a registered agent (built through the same
//! [`AgentFactory`](crate::AgentFactory) an agent run uses), and every `tool`
//! node's tool must be present in the server's [`ToolRegistry`]. A missing
//! reference is a synchronous `404` naming the node, not a background failure,
//! so an operator learns at submit that a run cannot proceed rather than
//! watching it stall.
//!
//! # Tool resolution: the existing registry, nothing new
//!
//! A graph `tool` node resolves through the server's [`ToolRegistry`] — the
//! SAME seam a client-driven tool step dispatches through. No new
//! tool-registration surface exists for graphs. `salvor serve` wires that
//! registry EMPTY, so on a stock server every `tool` node is a precise
//! `unknown_tool` refusal until a host registers the tool it names; a test (or
//! a future `aarg serve`) injects a registry holding its tools.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use salvor_core::{Event, EventEnvelope, RunId};
use salvor_engine::{GraphOutcome, graph_hash, run_graph};
use salvor_graph::{Graph, GraphError, GraphSummary, Node};
use salvor_replay::{GraphProjection, NodeState, derive_graph_projection};
use salvor_runtime::{Agent, RunCtx, validate_labels};
use salvor_tools::mcp::McpServer;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::{AppState, BuiltAgent};
use crate::tool_registry::ToolRegistry;

/// A graph `tool` node resolves through the server's whole tool inventory: the
/// [`ToolRegistry`] the client-driven tool step already dispatches through.
/// Implementing the engine's resolver on it directly is what lets the engine
/// stay ignorant of where the server keeps its tools.
impl salvor_engine::ToolResolver for ToolRegistry {
    fn resolve_tool(&self, name: &str) -> Option<&dyn salvor_tools::DynTool> {
        self.resolve(name)
    }
}

/// The body of `POST /v1/graph-runs`.
#[derive(Debug, Deserialize)]
struct StartRunRequest {
    /// The hash of a previously submitted graph document.
    graph_hash: String,
    /// The run input. Defaults to JSON null when omitted.
    #[serde(default)]
    input: Value,
    /// Optional correlation tags, recorded once on `GraphRunStarted`, checked
    /// against the same bounds an agent run's labels are.
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

/// Which verb a graph driver task runs. Mirrors the built-in loop's start /
/// resume / recover, but over the engine rather than the agent loop.
enum GraphVerb {
    Start {
        input: Value,
        labels: Option<BTreeMap<String, String>>,
    },
    Resume(Value),
    Recover,
}

// ---------------------------------------------------------------------------
// Graph documents: submit, list, get, validate.
// ---------------------------------------------------------------------------

/// `POST /v1/graphs`: submit (and strictly validate) a graph document.
///
/// The body is the document JSON. On success the document is stored
/// content-addressed by its reproducible [`graph_hash`], and the response is
/// `{ "graph": "<hash>", "created": <bool> }`, where `created` is `false` when
/// the identical document was already stored (idempotent re-submit). Any
/// validation failure is `400 invalid_graph` carrying the complete error list.
pub async fn submit(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let graph = parse_and_validate(&body)?;
    let hash = graph_hash(&graph).map_err(|error| ApiError::Internal(error.to_string()))?;
    let created = state.store_graph(hash.clone(), graph);
    Ok((
        StatusCode::CREATED,
        Json(json!({ "graph": hash, "created": created })),
    ))
}

/// `GET /v1/graphs`: one entry per stored graph, each with its hash and a
/// shape summary (node/edge counts, entry and terminal node ids), following the
/// enriched-runs precedent.
pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let graphs: Vec<Value> = state
        .graph_hashes()
        .into_iter()
        .filter_map(|hash| state.graph(&hash).map(|graph| (hash, graph)))
        .map(|(hash, graph)| {
            let mut entry = json!({ "graph": hash });
            // A stored graph passed validation at submit, so this summarizes
            // rather than re-validates; the fields are omitted (never faked)
            // in the impossible event a stored document does not summarize.
            if let Ok(summary) = salvor_graph::validate(&graph) {
                let object = entry.as_object_mut().expect("entry is a JSON object");
                for (key, value) in summary_fields(&summary) {
                    object.insert(key, value);
                }
            }
            entry
        })
        .collect();
    Json(json!({ "graphs": graphs }))
}

/// `GET /v1/graphs/{hash}`: the stored document, under its hash. `404
/// unknown_graph` when nothing is stored under the hash.
pub async fn get(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let graph = state
        .graph(&hash)
        .ok_or_else(|| ApiError::UnknownGraph(format!("no graph stored under `{hash}`")))?;
    let document = serde_json::to_value(&graph)
        .map_err(|error| ApiError::Internal(format!("re-encoding stored graph: {error}")))?;
    Ok(Json(json!({ "graph": hash, "document": document })))
}

/// `POST /v1/graphs/validate`: validate a document without storing it.
///
/// This is submit's dry run, the graph counterpart of `/replay`: it always
/// answers the question rather than treating an invalid document as a bad
/// request. On a valid document it returns `{ "valid": true, "graph": "<hash>",
/// "summary": {...} }` (the hash the document WOULD get, plus the shape summary
/// the CLI prints); on an invalid one it returns `{ "valid": false, "errors":
/// [...] }` with the same node/edge-precise list `POST /v1/graphs` would refuse
/// with. Nothing is ever stored.
pub async fn validate_only(body: Bytes) -> impl IntoResponse {
    match parse_and_validate(&body) {
        Ok(graph) => {
            let hash = graph_hash(&graph).unwrap_or_default();
            let summary = salvor_graph::validate(&graph).expect("just validated");
            let mut object = serde_json::Map::new();
            object.insert("valid".to_owned(), json!(true));
            object.insert("graph".to_owned(), json!(hash));
            object.insert("summary".to_owned(), json!(summary_object(&summary)));
            Json(Value::Object(object))
        }
        Err(ApiError::InvalidGraph { errors, .. }) => {
            Json(json!({ "valid": false, "errors": errors }))
        }
        // parse_and_validate only ever yields Ok or InvalidGraph.
        Err(_) => Json(json!({ "valid": false, "errors": [] })),
    }
}

// ---------------------------------------------------------------------------
// Graph runs.
// ---------------------------------------------------------------------------

/// `POST /v1/graph-runs`: start a run of a stored graph, and return its id at
/// once (the same fire-and-return shape `POST /v1/runs` uses).
///
/// Resolution is synchronous and up front: `404 unknown_graph` for an unknown
/// hash, `400 bad_request` for out-of-bounds labels, `404 unknown_agent` /
/// `404 unknown_tool` (naming the node) for a reference the server cannot
/// supply. Only once everything resolves is the run spawned.
pub async fn start_run(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let request: StartRunRequest = parse_body(&body)?;
    if let Some(labels) = &request.labels {
        validate_labels(labels).map_err(ApiError::BadRequest)?;
    }

    let graph = state.graph(&request.graph_hash).ok_or_else(|| {
        ApiError::UnknownGraph(format!(
            "no graph stored under `{}`; submit it to POST /v1/graphs first",
            request.graph_hash
        ))
    })?;

    // Resolve every referenced tool and agent synchronously; a missing one is a
    // 404 naming the node, before any run head is written.
    let registry = require_tools(&state, &graph)?;
    let (agents, servers) = build_agents(&state, &graph).await?;

    let run_id = RunId::new();
    // A minted id is fresh by construction, but guard defensively the same way
    // `POST /v1/runs` does before writing the run head.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if !log.is_empty() {
        close_servers(servers).await;
        return Err(ApiError::RunExists(format!(
            "run {} already has recorded history",
            run_id.as_uuid()
        )));
    }

    spawn_graph_drive(
        state,
        run_id,
        graph,
        agents,
        servers,
        registry,
        GraphVerb::Start {
            input: request.input,
            labels: request.labels,
        },
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "run": run_id.as_uuid().to_string(), "status": "running" })),
    ))
}

/// `GET /v1/runs/{id}/graph`: the graph run's per-node projection, for the
/// canvas. `404 unknown_run` when the id has no history; `409 not_a_graph_run`
/// when the run is an ordinary agent run (its log has no `GraphRunStarted`
/// head), mirroring the existing 409 error shape.
pub async fn projection(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if log.is_empty() {
        return Err(ApiError::UnknownRun(format!(
            "no run {} in this store",
            run_id.as_uuid()
        )));
    }
    if !is_graph_run(&log) {
        return Err(ApiError::NotAGraphRun(format!(
            "run {} is an agent run, not a graph run; it has no graph projection",
            run_id.as_uuid()
        )));
    }
    Ok(Json(projection_json(&derive_graph_projection(&log))))
}

/// Drives a parked or crashed GRAPH run further, for
/// [`crate::runs::resume`]'s graph branch. The resume handler has already read
/// the log, classified the run, and validated the input against the recorded
/// suspension schema; this resolves the graph document (by the hash the log
/// records) and its references, then spawns the engine over it. Returns the
/// same `202 driving` body the agent-run resume path returns.
pub async fn drive_resume(
    state: AppState,
    run_id: RunId,
    log: &[EventEnvelope],
    input: Option<Value>,
) -> Result<Response, ApiError> {
    let hash = recorded_graph_hash(log).ok_or_else(|| {
        ApiError::Internal("graph run log has no GraphRunStarted event".to_owned())
    })?;
    let graph = state.graph(&hash).ok_or_else(|| {
        ApiError::UnknownGraph(format!(
            "the graph {hash} this run executes is not stored on this server; submit it, then resume"
        ))
    })?;
    let registry = require_tools(&state, &graph)?;
    let (agents, servers) = build_agents(&state, &graph).await?;

    let verb = match input {
        Some(input) => GraphVerb::Resume(input),
        None => GraphVerb::Recover,
    };
    spawn_graph_drive(state, run_id, graph, agents, servers, registry, verb);
    Ok(driving(run_id).into_response())
}

/// Whether a log is a graph run: its first event is `GraphRunStarted`.
#[must_use]
pub fn is_graph_run(log: &[EventEnvelope]) -> bool {
    matches!(
        log.first().map(|envelope| &envelope.event),
        Some(Event::GraphRunStarted { .. })
    )
}

// ---------------------------------------------------------------------------
// Internals: the driver task, resolution, and JSON shaping.
// ---------------------------------------------------------------------------

/// Spawns the task that drives a graph run to its next resting point over the
/// engine, closing the built agents' MCP sessions afterward. Marks the run
/// active before spawning so a concurrent stream cannot miss it, exactly as the
/// agent-run driver does.
fn spawn_graph_drive(
    state: AppState,
    run_id: RunId,
    graph: Graph,
    agents: HashMap<String, Agent>,
    servers: Vec<McpServer>,
    registry: Arc<ToolRegistry>,
    verb: GraphVerb,
) {
    state.begin_run(run_id);
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        let result = drive_graph(&task_state, run_id, &graph, &agents, &registry, verb).await;
        close_servers(servers).await;
        if let Err(error) = result {
            tracing::error!(run_id = %run_id.as_uuid(), %error, "graph run drive ended with an error");
        }
        task_state.end_run(run_id);
    });
    state.set_handle(run_id, handle);
}

/// Builds the run's [`RunCtx`] and drives the engine over it. A resume seeds the
/// recorded input through [`RunCtx::set_resume_input`] before driving, exactly
/// as `Runtime::resume` does for an agent run; a fresh start stamps the labels.
async fn drive_graph(
    state: &AppState,
    run_id: RunId,
    graph: &Graph,
    agents: &HashMap<String, Agent>,
    registry: &ToolRegistry,
    verb: GraphVerb,
) -> Result<GraphOutcome, salvor_engine::EngineError> {
    let log = state
        .store()
        .read_log(run_id)
        .await
        .map_err(salvor_runtime::RuntimeError::Store)?;
    let mut ctx: RunCtx = state.run_ctx(run_id, log)?;
    let input = match verb {
        GraphVerb::Start { input, labels } => {
            if let Some(labels) = labels {
                ctx = ctx.with_labels(labels);
            }
            input
        }
        GraphVerb::Resume(input) => {
            ctx.set_resume_input(input);
            // The recorded input wins on replay; begin_graph ignores this one.
            Value::Null
        }
        GraphVerb::Recover => Value::Null,
    };
    run_graph(&mut ctx, graph, &input, agents, registry).await
}

/// Builds every agent the graph references (an `agent` node's hash, or a
/// model-decision `branch`'s), keyed by the hash the document declares.
///
/// An unregistered hash is `404 unknown_agent` naming the node; a definition
/// that will not build is `400`. The built agents' MCP sessions ride back in
/// the returned `Vec` so the driver can keep them alive for the run and close
/// them after.
async fn build_agents(
    state: &AppState,
    graph: &Graph,
) -> Result<(HashMap<String, Agent>, Vec<McpServer>), ApiError> {
    let mut agents: HashMap<String, Agent> = HashMap::new();
    let mut servers: Vec<McpServer> = Vec::new();
    for (node_id, hash) in referenced_agents(graph) {
        if agents.contains_key(hash) {
            continue;
        }
        let registered = state.agent(hash).ok_or_else(|| {
            ApiError::UnknownAgent(format!(
                "agent node `{node_id}` references agent `{hash}`, which is not registered on this \
                 server; register its definition, then start the run"
            ))
        })?;
        match state.build_agent(registered.definition).await {
            Ok(BuiltAgent { agent, servers: s }) => {
                agents.insert(hash.to_owned(), agent);
                servers.extend(s);
            }
            Err(message) => {
                close_servers(servers).await;
                return Err(ApiError::BadRequest(format!(
                    "agent node `{node_id}` references agent `{hash}`, which failed to build: \
                     {message}"
                )));
            }
        }
    }
    Ok((agents, servers))
}

/// Checks every `tool` node's tool is present in the server's registry,
/// returning the registry (cloned handle) for the driver to resolve through.
///
/// `503 tool_registry_unavailable` when no registry is wired at all; `404
/// unknown_tool` (naming the node) for a tool the wired registry does not hold.
/// `salvor serve` wires an empty registry, so on a stock server the first
/// `tool` node a graph declares is a clean `unknown_tool`.
fn require_tools(state: &AppState, graph: &Graph) -> Result<Arc<ToolRegistry>, ApiError> {
    let registry = state.tool_registry().ok_or_else(|| {
        ApiError::ToolRegistryUnavailable(
            "this server has no tool registry wired, so it cannot run a graph with `tool` nodes"
                .to_owned(),
        )
    })?;
    for node in &graph.nodes {
        if let Node::Tool(tool) = node
            && registry.get(&tool.tool).is_none()
        {
            return Err(ApiError::UnknownTool(format!(
                "tool node `{}` names tool `{}`, which is not registered on this server",
                tool.id, tool.tool
            )));
        }
    }
    Ok(registry)
}

/// Every agent a graph references, as `(node id, agent hash)` pairs: each
/// `agent` node, plus each model-decision `branch`'s agent.
fn referenced_agents(graph: &Graph) -> Vec<(&str, &str)> {
    graph
        .nodes
        .iter()
        .filter_map(|node| match node {
            Node::Agent(agent) => Some((agent.id.as_str(), agent.agent_hash.as_str())),
            Node::Branch(branch) => branch
                .agent_hash
                .as_deref()
                .map(|hash| (branch.id.as_str(), hash)),
            _ => None,
        })
        .collect()
}

/// The `graph_hash` recorded in a graph run's `GraphRunStarted` head.
fn recorded_graph_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::GraphRunStarted { graph_hash, .. } => Some(graph_hash.clone()),
        _ => None,
    })
}

/// Parses a graph document strictly and runs every validation check, mapping any
/// failure to `400 invalid_graph` with the complete, node/edge-precise error
/// list. A strict parse failure (an unknown field, a missing one, malformed
/// JSON) is reported as a single `invalid_graph` error, since the document could
/// not be typed well enough to run the collect-all checks over it.
fn parse_and_validate(body: &Bytes) -> Result<Graph, ApiError> {
    let graph: Graph = match serde_json::from_slice(body) {
        Ok(graph) => graph,
        Err(error) => {
            return Err(ApiError::InvalidGraph {
                message: "the graph document is not well formed".to_owned(),
                errors: json!([{ "code": "malformed_document", "message": error.to_string() }]),
            });
        }
    };
    match salvor_graph::validate(&graph) {
        Ok(_) => Ok(graph),
        Err(errors) => Err(ApiError::InvalidGraph {
            message: format!(
                "the graph document has {} validation error(s)",
                errors.len()
            ),
            errors: Value::Array(errors.iter().map(graph_error_json).collect()),
        }),
    }
}

/// Renders one [`GraphError`] as a structured JSON object: a stable `code`, the
/// human `message`, and the node or edge it names, so a client can pinpoint the
/// fault without parsing the sentence.
fn graph_error_json(error: &GraphError) -> Value {
    let message = error.to_string();
    match error {
        GraphError::UnsupportedSchemaVersion { found, supported } => json!({
            "code": "unsupported_schema_version", "message": message,
            "found": found, "supported": supported,
        }),
        GraphError::DuplicateNodeId { id } => json!({
            "code": "duplicate_node_id", "message": message, "node": id,
        }),
        GraphError::DanglingEdge {
            from,
            to,
            missing,
            suggestion,
        } => json!({
            "code": "dangling_edge", "message": message,
            "edge": { "from": from, "to": to }, "missing": missing, "suggestion": suggestion,
        }),
        GraphError::DanglingMapBody {
            id,
            missing,
            suggestion,
        } => json!({
            "code": "dangling_map_body", "message": message,
            "node": id, "missing": missing, "suggestion": suggestion,
        }),
        GraphError::MalformedAgentHash { id, hash } => json!({
            "code": "malformed_agent_hash", "message": message, "node": id, "hash": hash,
        }),
        GraphError::NonPositiveConcurrency { id, found } => json!({
            "code": "non_positive_concurrency", "message": message, "node": id, "found": found,
        }),
        GraphError::ApprovalSchemaNotObject { id } => json!({
            "code": "approval_schema_not_object", "message": message, "node": id,
        }),
        GraphError::Cycle { path } => json!({
            "code": "cycle", "message": message, "path": path,
        }),
        GraphError::EdgeTypeMismatch { from, to } => json!({
            "code": "edge_type_mismatch", "message": message, "edge": { "from": from, "to": to },
        }),
        GraphError::InvalidBranchExpression { node, case, error } => json!({
            "code": "invalid_branch_expression", "message": message,
            "node": node, "case": case, "error": error,
        }),
        GraphError::ModelDecisionWithoutAgent { node, case } => json!({
            "code": "model_decision_without_agent", "message": message, "node": node, "case": case,
        }),
    }
}

/// The shape summary as `(key, value)` pairs, for splicing onto a list entry.
fn summary_fields(summary: &GraphSummary) -> Vec<(String, Value)> {
    vec![
        ("node_count".to_owned(), json!(summary.node_count)),
        ("edge_count".to_owned(), json!(summary.edge_count)),
        ("entry_nodes".to_owned(), json!(summary.entry_nodes)),
        ("terminal_nodes".to_owned(), json!(summary.terminal_nodes)),
    ]
}

/// The shape summary as a standalone object, for the validate response.
fn summary_object(summary: &GraphSummary) -> Value {
    Value::Object(summary_fields(summary).into_iter().collect())
}

/// Serializes a [`GraphProjection`] for the canvas, honoring absent-vs-null: a
/// branch case, a map, and the fork origin appear only when recorded, and a
/// node's state carries its skip reason only when it was skipped.
fn projection_json(projection: &GraphProjection) -> Value {
    let nodes: Vec<Value> = projection
        .nodes
        .iter()
        .map(|node| {
            let mut object = serde_json::Map::new();
            object.insert("node".to_owned(), json!(node.node));
            match &node.state {
                NodeState::Entered => {
                    object.insert("state".to_owned(), json!("entered"));
                }
                NodeState::Exited => {
                    object.insert("state".to_owned(), json!("exited"));
                }
                NodeState::Skipped { reason } => {
                    object.insert("state".to_owned(), json!("skipped"));
                    object.insert("reason".to_owned(), json!(reason));
                }
            }
            if let Some(case) = &node.branch_case {
                object.insert("branch_case".to_owned(), json!(case));
            }
            if let Some(map) = &node.map {
                let iterations: Vec<Value> = map
                    .iterations
                    .iter()
                    .map(|it| {
                        json!({ "index": it.index, "child_run": it.child_run, "joined": it.joined })
                    })
                    .collect();
                object.insert(
                    "map".to_owned(),
                    json!({ "items": map.items, "iterations": iterations }),
                );
            }
            Value::Object(object)
        })
        .collect();

    let mut object = serde_json::Map::new();
    if let Some(hash) = &projection.graph_hash {
        object.insert("graph_hash".to_owned(), json!(hash));
    }
    if let Some(origin) = &projection.forked_from {
        object.insert(
            "forked_from".to_owned(),
            json!({
                "run_id": origin.run_id.as_uuid().to_string(),
                "through_seq": origin.through_seq.get(),
                "from_node": origin.from_node,
                "graph_hash": origin.graph_hash,
                "acknowledged_writes": origin.acknowledged_writes,
            }),
        );
    }
    if let Some(current) = &projection.current_node {
        object.insert("current_node".to_owned(), json!(current));
    }
    object.insert("nodes".to_owned(), Value::Array(nodes));
    Value::Object(object)
}

/// The 202 body for a run now driving in the background (identical to the
/// agent-run resume path's).
fn driving(run_id: RunId) -> impl IntoResponse {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "run": run_id.as_uuid().to_string(),
            "status": "running",
            "outcome": "driving",
        })),
    )
}

/// Closes every MCP session, logging (not propagating) a teardown hiccup.
async fn close_servers(servers: Vec<McpServer>) {
    for server in servers {
        if let Err(error) = server.close().await {
            tracing::warn!(%error, "MCP session did not close cleanly");
        }
    }
}

/// Parses a JSON body into `T`, mapping a decode failure to a `400`.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::BadRequest(format!("request body is not valid JSON: {error}")))
}

/// Parses a run id from its UUID string, mapping a bad id to a `400`.
fn parse_run_id(text: &str) -> Result<RunId, ApiError> {
    Uuid::parse_str(text).map(RunId::from_uuid).map_err(|_| {
        ApiError::BadRequest(format!("`{text}` is not a valid run id (expected a UUID)"))
    })
}

/// Maps a store error to a `500`; the store failing is not the client's fault.
fn store_error(error: salvor_store::StoreError) -> ApiError {
    ApiError::Internal(format!("store: {error}"))
}
