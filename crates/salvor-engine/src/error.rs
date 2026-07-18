//! [`EngineError`]: everything that can stop a graph drive.
//!
//! Two families sit here. The first is the engine's own refusals, each naming
//! the offending node: a `map` node whose `over` reference does not resolve to a
//! list ([`EngineError::MapOverNotAList`]) or whose body form is not
//! executable ([`EngineError::UnsupportedMapBody`]), an agent or tool the resolver
//! could not supply, a graph whose topology is not a well-formed DAG, a branch
//! that no case matched or whose model decision named no case, or a tool that
//! failed. Most are returned **before** recording anything for the node they
//! name, so the log never carries events past the refusal; the two branch-decision
//! errors that require running a model first are the documented exception (their
//! `NodeEntered` and the model's events are already recorded when the mapping
//! fails). The second family is [`EngineError::Runtime`], the plain pass-through
//! of a [`RuntimeError`] from the `RunCtx` operations the engine drives.

use salvor_runtime::RuntimeError;
use thiserror::Error;

/// Why a graph drive could not continue.
#[derive(Debug, Error)]
pub enum EngineError {
    /// A `map` node's `over` reference did not resolve to a JSON array against the
    /// routed value (it was missing, or resolved to a non-array value). A map can
    /// only fan out over a list, so the engine refuses deterministically rather
    /// than guessing. Returned **before** the map's `NodeEntered` is recorded, so
    /// nothing lands in the log past the refusal, and it reproduces on replay: the
    /// same recorded routed value re-resolves to the same non-list.
    #[error("map node `{node}`: the `over` reference `{over}` did not resolve to a list")]
    MapOverNotAList {
        /// The id of the map node.
        node: String,
        /// The `over` reference that failed to resolve to a list.
        over: String,
    },

    /// A `map` node's body is a form that is not executable: an embedded
    /// `subgraph` (per-item sub-walks need their own
    /// log per iteration to keep node ids unambiguous, which is not implemented
    /// yet), or a `node` body that
    /// names a node whose kind cannot be a per-item worker (only `agent` and
    /// `tool` bodies run). Returned **before** the map's `NodeEntered` is recorded,
    /// so nothing lands in the log past the refusal. The document layer still
    /// validates these as legal graphs; only the engine declines to run them.
    #[error("map node `{node}`: {detail}")]
    UnsupportedMapBody {
        /// The id of the map node.
        node: String,
        /// What about the body is not supported.
        detail: String,
    },

    /// An expression `branch` reached with no case whose condition evaluated
    /// true. The author declared the cases exhaustively or the graph cannot
    /// proceed; the engine refuses deterministically rather than guessing a
    /// route. Returned before the branch's `NodeEntered` is recorded, so nothing
    /// lands in the log past the refusal, and the refusal reproduces on replay
    /// (the same routed value re-evaluates to the same no-match).
    #[error("branch node `{node}`: no case condition matched the routed value")]
    NoBranchCaseMatched {
        /// The id of the branch node.
        node: String,
    },

    /// A model-decision `branch`'s agent produced a reply that is not one of the
    /// branch's case names. Unlike the other refusals this arrives **after** the
    /// branch's `NodeEntered` and the decision agent's own events are recorded
    /// (the model had to run to produce the reply); it still reproduces on
    /// replay, because the reply is decoded from the recorded model completion.
    #[error(
        "branch node `{node}`: the decision agent replied `{reply}`, which is not one of the cases [{}]",
        .cases.join(", ")
    )]
    BranchDecisionUnmatched {
        /// The id of the branch node.
        node: String,
        /// The agent's reply, trimmed, that named no case.
        reply: String,
        /// The branch's case names, in author order.
        cases: Vec<String>,
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
