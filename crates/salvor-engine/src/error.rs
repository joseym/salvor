//! [`EngineError`]: everything that can stop a graph drive.
//!
//! Two families sit here. The first is the engine's own refusals, each naming
//! the offending node: a `map` node whose `over` reference does not resolve to a
//! list ([`EngineError::MapOverNotAList`]) or whose body form is not
//! executable ([`EngineError::UnsupportedMapBody`]), a `fold` node whose body
//! form is not executable ([`EngineError::UnsupportedFoldBody`]) or whose
//! `best_by` join finds no comparable candidate
//! ([`EngineError::FoldNoComparableCandidate`]) or that reached its iteration
//! bound while declaring `on_bound: fail`
//! ([`EngineError::FoldBoundExceeded`]), an agent or tool the resolver
//! could not supply, a graph whose topology is not a well-formed DAG, a branch
//! that no case matched or whose model decision named no case, a tool that
//! failed, or a gate resumed with an approval that does not satisfy its
//! `approval_schema` ([`EngineError::ApprovalSchemaViolation`]). Most are
//! returned **before** recording anything for the node they
//! name, so the log never carries events past the refusal; the two branch-decision
//! errors that require running a model first are the documented exception (their
//! `NodeEntered` and the model's events are already recorded when the mapping
//! fails), as are the fold's two post-loop refusals, which can only be reached
//! once the passes they judge have run. There is no longer a whole-kind refusal:
//! every node kind the document defines is executed, so the variant that named
//! one is gone. The second family is [`EngineError::Runtime`], the plain
//! pass-through of a [`RuntimeError`] from the `RunCtx` operations the engine
//! drives.
//!
//! # Permanent and transient, and why the split is here
//!
//! Every variant answers [`EngineError::is_permanent`]. A **permanent** refusal
//! is a pure function of the frozen graph document and the recorded log: drive
//! the same run again and it re-fails identically, forever, with no live call
//! able to change the answer. A **transient** one depends on something outside
//! that pair (how the process was invoked, what was registered, what a provider
//! or the store did on this attempt), so a retry, or the same retry with
//! different flags, can succeed.
//!
//! The split exists so a graph driver can tell a run that is *stuck* from a run
//! that is *dead*. A dead run must stop reading as `running`: the drivers record
//! a terminal `RunFailed` for a permanent refusal (see
//! [`crate::record_permanent_refusal`]) and leave a transient one exactly as it
//! was, recoverable. Getting that backwards in the safe direction costs an
//! operator a re-drive; getting it backwards in the unsafe direction kills a run
//! that would have recovered. So the rule when a variant is genuinely arguable
//! is **transient**. [`EngineError::is_permanent`]'s doc comment defends every
//! variant's call, one line each, in one place so the whole table can be read
//! (and argued with) at once.

