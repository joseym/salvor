//! The Salvor graph engine: drives a frozen graph document through its nodes,
//! recording the walk into one durable run log.
//!
//! # Where this crate sits, and why it is its own crate
//!
//! The engine is deliberately **not** part of `salvor-runtime` (that would drag
//! the graph document format into the built-in agent loop) and **not** part of
//! `salvor-graph` (that crate is a pure, IO-free leaf). It sits above both and
//! composes only their public surfaces: the graph document from `salvor-graph`,
//! and the durability substrate ([`RunCtx`](salvor_runtime::RunCtx),
//! [`drive_loop`](salvor_runtime::drive_loop)) from `salvor-runtime`. It reaches
//! into nothing private. That is a deliberate proof of the runtime's API
//! guardrail: everything the engine needs, an outside crate could also do.
//!
//! # What it drives
//!
//! [`run_graph`] opens a run's log with `GraphRunStarted`, walks the nodes in
//! deterministic topological order (see [`walk`]), and drives each one:
//!
//! - an **agent** node runs the built-in agent loop
//!   ([`drive_loop`](salvor_runtime::drive_loop)) inside the same log, framed by
//!   `NodeEntered` / `NodeExited`;
//! - a **tool** node records one tool call through the same write-ahead
//!   intent/completion machinery the built-in loop uses, honoring the tool's
//!   effect class;
//! - a **gate** node parks the run through the exact `Suspended` / `Resumed`
//!   machinery the built-in loop uses for a tool suspension: entering it records
//!   `NodeEntered`, then `suspend` records the gate's `approval_schema` as the
//!   suspension schema and the drive returns [`GraphOutcome::Parked`]. A later
//!   drive over the log (carrying the resume input the existing resume machinery
//!   appended) passes that input through the gate as its output and continues.
//!   A gate needs no event kind of its own;
//! - a **branch** node routes on its input: an expression branch evaluates its
//!   cases in author order and the first true case wins; a model-decision branch
//!   drives the node's agent and maps the reply to a case name. Either way the
//!   chosen case is recorded as `BranchTaken`, the walk follows the like-named
//!   edge, and every node reachable only through a non-taken case is recorded
//!   `NodeSkipped`;
//! - a **map** node fans out over a list. Its `over` reference resolves against
//!   the routed value to a JSON array (a non-array is a typed
//!   [`EngineError::MapOverNotAList`] refused before `NodeEntered`); the engine
//!   records `NodeEntered`, then `MapFannedOut` with the resolved item list, then
//!   walks the list IN INDEX ORDER, and for each element records
//!   `MapIterationStarted` (carrying the derived child-run id), runs the body's
//!   work inline, and records `MapIterationJoined`. The joined output is the
//!   per-element outputs as a list in index order. Iterations run
//!   **inline and sequentially** in the parent's own log: the `concurrency` cap is
//!   accepted (the validator requires it be at least 1) but not honored — a
//!   deliberate v0.4 choice that costs only wall-clock and changes no event shape,
//!   so the whole fan-out is proven by the same single-log replay machinery
//!   already proven for linear and branching graphs. Concurrent child runs are
//!   not yet supported. A
//!   `subgraph` body, or a body node that is not an `agent` or `tool`, is a typed
//!   [`EngineError::UnsupportedMapBody`] refused before `NodeEntered`.
//!
//! A node that is a map's body is executed ONLY as that map's per-item worker; it
//! is never walked independently, so its own events (a tool call, an agent loop)
//! are recorded inline between the map's iteration markers and its node id is
//! never framed with a `NodeEntered` of its own. That keeps node ids unambiguous
//! in the one log and is why forking INTO a map iteration is refused: an iteration
//! is not a node boundary (see [`plan_fork`]).
//!
//! After the last node the engine records the single terminal `RunCompleted`.
//! There is no ambient clock or randomness in any decision: everything the
//! engine feeds forward — the walk order, each node's input, the branch route, a
//! map's resolved item list and its per-iteration child ids, an idempotent tool's
//! idempotency key — is a pure function of the document or of values the `RunCtx`
//! recorded, so a second drive over the recorded log replays with no live calls
//! and produces a byte-identical log. A map iteration's child-run id is
//! `sha256:` over the parent run id, the node id, and the index (see
//! [`map_child_run_id`]) — pure recorded data, so replay reconstructs the
//! identical id without storing anything extra. The idempotency
//! key is derived from the call's position in the graph (graph hash, node id,
//! call index) rather than from drawn randomness, which is what lets a FORK of a
//! run re-walk a segment and present the same key its origin recorded — see
//! [`fork_safe_idempotency_key`] and the `salvor-server` fork endpoint.
//!
//! # Data flow
//!
//! Each node's output flows to its successors along the edges, and a node's
//! input is the recorded output of the live inbound edge that reaches it (the
//! graph input for an entry node with no inbound edge). A branch passes its
//! routed value through unchanged to the taken case's edge; the decision only
//! selects the route, never the data. A tool node's `input` references are still
//! not resolved yet; the upstream output is the downstream input
//! verbatim. When more than one live inbound edge reaches a node, the one whose
//! source id is smallest wins, so the merge is a pure function of the document.
//!
//! # Resolving agents and tools
//!
//! A node names its agent by hash and its tool by name; the engine turns those
//! into executables through the [`AgentResolver`] and [`ToolResolver`] traits
//! the caller supplies. Tests inject maps; the server wires its own
//! registries in separately. Keeping resolution behind a trait is what lets the engine stay
//! ignorant of where agents and tools actually come from.

