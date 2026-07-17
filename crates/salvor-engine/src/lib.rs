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
//! - a **gate**, **branch**, or **map** node is refused with a typed
//!   [`EngineError::UnsupportedNode`] **before** any event for it is recorded,
//!   so the log never grows past the refusal. The document layer still
//!   validates these as legal graphs; only the engine declines to run them yet.
//!
//! After the last node the engine records the single terminal `RunCompleted`.
//! There is no ambient clock or randomness in any decision: everything the
//! engine feeds forward — the walk order, each node's input, an idempotency key
//! — is a pure function of the document or of values the `RunCtx` recorded, so a
//! second drive over the recorded log replays with no live calls and produces a
//! byte-identical log.
//!
//! # Data flow
//!
//! The graph input flows into the first node; each node's output flows into the
//! next along the linear chain. Richer input mapping (a `tool` node's `input`
//! references, a `branch`'s routed value) is not resolved yet; here the upstream
//! output is the downstream input verbatim.
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

use std::collections::HashMap;

use salvor_core::Effect;
use salvor_graph::{Graph, Node};
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
/// [`EngineError::UnsupportedNode`] at a gate/branch/map node (before any event
/// for it is recorded); [`EngineError::UnknownAgent`] / [`EngineError::UnknownTool`]
/// when a resolver cannot supply a node's executable;
/// [`EngineError::MalformedGraph`] when the topology is not a DAG;
/// [`EngineError::ToolFailed`] when a tool call fails; [`EngineError::Runtime`]
/// for any replay divergence, reconciliation refusal, provider, or store error.
pub async fn run_graph(
    ctx: &mut RunCtx,
    graph: &Graph,
    input: &Value,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
) -> Result<GraphOutcome, EngineError> {
    let hash = graph_hash(graph)?;
    // The recorded input always wins on replay; `begin_graph` returns it.
    let mut current = ctx.begin_graph(&hash, input).await?;

    for node in walk::walk_order(graph)? {
        match node {
            Node::Agent(agent_node) => {
                let agent = agents
                    .resolve_agent(&agent_node.agent_hash)
                    .ok_or_else(|| EngineError::UnknownAgent {
                        node: agent_node.id.clone(),
                        agent_hash: agent_node.agent_hash.clone(),
                    })?;
                ctx.node_entered(&agent_node.id).await?;
                // The agent loop runs inside this same log via the runtime's
                // begin/drive_loop split: no second run head, and it returns the
                // output without recording a terminal (the engine owns that).
                match drive_loop(ctx, agent, &current).await? {
                    LoopOutcome::Completed(output) => {
                        ctx.node_exited(&agent_node.id).await?;
                        current = output;
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
                ctx.node_entered(&tool_node.id).await?;
                // An idempotent tool's key derives from recorded randomness, so
                // it reproduces on replay exactly as the built-in loop's does.
                // Read and write tools carry no key (see the built-in loop).
                let idempotency_key = match tool.effect() {
                    Effect::Idempotent => Some(format!("{:016x}", ctx.random().await?)),
                    Effect::Read | Effect::Write => None,
                };
                match ctx
                    .tool_call(tool, &current, idempotency_key.as_deref())
                    .await?
                {
                    ToolCallResult::Output(output) => {
                        ctx.node_exited(&tool_node.id).await?;
                        current = output;
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
                                ctx.node_exited(&tool_node.id).await?;
                                current = resume_input;
                            }
                        }
                    }
                }
            }
            // Gate, branch, and map are legal documents but not yet executable
            // here. Refuse before recording NodeEntered, so nothing lands in the
            // log past this point.
            Node::Gate(_) | Node::Branch(_) | Node::Map(_) => {
                return Err(EngineError::UnsupportedNode {
                    node: node.id().to_owned(),
                    kind: node.kind_name(),
                });
            }
        }
    }

    ctx.complete_run(&current).await?;
    Ok(GraphOutcome::Completed { output: current })
}