use crate::approval::ApprovalViolation;
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

    /// A `fold` node's body is a form that is not executable: an embedded
    /// `subgraph` (per-pass sub-walks need their own log per pass to keep node
    /// ids unambiguous, which is not implemented yet, exactly as for a map), or
    /// a `node` body that names a node whose kind cannot be a per-pass worker
    /// (only `agent` and `tool` bodies run). Returned **before** the fold's
    /// `NodeEntered` is recorded, so nothing lands in the log past the refusal.
    /// The document layer still validates these as legal graphs; only the engine
    /// declines to run them.
    #[error("fold node `{node}`: {detail}")]
    UnsupportedFoldBody {
        /// The id of the fold node.
        node: String,
        /// What about the body is not supported.
        detail: String,
    },

    /// A `fold` node's `best_by` join found no pass it could choose between:
    /// the reference resolved on no pass, or resolved only to values the
    /// expression language does not order (anything but a number or a string).
    /// An argmax with no candidate has no answer, so the engine refuses rather
    /// than falling back to a pass no rule chose. Unlike the body refusal this
    /// arrives **after** the fold's `NodeEntered` and its passes are recorded
    /// (the passes had to run to be chosen among), but **before**
    /// `FoldConverged`: no winner and no reason land in the log for a
    /// convergence that did not happen. It reproduces on replay, because the
    /// argmax reads the recorded pass outputs.
    #[error(
        "fold node `{node}`: the `best_by` join reference `{reference}` named no comparable value in any pass"
    )]
    FoldNoComparableCandidate {
        /// The id of the fold node.
        node: String,
        /// The `best_by` reference that named nothing comparable.
        reference: String,
    },

    /// A `fold` node that declares `on_bound: fail` ran every pass its
    /// `max_iterations` bound allows and `stop_when` never held. For such a
    /// fold the predicate is a REQUIREMENT rather than an early exit: the loop
    /// converged on nothing, so the node produces no value and the join is
    /// never consulted.
    ///
    /// Recorded state at the refusal: the passes and their joins are all in the
    /// log, because they really happened and a replay must reproduce them. What
    /// does not land is the convergence: this is returned exactly where
    /// `FoldConverged` would have been recorded, so no `FoldConverged` and no
    /// `NodeExited` are written, mirroring
    /// [`EngineError::FoldNoComparableCandidate`]. It reproduces on replay,
    /// because the pass count and the predicate's verdict are both pure
    /// functions of the recorded pass outputs.
    #[error(
        "fold node `{node}`: reached the max_iterations bound of {bound} without `stop_when` holding, and this fold declares `on_bound: fail`"
    )]
    FoldBoundExceeded {
        /// The id of the fold node.
        node: String,
        /// The `max_iterations` bound the loop reached.
        bound: u32,
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

    /// A `gate` node was resumed with an input that does not satisfy the gate's
    /// declared `approval_schema`. Returned from the **accept edge**: after the
    /// gate's `Suspended` has been replayed and BEFORE `await_resume` can
    /// append a `Resumed`, so the refusal appends nothing and leaves the run
    /// parked exactly where it was, ready for a conforming approval. It is
    /// therefore not reachable on replay at all: a recorded `Resumed` is
    /// history and is fed to the gate untouched. See [`crate::approval`].
    #[error(
        "gate node `{node}`: the approval input does not satisfy the gate's approval_schema ({})",
        .violations.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
    )]
    ApprovalSchemaViolation {
        /// The id of the gate node the run is parked at.
        node: String,
        /// Every way the input failed the schema, in a stable order.
        violations: Vec<ApprovalViolation>,
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

impl EngineError {
    /// Whether this refusal is PERMANENT: a pure function of the frozen graph
    /// document and the recorded log, so the same drive re-fails identically
    /// forever and no retry, registration, or live call can change the answer.
    /// `false` means TRANSIENT: the refusal depends on the environment, on how
    /// the drive was invoked, or on a live call, so a retry (possibly with
    /// different flags) can succeed.
    ///
    /// A graph driver records a terminal `RunFailed` for a permanent refusal so
    /// a dead run stops reading as `running`, and leaves a transient one
    /// recoverable exactly as it was. Because a wrong `true` kills a run that
    /// would have come back and a wrong `false` costs only an operator's
    /// re-drive, an arguable variant is classified TRANSIENT.
    ///
    /// The match is exhaustive with no wildcard arm on purpose: a new variant
    /// does not compile until someone decides which side it falls on.
    ///
    /// # The table
    ///
    /// PERMANENT:
    ///
    /// - [`MapOverNotAList`](Self::MapOverNotAList): the `over` reference and
    ///   the routed value are both recorded, so the resolve re-runs to the same
    ///   non-list every time.
    /// - [`UnsupportedMapBody`](Self::UnsupportedMapBody) /
    ///   [`UnsupportedFoldBody`](Self::UnsupportedFoldBody): the body form is a
    ///   field of the frozen document. No retry makes a `subgraph` body run;
    ///   only a NEW document (a new run) does.
    /// - [`NoBranchCaseMatched`](Self::NoBranchCaseMatched): the cases are the
    ///   document's and the routed value is recorded, so the same no-match
    ///   reproduces exactly.
    /// - [`BranchDecisionUnmatched`](Self::BranchDecisionUnmatched): the reply
    ///   is decoded from a RECORDED model completion, never re-requested, so
    ///   the mapping re-fails on replay. The model is not asked again, which is
    ///   what separates this from a live provider failure.
    /// - [`FoldNoComparableCandidate`](Self::FoldNoComparableCandidate): the
    ///   argmax reads the recorded pass outputs; nothing in the log can become
    ///   comparable later.
    /// - [`FoldBoundExceeded`](Self::FoldBoundExceeded): the bound and
    ///   `on_bound` are the document's, and the passes that failed the
    ///   predicate are recorded. The loop cannot be given more passes without
    ///   changing the document.
    /// - [`MalformedGraph`](Self::MalformedGraph): a property of the document
    ///   alone (a cycle, a dangling body reference, a bound below one). The
    ///   supplied document is pinned to the run by the recorded `graph_hash`,
    ///   so "supply a fixed one" is not a retry of THIS run; it is a new run.
    ///
    /// TRANSIENT:
    ///
    /// - [`UnknownAgent`](Self::UnknownAgent) /
    ///   [`UnknownTool`](Self::UnknownTool): the document names a hash or a
    ///   name; whether it RESOLVES is a fact about this invocation's resolvers,
    ///   which is registration, not meaning. Registering the agent on the
    ///   server, or passing the missing `--agent` file, makes the same log
    ///   drive on. Killing the run for a forgotten flag would be the exact
    ///   mistake this split exists to avoid.
    /// - [`ToolFailed`](Self::ToolFailed): a live call failed after its retry
    ///   policy. The tool is the outside world; a resume can reach a world that
    ///   answers. (A recorded failure does replay, but re-driving is the
    ///   operator's decision to make, not the engine's to foreclose.)
    /// - [`ApprovalSchemaViolation`](Self::ApprovalSchemaViolation): NOT
    ///   permanent, and the clearest case of it. The refusal is about an input
    ///   that has not been recorded and never will be; the run is still parked
    ///   at its gate, and a conforming approval can arrive at any moment. This
    ///   variant is not even reachable on replay.
    /// - [`GraphEncode`](Self::GraphEncode): arguable, so transient. It is a
    ///   serializer edge rather than a statement about the document's meaning,
    ///   and it is raised before `begin_graph` writes the run head, so there is
    ///   no run for a terminal to belong to. Classifying it permanent would
    ///   invite appending `RunFailed` onto a log with no `GraphRunStarted`.
    /// - [`Runtime`](Self::Runtime): everything the `RunCtx` surfaces (a store
    ///   failure, a provider failure, a replay divergence, a dangling write
    ///   needing reconciliation). Store and provider failures are plainly
    ///   retryable; a divergence or a reconciliation refusal is the operator's
    ///   to resolve, and `resolve` exists precisely so such a run continues.
    ///   None of it is the engine's to declare dead.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::MapOverNotAList { .. }
            | Self::UnsupportedMapBody { .. }
            | Self::NoBranchCaseMatched { .. }
            | Self::BranchDecisionUnmatched { .. }
            | Self::UnsupportedFoldBody { .. }
            | Self::FoldNoComparableCandidate { .. }
            | Self::FoldBoundExceeded { .. }
            | Self::MalformedGraph { .. } => true,
            Self::UnknownAgent { .. }
            | Self::UnknownTool { .. }
            | Self::ToolFailed { .. }
            | Self::ApprovalSchemaViolation { .. }
            | Self::GraphEncode(_)
            | Self::Runtime(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A short name per variant, written as an EXHAUSTIVE match with no
    /// wildcard arm. This is the forcing function the classification test rests
    /// on: a variant added to [`EngineError`] does not compile until it is
    /// named here, and naming it here is not enough until [`samples`] carries
    /// one and states which side of the split it falls on.
    fn variant_name(error: &EngineError) -> &'static str {
        match error {
            EngineError::MapOverNotAList { .. } => "MapOverNotAList",
            EngineError::UnsupportedMapBody { .. } => "UnsupportedMapBody",
            EngineError::NoBranchCaseMatched { .. } => "NoBranchCaseMatched",
            EngineError::BranchDecisionUnmatched { .. } => "BranchDecisionUnmatched",
            EngineError::UnsupportedFoldBody { .. } => "UnsupportedFoldBody",
            EngineError::FoldNoComparableCandidate { .. } => "FoldNoComparableCandidate",
            EngineError::FoldBoundExceeded { .. } => "FoldBoundExceeded",
            EngineError::UnknownAgent { .. } => "UnknownAgent",
            EngineError::UnknownTool { .. } => "UnknownTool",
            EngineError::MalformedGraph { .. } => "MalformedGraph",
            EngineError::ToolFailed { .. } => "ToolFailed",
            EngineError::ApprovalSchemaViolation { .. } => "ApprovalSchemaViolation",
            EngineError::GraphEncode(_) => "GraphEncode",
            EngineError::Runtime(_) => "Runtime",
        }
    }

    /// One of every variant, paired with the answer
    /// [`EngineError::is_permanent`] must give it. This is the table the
    /// method's doc comment argues, written out independently so a change to
    /// the method that is not also a change to the argument fails here.
    fn samples() -> Vec<(EngineError, bool)> {
        vec![
            (
                EngineError::MapOverNotAList {
                    node: "fanout".to_owned(),
                    over: "roster".to_owned(),
                },
                true,
            ),
            (
                EngineError::UnsupportedMapBody {
                    node: "fanout".to_owned(),
                    detail: "a `subgraph` body is not executed yet".to_owned(),
                },
                true,
            ),
            (
                EngineError::NoBranchCaseMatched {
                    node: "route".to_owned(),
                },
                true,
            ),
            (
                EngineError::BranchDecisionUnmatched {
                    node: "route".to_owned(),
                    reply: "maybe".to_owned(),
                    cases: vec!["yes".to_owned(), "no".to_owned()],
                },
                true,
            ),
            (
                EngineError::UnsupportedFoldBody {
                    node: "refine".to_owned(),
                    detail: "a `gate` body node cannot be a per-pass worker".to_owned(),
                },
                true,
            ),
            (
                EngineError::FoldNoComparableCandidate {
                    node: "refine".to_owned(),
                    reference: "score".to_owned(),
                },
                true,
            ),
            (
                EngineError::FoldBoundExceeded {
                    node: "refine".to_owned(),
                    bound: 3,
                },
                true,
            ),
            (
                EngineError::MalformedGraph {
                    detail: "the edges form a cycle".to_owned(),
                },
                true,
            ),
            (
                EngineError::UnknownAgent {
                    node: "research".to_owned(),
                    agent_hash: "sha256:0".to_owned(),
                },
                false,
            ),
            (
                EngineError::UnknownTool {
                    node: "publish".to_owned(),
                    tool: "publish_post".to_owned(),
                },
                false,
            ),
            (
                EngineError::ToolFailed {
                    node: "publish".to_owned(),
                    message: "publish endpoint unreachable".to_owned(),
                },
                false,
            ),
            (
                EngineError::ApprovalSchemaViolation {
                    node: "approve".to_owned(),
                    violations: vec![ApprovalViolation {
                        path: "$.approved".to_owned(),
                        message: "is a required property".to_owned(),
                    }],
                },
                false,
            ),
            (
                EngineError::GraphEncode(
                    serde_json::from_str::<serde_json::Value>("{").expect_err("malformed JSON"),
                ),
                false,
            ),
            (
                EngineError::Runtime(RuntimeError::ResumeInputRejected(
                    "the store is unavailable".to_owned(),
                )),
                false,
            ),
        ]
    }

    /// Every variant answers `is_permanent` with the value its doc comment
    /// defends, and the sample table covers every variant there is: a new
    /// variant fails `variant_name`'s exhaustive match at compile time, and a
    /// variant named there but left out of the table fails here.
    #[test]
    fn every_engine_error_variant_is_classified_permanent_or_transient() {
        for (error, permanent) in samples() {
            assert_eq!(
                error.is_permanent(),
                permanent,
                "{}: classified against its documented side ({error})",
                variant_name(&error)
            );
        }

        let covered: BTreeSet<&'static str> = samples()
            .iter()
            .map(|(error, _)| variant_name(error))
            .collect();
        let expected: BTreeSet<&'static str> = [
            "MapOverNotAList",
            "UnsupportedMapBody",
            "NoBranchCaseMatched",
            "BranchDecisionUnmatched",
            "UnsupportedFoldBody",
            "FoldNoComparableCandidate",
            "FoldBoundExceeded",
            "UnknownAgent",
            "UnknownTool",
            "MalformedGraph",
            "ToolFailed",
            "ApprovalSchemaViolation",
            "GraphEncode",
            "Runtime",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            covered, expected,
            "every EngineError variant carries a sample and a decided classification"
        );
    }

    /// The split itself, stated once as a fact rather than variant by variant:
    /// exactly the eight refusals that read only the frozen document and the
    /// recorded log are permanent, and every refusal that depends on
    /// registration, a live call, an input that has not arrived, or the store
    /// is not.
    #[test]
    fn the_permanent_side_is_exactly_the_document_and_log_refusals() {
        let permanent: BTreeSet<&'static str> = samples()
            .iter()
            .filter(|(error, _)| error.is_permanent())
            .map(|(error, _)| variant_name(error))
            .collect();
        let expected: BTreeSet<&'static str> = [
            "BranchDecisionUnmatched",
            "FoldBoundExceeded",
            "FoldNoComparableCandidate",
            "MalformedGraph",
            "MapOverNotAList",
            "NoBranchCaseMatched",
            "UnsupportedFoldBody",
            "UnsupportedMapBody",
        ]
        .into_iter()
        .collect();
        assert_eq!(permanent, expected);
    }
}
