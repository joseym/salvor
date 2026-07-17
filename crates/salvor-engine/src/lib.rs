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
//! - a **map** node is still refused with a typed
//!   [`EngineError::UnsupportedNode`] **before** any event for it is recorded,
//!   so the log never grows past the refusal (fan-out is not implemented yet).
//!
//! After the last node the engine records the single terminal `RunCompleted`.
//! There is no ambient clock or randomness in any decision: everything the
//! engine feeds forward — the walk order, each node's input, the branch route,
//! an idempotency key — is a pure function of the document or of values the
//! `RunCtx` recorded, so a second drive over the recorded log replays with no
//! live calls and produces a byte-identical log.
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
mod walk;

use std::collections::{HashMap, HashSet};

use salvor_core::Effect;
use salvor_graph::expr::Expr;
use salvor_graph::{BranchCondition, BranchNode, Edge, GateNode, Graph, Node};
use salvor_runtime::{
    Agent, LoopOutcome, ParkReason, Resumption, RunCtx, ToolCallResult, drive_loop, hash_value,
};
use salvor_tools::DynTool;
use serde_json::Value;

pub use error::EngineError;

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
/// [`EngineError::UnsupportedNode`] at a map node (before any event for it is
/// recorded); [`EngineError::NoBranchCaseMatched`] when an expression branch
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
                // An idempotent tool's key derives from recorded randomness, so
                // it reproduces on replay exactly as the built-in loop's does.
                // Read and write tools carry no key (see the built-in loop).
                let idempotency_key = match tool.effect() {
                    Effect::Idempotent => Some(format!("{:016x}", ctx.random().await?)),
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
            // Map fan-out is not implemented yet. A reached map is refused before its
            // NodeEntered; an unreached one was already skipped above.
            Node::Map(_) => {
                return Err(EngineError::UnsupportedNode {
                    node: id.to_owned(),
                    kind: node.kind_name(),
                });
            }
        }
    }

    ctx.complete_run(&last_output).await?;
    Ok(GraphOutcome::Completed {
        output: last_output,
    })
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