#![warn(missing_docs)]

mod error;
pub mod fork;
mod walk;

use std::collections::{HashMap, HashSet};

use salvor_core::{Effect, RunId};
use salvor_graph::expr::Expr;
use salvor_graph::{BranchCondition, BranchNode, Edge, GateNode, Graph, MapBody, MapNode, Node};
use salvor_runtime::{
    Agent, LoopOutcome, ParkReason, Resumption, RunCtx, ToolCallResult, drive_loop, hash_value,
};
use salvor_tools::DynTool;
use serde_json::{Value, json};

pub use error::EngineError;
pub use fork::{ForkError, ForkPlan, WriteHazard, plan_fork};

/// Resolves an `agent` node's declared hash to the [`Agent`] that executes it.
///
/// A small trait, not a fixed type, so a test can inject a map while the server
/// injects its agent registry. A [`HashMap<String, Agent>`](std::collections::HashMap)
/// implements it out of the box.
pub trait AgentResolver {
    /// The agent registered under `agent_hash`, or `None` if none is.
    fn resolve_agent(&self, agent_hash: &str) -> Option<&Agent>;
}

/// Resolves a `tool` node's declared name to the [`DynTool`] that executes it.
///
/// The tool counterpart of [`AgentResolver`]. A
/// [`HashMap<String, Box<dyn DynTool>>`](std::collections::HashMap) implements
/// it out of the box.
pub trait ToolResolver {
    /// The tool registered under `name`, or `None` if none is.
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool>;
}

impl AgentResolver for HashMap<String, Agent> {
    fn resolve_agent(&self, agent_hash: &str) -> Option<&Agent> {
        self.get(agent_hash)
    }
}

impl ToolResolver for HashMap<String, Box<dyn DynTool>> {
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool> {
        self.get(name).map(AsRef::as_ref)
    }
}

