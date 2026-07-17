//! [`EngineError`]: everything that can stop a graph drive.
//!
//! Two families sit here. The first is the engine's own refusals, each naming
//! the offending node: a node kind the engine does not execute yet
//! ([`EngineError::UnsupportedNode`]), an agent or tool the resolver could not
//! supply, a graph whose topology is not a well-formed DAG, or a tool that
//! failed. These are typed, not panics, and the engine returns them **before**
//! recording anything for the node they name, so the log never carries events
//! past a refusal. The second is [`EngineError::Runtime`], the plain pass-through
//! of a [`RuntimeError`] from the `RunCtx` operations the engine drives.

use salvor_runtime::RuntimeError;
use thiserror::Error;

/// Why a graph drive could not continue.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The walk reached a node whose kind the engine does not execute yet
    /// (a `gate`, `branch`, or `map`). Returned before any event for the node
    /// is recorded, so nothing lands in the log past the refusal. The document
    /// layer still validates these as legal graphs; only the engine declines to
    /// run them for now.
    #[error("node `{node}`: the engine does not support `{kind}` nodes yet")]
    UnsupportedNode {
        /// The id of the node that could not be executed.
        node: String,
        /// Its kind name (`gate`, `branch`, or `map`).
        kind: &'static str,
    },

    /// An `agent` node referenced an agent hash the resolver could not supply.
    #[error("agent node `{node}`: no agent registered for hash `{agent_hash}`")]
    UnknownAgent {
        /// The id of the agent node.
        node: String,
        /// The unresolved agent definition hash.
        agent_hash: String,
    },

    /// A `tool` node named a tool the resolver could not supply.
    #[error("tool node `{node}`: no tool registered under the name `{tool}`")]
    UnknownTool {
        /// The id of the tool node.
        node: String,
        /// The unresolved tool name.
        tool: String,
    },

    /// The graph's edges do not form a well-formed DAG (a cycle, or an edge
    /// referencing a node that is not in the document). The document validator
    /// rejects both at submit; the engine re-checks defensively so a walk is
    /// never attempted over a malformed topology.
    #[error("the graph is not a well-formed acyclic document: {detail}")]
    MalformedGraph {
        /// What was wrong with the topology.
        detail: String,
    },

    /// A `tool` node's call failed after exhausting its retry policy. The full
    /// failure is already recorded in the log's `ToolCallCompleted`; this
    /// carries the message so the caller sees why the graph stopped.
    #[error("tool node `{node}` failed: {message}")]
    ToolFailed {
        /// The id of the tool node that failed.
        node: String,
        /// The recorded failure message.
        message: String,
    },

    /// The graph document could not be serialized to compute its hash. A graph
    /// is plain data, so this does not arise in practice; it exists to keep the
    /// hashing edge honest rather than panicking on a `serde_json` error.
    #[error("could not serialize the graph document to hash it: {0}")]
    GraphEncode(#[source] serde_json::Error),

    /// A `RunCtx` operation surfaced a runtime error (replay divergence, a
    /// dangling write needing reconciliation, a live provider failure, a store
    /// failure). Passed through unchanged.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}