/// How a graph drive ended.
#[derive(Debug)]
pub enum GraphOutcome {
    /// The graph ran to completion; this is the final output the terminal
    /// `RunCompleted` recorded.
    Completed {
        /// The graph run's final output (the last node's output).
        output: Value,
    },
    /// A node parked the run durably (an agent's budget crossing or a tool
    /// suspension). The run survives restarts; resume it through the runtime's
    /// resume path, then drive the graph again to continue.
    Parked {
        /// The node that parked.
        node: String,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// Computes a graph document's content hash: `sha256:` over its canonical JSON,
/// the exact string recorded in `GraphRunStarted`.
///
/// Reuses `salvor-runtime`'s canonical hashing (the same story behind
/// `agent_def_hash` and `request_hash`), so a graph run's `graph_hash` is
/// reproducible and matches whatever a control plane computes for the same
/// document.
///
/// # Errors
///
/// [`EngineError::GraphEncode`] if the document cannot be serialized (it always
/// can; the edge is kept honest rather than panicking).
pub fn graph_hash(graph: &Graph) -> Result<String, EngineError> {
    let value = serde_json::to_value(graph).map_err(EngineError::GraphEncode)?;
    Ok(hash_value(&value))
}

/// Drives `graph` to completion (or a park) over `ctx`, recording the walk into
/// the run's log.
///
/// The log opens with `GraphRunStarted { graph_hash }`, each node contributes
/// `NodeEntered` … its own events … `NodeExited`, and the run closes with one
/// `RunCompleted`. See the crate docs for the node handling and determinism
/// guarantees. Fresh, recovering, or replaying is entirely the `ctx`'s
/// business: the engine issues the same sequence of `RunCtx` calls either way,
/// which is what makes a second drive over the recorded log a byte-identical,
/// zero-live-call replay.
///
/// # Errors
///
/// [`EngineError::MapOverNotAList`] when a map node's `over` reference does not
/// resolve to a list, and [`EngineError::UnsupportedMapBody`] for a `subgraph` or
/// non-`agent`/`tool` body (both before the map's `NodeEntered` is recorded);
/// [`EngineError::NoBranchCaseMatched`] when an expression branch
/// matches no case (also before its `NodeEntered`);
/// [`EngineError::BranchDecisionUnmatched`] when a model-decision branch's agent
/// names no case (after its `NodeEntered`, since the model had to run);
/// [`EngineError::UnknownAgent`] / [`EngineError::UnknownTool`] when a resolver
/// cannot supply a node's executable; [`EngineError::MalformedGraph`] when the
/// topology is not a DAG (or, unreachable in practice, a branch condition the
/// validator accepted fails to parse here); [`EngineError::ToolFailed`] when a
/// tool call fails; [`EngineError::Runtime`] for any replay divergence,
/// reconciliation refusal, provider, or store error.
pub async fn run_graph(
    ctx: &mut RunCtx,
    graph: &Graph,
    input: &Value,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
) -> Result<GraphOutcome, EngineError> {
    let hash = graph_hash(graph)?;
    // The recorded input always wins on replay; `begin_graph` returns it.
    let graph_input = ctx.begin_graph(&hash, input).await?;

    // Topology and routing state, all keyed on ids that borrow the document.
    let by_id: HashMap<&str, &Node> = graph.nodes.iter().map(|n| (n.id(), n)).collect();
    let mut inbound: HashMap<&str, Vec<&Edge>> = HashMap::new();
    for edge in &graph.edges {
        inbound.entry(edge.to.as_str()).or_default().push(edge);
    }
    // Branch conditions are parsed ONCE here (the validator already guarantees
    // they parse; a failure now is a MalformedGraph unreachable in practice).
    let branches = parse_branches(graph)?;
    // The ids of every node used as a `map` body: they are the per-item workers
    // of their map and are executed ONLY inside its fan-out, never walked
    // independently, so their events stay unambiguous inside the one log.
    let map_body_targets: HashSet<&str> = graph
        .nodes
        .iter()
        .filter_map(|node| match node {
            Node::Map(map) => match &map.body {
                MapBody::Node(target) => Some(target.as_str()),
                MapBody::Subgraph(_) => None,
            },
            _ => None,
        })
        .collect();

    // What each executed node produced, which nodes were skipped, and which case
    // each branch fired: the pure state the routing reads. `last_output` threads
    // the terminal output, seeded with the graph input so an empty graph still
    // completes with it (matching a linear graph with no nodes at all).
    let mut outputs: HashMap<&str, Value> = HashMap::new();
    let mut skipped: HashSet<&str> = HashSet::new();
    let mut branch_case: HashMap<&str, String> = HashMap::new();
    let mut last_output = graph_input.clone();

    for node in walk::walk_order(graph)? {
        let id = node.id();
        // A node used as a map body is not walked independently: it runs only as
        // its map's per-item worker, inline in the fan-out below. Nothing is
        // recorded for it here — it is map-owned, not "skipped".
        if map_body_targets.contains(id) {
            continue;
        }
        // A node with no live inbound edge was routed past: record the skip (its
        // sole marker) and move on. Predecessors are visited first in topological
        // order, so their skip/branch state is already known here.
        let Some(node_input) = select_input(
            id,
            &inbound,
            &by_id,
            &branch_case,
            &skipped,
            &outputs,
            &graph_input,
        ) else {
            ctx.node_skipped(id, SKIP_REASON).await?;
            skipped.insert(id);
            continue;
        };

        match node {
            Node::Agent(agent_node) => {
                let agent = agents
                    .resolve_agent(&agent_node.agent_hash)
                    .ok_or_else(|| EngineError::UnknownAgent {
                        node: agent_node.id.clone(),
                        agent_hash: agent_node.agent_hash.clone(),
                    })?;
                ctx.node_entered(id).await?;
                // The agent loop runs inside this same log via the runtime's
                // begin/drive_loop split: no second run head, and it returns the
                // output without recording a terminal (the engine owns that).
                match drive_loop(ctx, agent, &node_input).await? {
                    LoopOutcome::Completed(output) => {
                        ctx.node_exited(id).await?;
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    LoopOutcome::Parked(reason) => {
                        return Ok(GraphOutcome::Parked {
                            node: agent_node.id.clone(),
                            reason,
                        });
                    }
                }
            }
            Node::Tool(tool_node) => {
                let tool = tools.resolve_tool(&tool_node.tool).ok_or_else(|| {
                    EngineError::UnknownTool {
                        node: tool_node.id.clone(),
                        tool: tool_node.tool.clone(),
                    }
                })?;
                ctx.node_entered(id).await?;
                // An idempotent tool's key is a PURE function of WHERE the call
                // sits in the graph — the graph hash, the node id, the call index
                // within the node — not of drawn randomness. That is what makes it
                // fork-safe: a fork re-walks the segment from its fork node and
                // re-executes the idempotent calls in it live, and this derivation
                // hands each of them the IDENTICAL key its origin recorded, so the
                // provider collapses the duplicate. It also leaves `Effect::Write`
                // as the sole class a fork must have acknowledged. Read and write
                // tools carry no key (see the built-in loop).
                let idempotency_key = match tool.effect() {
                    Effect::Idempotent => Some(fork_safe_idempotency_key(&hash, id, 0)),
                    Effect::Read | Effect::Write => None,
                };
                match ctx
                    .tool_call(tool, &node_input, idempotency_key.as_deref())
                    .await?
                {
                    ToolCallResult::Output(output) => {
                        ctx.node_exited(id).await?;
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    ToolCallResult::Failed(failure) => {
                        return Err(EngineError::ToolFailed {
                            node: tool_node.id.clone(),
                            message: failure.message,
                        });
                    }
                    ToolCallResult::Suspended(suspension) => {
                        ctx.suspend(&suspension.reason, &suspension.input_schema)
                            .await?;
                        match ctx.await_resume().await? {
                            Resumption::Parked => {
                                return Ok(GraphOutcome::Parked {
                                    node: tool_node.id.clone(),
                                    reason: ParkReason::Suspended {
                                        reason: suspension.reason,
                                        input_schema: suspension.input_schema,
                                    },
                                });
                            }
                            Resumption::Resumed(resume_input) => {
                                // The recorded resume input is the tool's answer.
                                ctx.node_exited(id).await?;
                                last_output = resume_input.clone();
                                outputs.insert(id, resume_input);
                            }
                        }
                    }
                }
            }
            // A gate parks through the exact suspension machinery a tool uses:
            // NodeEntered, then `suspend` recording the gate's approval schema,
            // then a park. A later drive over the resumed log passes the resume
            // input through as the gate's output. No gate-specific event kind.
            Node::Gate(gate) => {
                ctx.node_entered(id).await?;
                let reason = gate_reason(gate);
                ctx.suspend(&reason, &gate.approval_schema).await?;
                match ctx.await_resume().await? {
                    Resumption::Parked => {
                        return Ok(GraphOutcome::Parked {
                            node: gate.id.clone(),
                            reason: ParkReason::Suspended {
                                reason,
                                input_schema: gate.approval_schema.clone(),
                            },
                        });
                    }
                    Resumption::Resumed(resume_input) => {
                        ctx.node_exited(id).await?;
                        last_output = resume_input.clone();
                        outputs.insert(id, resume_input);
                    }
                }
            }
            Node::Branch(branch) => {
                // A branch is a pure router: whichever case fires, the routed
                // value passes through unchanged to the taken edge.
                let cases = branches.get(id).expect("every branch node is parsed");
                let chosen: String = match &branch.agent_hash {
                    // Expression branch: choose purely, so a no-match refuses
                    // before NodeEntered and nothing lands past the refusal.
                    None => {
                        let case = choose_expression_case(id, cases, &node_input)?;
                        ctx.node_entered(id).await?;
                        case.to_owned()
                    }
                    // Model-decision branch: the agent must run first, so its
                    // NodeEntered and model events precede the mapping (and the
                    // BranchDecisionUnmatched refusal, if the reply names no case).
                    Some(agent_hash) => {
                        let agent = agents.resolve_agent(agent_hash).ok_or_else(|| {
                            EngineError::UnknownAgent {
                                node: branch.id.clone(),
                                agent_hash: agent_hash.clone(),
                            }
                        })?;
                        ctx.node_entered(id).await?;
                        let reply = match drive_loop(ctx, agent, &node_input).await? {
                            LoopOutcome::Completed(output) => output,
                            LoopOutcome::Parked(reason) => {
                                return Ok(GraphOutcome::Parked {
                                    node: branch.id.clone(),
                                    reason,
                                });
                            }
                        };
                        match_decision(branch, &reply)?.to_owned()
                    }
                };
                ctx.branch_taken(id, &chosen).await?;
                ctx.node_exited(id).await?;
                branch_case.insert(id, chosen);
                last_output = node_input.clone();
                outputs.insert(id, node_input);
            }
            Node::Map(map_node) => {
                match drive_map(ctx, map_node, &node_input, &by_id, agents, tools, &hash).await? {
                    MapOutcome::Joined(output) => {
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    MapOutcome::Parked { node, reason } => {
                        return Ok(GraphOutcome::Parked { node, reason });
                    }
                }
            }
            // A fold's execution semantics are not implemented yet, so the
            // engine refuses it with a typed error BEFORE recording its
            // `NodeEntered`. The fold still validates as a legal
            // document, and its markers and projection exist for client-driven
            // runs to record against; the engine simply does not drive the loop.
            Node::Fold(fold) => {
                return Err(EngineError::UnsupportedNode {
                    node: fold.id.clone(),
                    kind: "fold",
                });
            }
        }
    }

    ctx.complete_run(&last_output).await?;
    Ok(GraphOutcome::Completed {
        output: last_output,
    })
}

/// How driving one map node's fan-out ended.
enum MapOutcome {
    /// Every iteration joined; the map's output is the per-index outputs as a
    /// JSON array in index order.
    Joined(Value),
    /// An iteration parked the run (an `agent` body's budget crossing or a `tool`
    /// body's suspension), propagated up as a graph park at the map node.
    Parked {
        /// The map node that parked.
        node: String,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// Drives one map node's fan-out inline and sequentially into the parent log.
///
/// Refuses an unsupported body form or a non-list `over` **before** recording the
/// map's `NodeEntered`, so nothing lands in the log past such a refusal. Then
/// records `NodeEntered`, `MapFannedOut` with the resolved item list, and for each
/// element in INDEX ORDER records `MapIterationStarted` (with the derived
/// child-run id), runs the body's work inline, and records `MapIterationJoined`.
/// The `concurrency` cap is accepted but not honored: iterations run
/// one after another, which is why the whole fan-out is a plain single-log replay.
async fn drive_map(
    ctx: &mut RunCtx,
    map_node: &MapNode,
    routed: &Value,
    by_id: &HashMap<&str, &Node>,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
    graph_hash: &str,
) -> Result<MapOutcome, EngineError> {
    let node_id = map_node.id.as_str();

    // Resolve the body up front so an unsupported body form refuses BEFORE the
    // map's NodeEntered is recorded.
    let body: &Node = match &map_node.body {
        MapBody::Node(target) => {
            let body_node =
                by_id
                    .get(target.as_str())
                    .copied()
                    .ok_or_else(|| EngineError::MalformedGraph {
                        detail: format!("map node `{node_id}`: body names unknown node `{target}`"),
                    })?;
            match body_node {
                Node::Agent(_) | Node::Tool(_) => body_node,
                other => {
                    return Err(EngineError::UnsupportedMapBody {
                        node: node_id.to_owned(),
                        detail: format!(
                            "a `{}` body node cannot be a per-item worker; only `agent` and `tool` bodies run",
                            other.kind_name()
                        ),
                    });
                }
            }
        }
        MapBody::Subgraph(_) => {
            return Err(EngineError::UnsupportedMapBody {
                node: node_id.to_owned(),
                detail: "an embedded `subgraph` body is not executed yet".to_owned(),
            });
        }
    };

    // Resolve `over` against the routed value; a non-array (including a missing
    // path) is a typed refusal BEFORE NodeEntered.
    let items = resolve_over(node_id, &map_node.over, routed)?;

    ctx.node_entered(node_id).await?;
    ctx.map_fanned_out(node_id, &Value::Array(items.clone()))
        .await?;

    let mut joined: Vec<Value> = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let index = position as u64;
        let child_run = map_child_run_id(ctx.run_id(), node_id, index);
        ctx.map_iteration_started(node_id, index, &child_run)
            .await?;
        let call = MapCall {
            graph_hash,
            node_id,
            index,
        };
        match run_map_body(ctx, body, item, agents, tools, call).await? {
            IterationOutcome::Output(output) => joined.push(output),
            IterationOutcome::Parked(reason) => {
                return Ok(MapOutcome::Parked {
                    node: node_id.to_owned(),
                    reason,
                });
            }
        }
        // Joins are recorded strictly in index order, never completion order.
        ctx.map_iteration_joined(node_id, index).await?;
    }
    ctx.node_exited(node_id).await?;
    Ok(MapOutcome::Joined(Value::Array(joined)))
}

/// How one map iteration's body work ended.
enum IterationOutcome {
    /// The body produced this output for the iteration.
    Output(Value),
    /// The body parked (an agent budget crossing or a tool suspension).
    Parked(ParkReason),
}

/// Where one map iteration's tool call sits, for deriving its fork-safe
/// idempotency key: the graph hash, the map node id, and the iteration index.
struct MapCall<'a> {
    /// The graph document hash.
    graph_hash: &'a str,
    /// The map node id (the "node" this iteration is a call of).
    node_id: &'a str,
    /// The zero-based iteration index (the call index within the map node).
    index: u64,
}

/// Runs one map iteration's body work inline: the referenced `agent` or `tool`
/// node's work with `item` as its input, recorded in the parent log WITHOUT a
/// `NodeEntered` frame of its own (the iteration markers bracket it instead). The
/// body kind was already validated as `agent` or `tool` by [`drive_map`].
async fn run_map_body(
    ctx: &mut RunCtx,
    body: &Node,
    item: &Value,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
    call: MapCall<'_>,
) -> Result<IterationOutcome, EngineError> {
    match body {
        Node::Agent(agent_node) => {
            let agent = agents
                .resolve_agent(&agent_node.agent_hash)
                .ok_or_else(|| EngineError::UnknownAgent {
                    node: agent_node.id.clone(),
                    agent_hash: agent_node.agent_hash.clone(),
                })?;
            match drive_loop(ctx, agent, item).await? {
                LoopOutcome::Completed(output) => Ok(IterationOutcome::Output(output)),
                LoopOutcome::Parked(reason) => Ok(IterationOutcome::Parked(reason)),
            }
        }
        Node::Tool(tool_node) => {
            let tool =
                tools
                    .resolve_tool(&tool_node.tool)
                    .ok_or_else(|| EngineError::UnknownTool {
                        node: tool_node.id.clone(),
                        tool: tool_node.tool.clone(),
                    })?;
            // Each iteration is a distinct call of the MAP node, so its idempotent
            // key is derived from the map node id and the index — the "several
            // calls within one node" case `fork_safe_idempotency_key`'s call-index
            // parameter exists for. A fork re-walking the fan-out presents each
            // iteration's identical key; Read/Write carry none.
            let idempotency_key = match tool.effect() {
                Effect::Idempotent => Some(fork_safe_idempotency_key(
                    call.graph_hash,
                    call.node_id,
                    call.index,
                )),
                Effect::Read | Effect::Write => None,
            };
            match ctx
                .tool_call(tool, item, idempotency_key.as_deref())
                .await?
            {
                ToolCallResult::Output(output) => Ok(IterationOutcome::Output(output)),
                ToolCallResult::Failed(failure) => Err(EngineError::ToolFailed {
                    node: tool_node.id.clone(),
                    message: failure.message,
                }),
                ToolCallResult::Suspended(suspension) => {
                    ctx.suspend(&suspension.reason, &suspension.input_schema)
                        .await?;
                    match ctx.await_resume().await? {
                        Resumption::Parked => Ok(IterationOutcome::Parked(ParkReason::Suspended {
                            reason: suspension.reason,
                            input_schema: suspension.input_schema,
                        })),
                        Resumption::Resumed(resume_input) => {
                            Ok(IterationOutcome::Output(resume_input))
                        }
                    }
                }
            }
        }
        // `drive_map` validated the body kind as agent or tool before calling here.
        other => Err(EngineError::UnsupportedMapBody {
            node: call.node_id.to_owned(),
            detail: format!(
                "a `{}` body node cannot be a per-item worker",
                other.kind_name()
            ),
        }),
    }
}

/// The derived id of the child run executing one map iteration: `sha256:` over the
/// parent run id, the map node id, and the zero-based index.
///
/// A pure function of recorded data (the parent run id the `RunCtx` carries, the
/// document's node id, and the index), so a replay of the parent reconstructs the
/// identical id without storing anything extra, and it is stable across processes
/// and languages. Reuses `salvor-runtime`'s canonical hashing, the same story
/// behind [`graph_hash`] and [`fork_safe_idempotency_key`]. Iterations run
/// inline in the parent log today, so no separate log exists under this id
/// yet; it is recorded on `MapIterationStarted` as the durable, forward-compatible
/// identity a future concurrent-child-run would key each iteration's own log
/// on.
fn map_child_run_id(parent_run: RunId, node_id: &str, index: u64) -> String {
    hash_value(&json!({
        "parent_run": parent_run,
        "node": node_id,
        "index": index,
    }))
}

/// Resolves a map node's `over` reference against the routed value to the list of
/// items to fan out over.
///
/// The reference uses the same path grammar and missing-path semantics the branch
/// expressions use (see [`salvor_graph::expr::parse_reference`]). A reference that
/// fails to parse is a [`EngineError::MalformedGraph`] that does not arise for a
/// validated document; one that resolves to anything but a JSON array — including
/// a missing path — is a typed [`EngineError::MapOverNotAList`].
fn resolve_over(node_id: &str, over: &str, routed: &Value) -> Result<Vec<Value>, EngineError> {
    let reference =
        salvor_graph::expr::parse_reference(over).map_err(|error| EngineError::MalformedGraph {
            detail: format!(
                "map node `{node_id}`: `over` reference `{over}` is unparseable: {error}"
            ),
        })?;
    match reference.resolve(routed) {
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err(EngineError::MapOverNotAList {
            node: node_id.to_owned(),
            over: over.to_owned(),
        }),
    }
}

/// The idempotency key a graph `tool` node's [`Effect::Idempotent`] call
/// presents: a pure function of the call's POSITION in the graph — the graph
/// hash, the node id, and the call index within the node — never of drawn
/// randomness.
///
/// This is what makes an idempotent tool fork-safe. A fork re-walks the segment
/// from its fork node, re-executing the idempotent calls in it live; deriving
/// the key from position means each re-executed call presents the IDENTICAL key
/// its origin recorded, so the provider collapses the duplicate. Drawing the key
/// from [`RunCtx::random`](salvor_runtime::RunCtx::random) instead would mint a
/// fresh key in the fork, and the provider would see a second, distinct call.
/// With this derivation, [`Effect::Write`] is the only effect class a fork must
/// have acknowledged, because a [`Effect::Read`] re-executes freely and a
/// [`Effect::Idempotent`] retry collapses.
///
/// `call_index` is the zero-based position of the call within the node. A `tool`
/// node makes exactly one call, so it is always `0` today; the parameter is
/// carried so a future node kind issuing several calls keeps their keys distinct.
///
/// Reuses `salvor-runtime`'s canonical hashing (the same behind `graph_hash`
/// itself), so the key is reproducible and stable across processes and
/// languages. Existing recorded logs are not disturbed: a key is plain data in
/// the log, and replay correlates on the RECORDED request, never on a
/// re-derivation, so a log whose idempotent call recorded a random-drawn key
/// still replays byte for byte under this build.
fn fork_safe_idempotency_key(graph_hash: &str, node_id: &str, call_index: u64) -> String {
    hash_value(&serde_json::json!({
        "graph_hash": graph_hash,
        "node": node_id,
        "call": call_index,
    }))
}

/// The reason recorded for every [`salvor_core::Event::NodeSkipped`]: a constant,
/// so it is trivially a pure function of the run and reproduces byte for byte on
/// replay (the cursor matches the recorded reason).
const SKIP_REASON: &str = "no live inbound edge: an upstream branch routed to another case";

/// Every branch node's cases, parsed once at load: the branch node id maps to
/// its cases as `(case name, optional parsed expression)` pairs, where the
/// expression is `None` for a `model_decision` case. All ids and names borrow
/// the graph document.
type ParsedBranches<'a> = HashMap<&'a str, Vec<(&'a str, Option<Expr>)>>;

/// Parses every branch node's case conditions once, up front. An `expression`
/// case parses to an [`Expr`]; a `model_decision` case has no expression, so it
/// stores `None`. The validator already guarantees each expression parses, so a
/// failure here is a [`EngineError::MalformedGraph`] that does not arise for a
/// validated document.
fn parse_branches(graph: &Graph) -> Result<ParsedBranches<'_>, EngineError> {
    let mut parsed = HashMap::new();
    for node in &graph.nodes {
        let Node::Branch(branch) = node else {
            continue;
        };
        let mut cases = Vec::with_capacity(branch.cases.len());
        for case in &branch.cases {
            let expr = match &case.when {
                BranchCondition::Expression(source) => {
                    Some(salvor_graph::expr::parse(source).map_err(|error| {
                        EngineError::MalformedGraph {
                            detail: format!(
                                "branch node `{}`: case `{}` has an unparseable condition: {error}",
                                branch.id, case.name
                            ),
                        }
                    })?)
                }
                BranchCondition::ModelDecision => None,
            };
            cases.push((case.name.as_str(), expr));
        }
        parsed.insert(branch.id.as_str(), cases);
    }
    Ok(parsed)
}

/// The input a node receives: the recorded output of its live inbound edge, or
/// the graph input for an entry node (no inbound edge). Returns `None` when no
/// inbound edge is live, which means the node was routed past and must be
/// skipped.
///
/// An inbound edge is live when its source ran (was not skipped) and, if the
/// source is a branch, the edge realizes the case that fired. Among several live
/// inbound edges the smallest source id wins, so a merge is a pure function of
/// the document.
fn select_input(
    id: &str,
    inbound: &HashMap<&str, Vec<&Edge>>,
    by_id: &HashMap<&str, &Node>,
    branch_case: &HashMap<&str, String>,
    skipped: &HashSet<&str>,
    outputs: &HashMap<&str, Value>,
    graph_input: &Value,
) -> Option<Value> {
    let edges = inbound.get(id).map(Vec::as_slice).unwrap_or_default();
    if edges.is_empty() {
        return Some(graph_input.clone());
    }
    let mut chosen: Option<&Edge> = None;
    for edge in edges {
        if !is_live_inbound(edge, by_id, branch_case, skipped) {
            continue;
        }
        chosen = match chosen {
            Some(best) if best.from <= edge.from => Some(best),
            _ => Some(edge),
        };
    }
    chosen.map(|edge| {
        outputs
            .get(edge.from.as_str())
            .cloned()
            .unwrap_or(Value::Null)
    })
}

/// Whether an inbound edge carries a live value into its destination: the source
/// ran, and if the source is a branch the edge's label names the fired case.
fn is_live_inbound(
    edge: &Edge,
    by_id: &HashMap<&str, &Node>,
    branch_case: &HashMap<&str, String>,
    skipped: &HashSet<&str>,
) -> bool {
    if skipped.contains(edge.from.as_str()) {
        return false;
    }
    match by_id.get(edge.from.as_str()) {
        // A branch only lets the edge realizing its fired case through.
        Some(Node::Branch(_)) => {
            branch_case.get(edge.from.as_str()).map(String::as_str) == edge.label.as_deref()
        }
        // Every non-branch source feeds all of its outbound edges.
        _ => true,
    }
}

/// The human-readable suspension reason a gate parks under: its prompt when it
/// has one, else a phrase derived from the node id. A pure function of the
/// document, so it reproduces on replay.
fn gate_reason(gate: &GateNode) -> String {
    gate.prompt
        .clone()
        .unwrap_or_else(|| format!("approval required at gate `{}`", gate.id))
}

/// Picks the first expression case whose condition is true, in author order.
/// Returns [`EngineError::NoBranchCaseMatched`] when none fires. A
/// `model_decision` case reaching here means an expression branch (no
/// `agent_hash`) carried one, which the validator rejects, so it is a
/// [`EngineError::MalformedGraph`] unreachable for a validated document.
fn choose_expression_case<'a>(
    node_id: &str,
    cases: &'a [(&'a str, Option<Expr>)],
    value: &Value,
) -> Result<&'a str, EngineError> {
    for (name, expr) in cases {
        match expr {
            Some(expr) if expr.eval(value) => return Ok(name),
            Some(_) => {}
            None => {
                return Err(EngineError::MalformedGraph {
                    detail: format!(
                        "branch node `{node_id}`: an expression branch must not carry a model-decision case"
                    ),
                });
            }
        }
    }
    Err(EngineError::NoBranchCaseMatched {
        node: node_id.to_owned(),
    })
}

/// Maps a decision agent's reply to a case name: the reply's final text,
/// trimmed, must exactly equal one of the branch's case names. Anything else is
/// [`EngineError::BranchDecisionUnmatched`], listing the case names.
fn match_decision<'a>(branch: &'a BranchNode, reply: &Value) -> Result<&'a str, EngineError> {
    let reply_text = reply
        .as_str()
        .map_or_else(|| reply.to_string(), |text| text.trim().to_owned());
    for case in &branch.cases {
        if case.name == reply_text {
            return Ok(case.name.as_str());
        }
    }
    Err(EngineError::BranchDecisionUnmatched {
        node: branch.id.clone(),
        reply: reply_text,
        cases: branch.cases.iter().map(|case| case.name.clone()).collect(),
    })
}
