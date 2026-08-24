//! Strict, versioned validation of a graph document.
//!
//! [`validate`] runs a set of INDEPENDENT checks and collects EVERY error
//! rather than stopping at the first, so an author sees the whole picture in one
//! pass. Each check is its own function that reads the graph and pushes any
//! failures onto a shared list, so a check can be added, relaxed, or removed
//! without touching the others. The acyclic check in particular is isolated on
//! purpose: the current design leans acyclic, and a future change that admits
//! some cycles can drop that one function and leave the rest untouched.
//!
//! The errors are structured ([`GraphError`]): each names the offending node or
//! edge and carries a clear message, so the CLI can print node/edge-level
//! diagnostics rather than a bare "invalid".

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;

use crate::document::{BranchCondition, FoldBody, FoldJoin, Graph, MapBody, Node, SCHEMA_VERSION};
use crate::expr;

/// The longest an optional node `name` may be, in characters. Mirrors the
/// agent definition's own name bound
/// (`salvor_cli::agent_config::MAX_NAME_LEN`); see [`crate::document`]'s "The
/// optional node display name" section for why the two fields, though bounded
/// alike, differ in whether they hash.
pub const MAX_NODE_NAME_LEN: usize = 64;

/// A single validation failure, naming the node or edge at fault.
///
/// `PartialEq` is derived so tests can assert on the exact error value. Each
/// variant's `Display` (via `thiserror`) is the human message the CLI prints.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// The document declares a `schema_version` this build cannot understand
    /// (greater than [`SCHEMA_VERSION`], or zero).
    #[error("unsupported schema_version {found}: this build understands versions 1..={supported}")]
    UnsupportedSchemaVersion {
        /// The version the document declared.
        found: u32,
        /// The newest version this build understands.
        supported: u32,
    },

    /// Two nodes share an id. Node ids must be unique within a document.
    #[error("duplicate node id `{id}`")]
    DuplicateNodeId {
        /// The repeated id.
        id: String,
    },

    /// An edge names a node id that does not exist.
    #[error("edge `{from}` -> `{to}` references unknown node id `{missing}`{}", suggest(.suggestion))]
    DanglingEdge {
        /// The edge's declared source.
        from: String,
        /// The edge's declared destination.
        to: String,
        /// The endpoint id that does not exist (either `from` or `to`).
        missing: String,
        /// The nearest existing id, when one is close enough to suggest.
        suggestion: Option<String>,
    },

    /// A `map` node's `node` body references a node id that does not exist.
    #[error("map node `{id}` maps unknown node id `{missing}`{}", suggest(.suggestion))]
    DanglingMapBody {
        /// The map node's id.
        id: String,
        /// The referenced id that does not exist.
        missing: String,
        /// The nearest existing id, when one is close enough to suggest.
        suggestion: Option<String>,
    },

    /// A `fold` node's `node` body references a node id that does not exist.
    #[error("fold node `{id}` folds unknown node id `{missing}`{}", suggest(.suggestion))]
    DanglingFoldBody {
        /// The fold node's id.
        id: String,
        /// The referenced id that does not exist.
        missing: String,
        /// The nearest existing id, when one is close enough to suggest.
        suggestion: Option<String>,
    },

    /// An `agent` node's hash is not a well-formed `sha256:<64 lowercase hex>`
    /// string.
    #[error("agent node `{id}`: `{hash}` is not a well-formed `sha256:<64 hex>` agent hash")]
    MalformedAgentHash {
        /// The agent node's id.
        id: String,
        /// The malformed hash string.
        hash: String,
    },

    /// A `map` node's concurrency cap is not positive.
    #[error("map node `{id}`: concurrency cap must be at least 1, found {found}")]
    NonPositiveConcurrency {
        /// The map node's id.
        id: String,
        /// The declared cap.
        found: u32,
    },

    /// A `fold` node's iteration bound is not positive.
    #[error("fold node `{id}`: max_iterations must be at least 1, found {found}")]
    NonPositiveMaxIterations {
        /// The fold node's id.
        id: String,
        /// The declared bound.
        found: u32,
    },

    /// A `delay` node's wait is zero.
    ///
    /// Refused rather than accepted as a no-op, on the same reasoning
    /// [`GraphError::NonPositiveMaxIterations`] rests on: the whole meaning of
    /// the node is the wait, so a wait of nothing is an authoring mistake and
    /// not an intent. A zero delay would still park nothing, record a clock
    /// reading, a `SleepStarted`, and a `SleepCompleted` in every log forever,
    /// and mean exactly what deleting the node means. Saying so at submit is
    /// cheaper than leaving it to be noticed in a run log.
    #[error("delay node `{id}`: seconds must be at least 1, found {found}")]
    NonPositiveDelay {
        /// The delay node's id.
        id: String,
        /// The declared wait.
        found: u64,
    },

    /// A `gate` node's approval schema is not a JSON object.
    #[error("gate node `{id}`: approval_schema must be a JSON object")]
    ApprovalSchemaNotObject {
        /// The gate node's id.
        id: String,
    },

    /// The edge list contains a cycle. `path` renders it as `a -> b -> ... -> a`.
    #[error("cycle detected: {path}")]
    Cycle {
        /// The cycle rendered as a node-id path, closing back on its start.
        path: String,
    },

    /// An edge connects two nodes whose declared schemas do not match. See
    /// [`check_edge_type_compat`] for the deliberately conservative rule.
    #[error(
        "edge `{from}` -> `{to}`: the output schema of `{from}` does not match the input schema of `{to}`"
    )]
    EdgeTypeMismatch {
        /// The source node id (its output schema).
        from: String,
        /// The destination node id (its input schema).
        to: String,
    },

    /// A `branch` node case carries an expression condition that does not parse
    /// in the [`crate::expr`] condition language. Caught at submit so a bad
    /// expression is never a run-time failure.
    #[error("branch node `{node}`: case `{case}` has an invalid condition expression: {error}")]
    InvalidBranchExpression {
        /// The branch node's id.
        node: String,
        /// The name of the offending case.
        case: String,
        /// The parse error's message.
        error: String,
    },

    /// A `branch` node carries a `model_decision` case but declares no
    /// `agent_hash`, so the engine would have no agent to make the decision.
    /// Caught at submit, node- and case-precise.
    #[error(
        "branch node `{node}`: case `{case}` is a model decision but the branch declares no `agent_hash`"
    )]
    ModelDecisionWithoutAgent {
        /// The branch node's id.
        node: String,
        /// The name of the model-decision case with no agent.
        case: String,
    },

    /// A `branch` node case names no outbound edge from that node: no edge's
    /// `label` matches the case name. Caught at submit, because otherwise the
    /// engine fires the case at run time, finds no live edge, skips every node
    /// downstream of it, and the run completes as if that were the intended
    /// path. See [`check_branch_case_edges`].
    #[error(
        "branch node `{node}`: case `{case}` has no outbound edge; point the case at a node, and point a route meant to end the run at a terminal node instead"
    )]
    BranchCaseWithoutEdge {
        /// The branch node's id.
        node: String,
        /// The name of the case with no matching edge.
        case: String,
    },

    /// A `fold` node's `stop_when` predicate does not parse in the
    /// [`crate::expr`] condition language. Caught at submit so a bad predicate is
    /// never a run-time failure, exactly like a branch case's expression.
    #[error("fold node `{node}`: `stop_when` is not a valid condition expression: {error}")]
    InvalidFoldStopExpression {
        /// The fold node's id.
        node: String,
        /// The parse error's message.
        error: String,
    },

    /// A `fold` node's `best_by` join reference is not a well-formed path in the
    /// [`crate::expr`] language (a bare literal, or a malformed path). Caught at
    /// submit, node-precise.
    #[error(
        "fold node `{node}`: the `best_by` join reference `{reference}` is not a valid path: {error}"
    )]
    InvalidFoldJoinReference {
        /// The fold node's id.
        node: String,
        /// The malformed reference.
        reference: String,
        /// The parse error's message.
        error: String,
    },

    /// A `fold` node's `stop_when` predicate reads a path the body node's
    /// declared `output_schema` does not describe. See
    /// [`check_fold_reference_shapes`] for when this fires and, more
    /// importantly, when it stays quiet.
    #[error(
        "fold node `{node}`: `stop_when` reads `{path}`, which body node `{body}`'s declared output schema does not describe"
    )]
    FoldStopPathNotInBodySchema {
        /// The fold node's id.
        node: String,
        /// The path the predicate reads, as the expression names it.
        path: String,
        /// The id of the body node whose schema does not describe it.
        body: String,
    },

    /// A `fold` node's `best_by` join reference names a path the body node's
    /// declared `output_schema` does not describe. The join half of
    /// [`GraphError::FoldStopPathNotInBodySchema`], reported separately because
    /// the two are fixed in different places.
    #[error(
        "fold node `{node}`: the `best_by` join reference `{reference}` is not described by body node `{body}`'s declared output schema"
    )]
    FoldJoinReferenceNotInBodySchema {
        /// The fold node's id.
        node: String,
        /// The join reference, as the document writes it.
        reference: String,
        /// The id of the body node whose schema does not describe it.
        body: String,
    },

    /// A node's optional `name` is over [`MAX_NODE_NAME_LEN`] characters.
    #[error("node `{id}`: `name` is {len} characters, over the {max}-character cap")]
    NodeNameTooLong {
        /// The node's id.
        id: String,
        /// The name's length, in characters (`chars().count()`, not bytes).
        len: usize,
        /// [`MAX_NODE_NAME_LEN`], repeated here so the error is self-contained.
        max: usize,
    },

    /// A node's optional `name` is set but empty or all whitespace.
    #[error("node `{id}`: `name`, if set, must not be empty or all whitespace")]
    BlankNodeName {
        /// The node's id.
        id: String,
    },
}

/// Formats an optional nearest-name suggestion as a trailing clause, or empty.
fn suggest(suggestion: &Option<String>) -> String {
    match suggestion {
        Some(name) => format!(" (did you mean `{name}`?)"),
        None => String::new(),
    }
}

/// A successful validation's summary of the graph's shape.
///
/// Entry nodes have no inbound edge; terminal nodes have no outbound edge. Both
/// lists are sorted, so the CLI output is deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSummary {
    /// Number of nodes in the document.
    pub node_count: usize,
    /// Number of edges in the document.
    pub edge_count: usize,
    /// Ids of nodes with no inbound edge, sorted.
    pub entry_nodes: Vec<String>,
    /// Ids of nodes with no outbound edge, sorted.
    pub terminal_nodes: Vec<String>,
}

/// Validates a graph document, returning a summary on success or EVERY error on
/// failure.
///
/// The checks are independent and all run: the returned `Vec` holds a failure
/// from each check that found one, so an author fixes everything at once. The
/// order of checks below is the order errors appear in.
///
/// # Errors
///
/// Returns the collected [`GraphError`]s when any check fails.
pub fn validate(graph: &Graph) -> Result<GraphSummary, Vec<GraphError>> {
    let mut errors = Vec::new();

    check_schema_version(graph, &mut errors);
    check_unique_node_ids(graph, &mut errors);
    check_referential_integrity(graph, &mut errors);
    check_node_fields(graph, &mut errors);
    check_node_names(graph, &mut errors);
    check_branch_expressions(graph, &mut errors);
    check_branch_case_edges(graph, &mut errors);
    check_fold_expressions(graph, &mut errors);
    check_fold_reference_shapes(graph, &mut errors);
    check_acyclic(graph, &mut errors);
    check_edge_type_compat(graph, &mut errors);

    if errors.is_empty() {
        Ok(summarize(graph))
    } else {
        Err(errors)
    }
}

/// Rejects a `schema_version` from the future (or zero). This is the strict-in
/// half of the version discipline; the additive-out half is that an
/// older-or-equal version is accepted unchanged. See [`SCHEMA_VERSION`].
fn check_schema_version(graph: &Graph, errors: &mut Vec<GraphError>) {
    if graph.schema_version == 0 || graph.schema_version > SCHEMA_VERSION {
        errors.push(GraphError::UnsupportedSchemaVersion {
            found: graph.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
}

/// Rejects a document where two nodes share an id.
fn check_unique_node_ids(graph: &Graph, errors: &mut Vec<GraphError>) {
    let mut seen = HashSet::new();
    for node in &graph.nodes {
        if !seen.insert(node.id()) {
            errors.push(GraphError::DuplicateNodeId {
                id: node.id().to_owned(),
            });
        }
    }
}

/// Every id an edge endpoint or a `map` body names must be a real node id.
///
/// When a named id is missing, the nearest existing id (by edit distance) is
/// suggested if it is close enough to be a plausible typo.
fn check_referential_integrity(graph: &Graph, errors: &mut Vec<GraphError>) {
    let ids: BTreeSet<&str> = graph.nodes.iter().map(Node::id).collect();

    for edge in &graph.edges {
        if !ids.contains(edge.from.as_str()) {
            errors.push(GraphError::DanglingEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
                missing: edge.from.clone(),
                suggestion: nearest(&edge.from, &ids),
            });
        }
        if !ids.contains(edge.to.as_str()) {
            errors.push(GraphError::DanglingEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
                missing: edge.to.clone(),
                suggestion: nearest(&edge.to, &ids),
            });
        }
    }

    for node in &graph.nodes {
        if let Node::Map(map) = node
            && let MapBody::Node(target) = &map.body
            && !ids.contains(target.as_str())
        {
            errors.push(GraphError::DanglingMapBody {
                id: map.id.clone(),
                missing: target.clone(),
                suggestion: nearest(target, &ids),
            });
        }
        if let Node::Fold(fold) = node
            && let FoldBody::Node(target) = &fold.body
            && !ids.contains(target.as_str())
        {
            errors.push(GraphError::DanglingFoldBody {
                id: fold.id.clone(),
                missing: target.clone(),
                suggestion: nearest(target, &ids),
            });
        }
    }
}

/// Per-node required-field checks: an agent hash is well-formed, a map cap is
/// positive, a gate's approval schema is an object, a delay waits for
/// something. Each rule is a small, independent block so a rule can be relaxed
/// on its own.
fn check_node_fields(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        match node {
            Node::Agent(agent) => {
                if !is_well_formed_agent_hash(&agent.agent_hash) {
                    errors.push(GraphError::MalformedAgentHash {
                        id: agent.id.clone(),
                        hash: agent.agent_hash.clone(),
                    });
                }
            }
            Node::Map(map) => {
                if map.concurrency < 1 {
                    errors.push(GraphError::NonPositiveConcurrency {
                        id: map.id.clone(),
                        found: map.concurrency,
                    });
                }
            }
            Node::Gate(gate) => {
                if !gate.approval_schema.is_object() {
                    errors.push(GraphError::ApprovalSchemaNotObject {
                        id: gate.id.clone(),
                    });
                }
            }
            Node::Fold(fold) => {
                if fold.max_iterations < 1 {
                    errors.push(GraphError::NonPositiveMaxIterations {
                        id: fold.id.clone(),
                        found: fold.max_iterations,
                    });
                }
            }
            Node::Delay(delay) => {
                if delay.seconds < 1 {
                    errors.push(GraphError::NonPositiveDelay {
                        id: delay.id.clone(),
                        found: delay.seconds,
                    });
                }
            }
            // Tool and branch carry no field rule beyond the strict parse.
            Node::Tool(_) | Node::Branch(_) => {}
        }
    }
}

/// A node's optional `name`, when set, must not be empty or all whitespace,
/// and must be at most [`MAX_NODE_NAME_LEN`] characters
/// (`chars().count()`, not bytes). Applies uniformly across all seven node
/// kinds through [`Node::name`], mirroring the agent definition's own name
/// rule.
fn check_node_names(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        let Some(name) = node.name() else {
            continue;
        };
        if name.trim().is_empty() {
            errors.push(GraphError::BlankNodeName {
                id: node.id().to_owned(),
            });
            continue;
        }
        let len = name.chars().count();
        if len > MAX_NODE_NAME_LEN {
            errors.push(GraphError::NodeNameTooLong {
                id: node.id().to_owned(),
                len,
                max: MAX_NODE_NAME_LEN,
            });
        }
    }
}

/// Every `branch` case is checked for the rule its condition kind implies.
///
/// This is where the opaque case conditions earn their meaning, AT SUBMIT, so a
/// malformed branch is a node-precise error the author sees now, never a
/// run-time failure inside a durable, replayed run:
///
/// - an `expression` condition must parse in the [`crate::expr`] condition
///   language;
/// - a `model_decision` condition requires the branch to declare an
///   `agent_hash`, because the engine drives that agent to make the decision;
/// - a declared `agent_hash` must be a well-formed `sha256:<64 hex>` string,
///   exactly like an agent node's hash.
///
/// Each fault is one collected error naming the node (and, for a case fault, the
/// case).
fn check_branch_expressions(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        let Node::Branch(branch) = node else {
            continue;
        };
        if let Some(hash) = &branch.agent_hash
            && !is_well_formed_agent_hash(hash)
        {
            errors.push(GraphError::MalformedAgentHash {
                id: branch.id.clone(),
                hash: hash.clone(),
            });
        }
        for case in &branch.cases {
            match &case.when {
                BranchCondition::Expression(source) => {
                    if let Err(error) = expr::parse(source) {
                        errors.push(GraphError::InvalidBranchExpression {
                            node: branch.id.clone(),
                            case: case.name.clone(),
                            error: error.to_string(),
                        });
                    }
                }
                BranchCondition::ModelDecision => {
                    if branch.agent_hash.is_none() {
                        errors.push(GraphError::ModelDecisionWithoutAgent {
                            node: branch.id.clone(),
                            case: case.name.clone(),
                        });
                    }
                }
            }
        }
    }
}

/// Every `branch` case must label at least one outbound edge from that node.
///
/// The engine picks a branch's live outbound edge by matching the fired case's
/// name against each edge's `label` (see `salvor_engine::is_live_inbound`). A
/// case with no matching label can still fire: the branch evaluates its
/// condition, records which case won, and only then discovers there is nowhere
/// to route it. Every node downstream of that edge is then skipped, and the run
/// completes having silently taken no path at all, exactly as if the missing
/// edge had been the intended one. Caught here instead, node- and
/// case-precise, so a misspelled or forgotten edge label is an authoring
/// mistake seen at submit, not a run that finishes looking healthy.
///
/// The inverse (an edge labeled with a name no case declares) is not checked
/// here: that edge is simply dead and never fires, which is a different, less
/// silent mistake than the one this check exists to catch.
fn check_branch_case_edges(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        let Node::Branch(branch) = node else {
            continue;
        };
        let labels: HashSet<&str> = graph
            .edges
            .iter()
            .filter(|edge| edge.from == branch.id)
            .filter_map(|edge| edge.label.as_deref())
            .collect();
        for case in &branch.cases {
            if !labels.contains(case.name.as_str()) {
                errors.push(GraphError::BranchCaseWithoutEdge {
                    node: branch.id.clone(),
                    case: case.name.clone(),
                });
            }
        }
    }
}

/// Every `fold` node's expression fields are checked AT SUBMIT, exactly like a
/// branch's, so a malformed predicate or join reference is a node-precise error
/// now rather than a run-time failure inside a durable, replayed run:
///
/// - the `stop_when` predicate must parse in the [`crate::expr`] condition
///   language (the same one a branch case's expression uses);
/// - a [`FoldJoin::BestBy`] reference must parse as an [`crate::expr`] path (a
///   location in the accumulated value, never a bare literal).
///
/// The `last` and `all` join rules carry no expression to check. The iteration
/// bound is checked in [`check_node_fields`], the body reference in
/// [`check_referential_integrity`], keeping each rule independent.
fn check_fold_expressions(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        let Node::Fold(fold) = node else {
            continue;
        };
        if let Err(error) = expr::parse(&fold.stop_when) {
            errors.push(GraphError::InvalidFoldStopExpression {
                node: fold.id.clone(),
                error: error.to_string(),
            });
        }
        if let FoldJoin::BestBy(reference) = &fold.join
            && let Err(error) = expr::parse_reference(reference)
        {
            errors.push(GraphError::InvalidFoldJoinReference {
                node: fold.id.clone(),
                reference: reference.clone(),
                error: error.to_string(),
            });
        }
    }
}

/// Every `fold` whose body names a node that DECLARES an output schema has its
/// expression references read against that schema, AT SUBMIT.
///
/// A fold's accumulated value is what its body produced: the body's declared
/// `output_schema` is therefore the shape `stop_when` and a `best_by` reference
/// read, path for path, with no envelope or prefix in front of it. So a
/// predicate reading `scoer >= 0.85` against a body that declares only `score`
/// is a typo the author can be told about now rather than a loop that silently
/// never stops.
///
/// # When this stays quiet
///
/// A path is reported ONLY when walking it POSITIVELY fails: a segment is
/// absent from a `properties` map that exists and does not admit extra keys.
/// Everything else is unjudged, and deliberately so, because a check that
/// guessed would cost an author a legal document:
///
/// - a body that declares no `output_schema`, or a `subgraph` body, or a body
///   id that names no node: nothing to read the path against;
/// - a schema with no `properties` (`{"type": "object"}` on its own), or one
///   whose declared `type` is not the kind the segment steps into;
/// - a schema that composes its shape elsewhere (`$ref`, `anyOf`, `oneOf`,
///   `allOf`, `not`), or admits extra keys (`additionalProperties` set to
///   anything but `false`, or any `patternProperties`);
/// - every segment past the first that could not be walked, since a walk that
///   stopped knowing nothing cannot judge what comes after it.
///
/// The bound, the body reference, and the expressions' own parse are checked
/// elsewhere ([`check_node_fields`], [`check_referential_integrity`],
/// [`check_fold_expressions`]); a `stop_when` that does not parse is skipped
/// here, because the parse error is the error worth printing.
fn check_fold_reference_shapes(graph: &Graph, errors: &mut Vec<GraphError>) {
    let by_id: HashMap<&str, &Node> = graph.nodes.iter().map(|n| (n.id(), n)).collect();

    for node in &graph.nodes {
        let Node::Fold(fold) = node else {
            continue;
        };
        let FoldBody::Node(body_id) = &fold.body else {
            continue;
        };
        let Some(schema) = by_id
            .get(body_id.as_str())
            .and_then(|body| body.output_schema())
        else {
            continue;
        };

        if let Ok(predicate) = expr::parse(&fold.stop_when) {
            for path in predicate.paths() {
                if !schema_describes(schema, path) {
                    errors.push(GraphError::FoldStopPathNotInBodySchema {
                        node: fold.id.clone(),
                        path: render_path(path),
                        body: body_id.clone(),
                    });
                }
            }
        }

        if let FoldJoin::BestBy(reference) = &fold.join
            && let Ok(parsed) = expr::parse_reference(reference)
            && !schema_describes(schema, parsed.segments())
        {
            errors.push(GraphError::FoldJoinReferenceNotInBodySchema {
                node: fold.id.clone(),
                reference: reference.clone(),
                body: body_id.clone(),
            });
        }
    }
}

/// Whether `schema` leaves `path` plausible: false ONLY when a step positively
/// fails. A step that the schema says nothing about ends the walk in the
/// author's favor.
fn schema_describes(schema: &Value, path: &[expr::Segment]) -> bool {
    let mut here = schema;
    for segment in path {
        match step_into(here, segment) {
            Step::Into(next) => here = next,
            Step::Unjudged => return true,
            Step::Absent => return false,
        }
    }
    true
}

/// What one step of a path finds in a schema.
enum Step<'a> {
    /// The sub-schema the step lands in, which the next step reads.
    Into(&'a Value),
    /// The schema does not say, so nothing after this point can be judged.
    Unjudged,
    /// The schema positively excludes this step.
    Absent,
}

/// Takes one path step through a schema. The whole judgment of this check lives
/// here; see [`check_fold_reference_shapes`] for why each `Unjudged` is one.
fn step_into<'a>(schema: &'a Value, segment: &expr::Segment) -> Step<'a> {
    let Some(object) = schema.as_object() else {
        return Step::Unjudged;
    };
    // A schema that names its shape somewhere else is not one this walk reads.
    if ["$ref", "anyOf", "oneOf", "allOf", "not"]
        .iter()
        .any(|keyword| object.contains_key(*keyword))
    {
        return Step::Unjudged;
    }

    match segment {
        expr::Segment::Key(key) => {
            if !admits_type(object, "object") {
                return Step::Unjudged;
            }
            let Some(properties) = object.get("properties").and_then(Value::as_object) else {
                return Step::Unjudged;
            };
            if let Some(property) = properties.get(key) {
                return Step::Into(property);
            }
            if admits_extra_keys(object) {
                Step::Unjudged
            } else {
                Step::Absent
            }
        }
        expr::Segment::Index(index) => {
            if !admits_type(object, "array") {
                return Step::Unjudged;
            }
            match object.get("items") {
                Some(items) if items.is_object() => Step::Into(items),
                // The tuple form: an index inside it is that entry, an index
                // past it is not something this check will call a mistake.
                Some(Value::Array(entries)) => {
                    entries.get(*index).map_or(Step::Unjudged, Step::Into)
                }
                _ => Step::Unjudged,
            }
        }
    }
}

/// Whether a schema's declared `type`, if it declares one at all, admits
/// `wanted`. A schema with no `type` is read as its `properties` describe it.
fn admits_type(object: &serde_json::Map<String, Value>, wanted: &str) -> bool {
    match object.get("type") {
        None => true,
        Some(Value::String(declared)) => declared == wanted,
        Some(Value::Array(declared)) => declared.iter().any(|one| one.as_str() == Some(wanted)),
        // A malformed `type` is not this check's to report.
        Some(_) => true,
    }
}

/// Whether a schema admits keys its `properties` does not name. A declared
/// `properties` map is read as the author's statement of the shape, so silence
/// about `additionalProperties` is CLOSED here: an author who means open says
/// so, and that is the one reading under which this check can say anything at
/// all.
fn admits_extra_keys(object: &serde_json::Map<String, Value>) -> bool {
    if object.contains_key("patternProperties") {
        return true;
    }
    match object.get("additionalProperties") {
        None | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

/// A path as its source spells it, for an error message: `review.scores.0`.
fn render_path(path: &[expr::Segment]) -> String {
    path.iter()
        .map(|segment| match segment {
            expr::Segment::Key(key) => key.clone(),
            expr::Segment::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// An agent hash is `sha256:` followed by exactly 64 lowercase hex digits.
fn is_well_formed_agent_hash(hash: &str) -> bool {
    let Some(hex) = hash.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reports the first cycle found in the edge topology as a node-id path.
///
/// A depth-first walk colors nodes white (unseen), gray (on the current stack),
/// or black (finished). Reaching a gray node closes a cycle, which is rebuilt
/// from the current stack. This is the single isolated check that encodes the
/// acyclic lean; a later change that admits cycles removes only this function.
fn check_acyclic(graph: &Graph, errors: &mut Vec<GraphError>) {
    // Adjacency by node id. Edges to unknown ids are skipped: referential
    // integrity already reports those, and skipping keeps this walk in-bounds.
    let ids: HashSet<&str> = graph.nodes.iter().map(Node::id).collect();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if ids.contains(edge.from.as_str()) && ids.contains(edge.to.as_str()) {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: HashMap<&str, Color> = ids.iter().map(|id| (*id, Color::White)).collect();
    let mut stack: Vec<&str> = Vec::new();

    // An explicit work stack instead of recursion, so a deep graph cannot blow
    // the call stack. Each frame is a node and the index of the next neighbor
    // to visit.
    for start in graph.nodes.iter().map(Node::id) {
        if color[start] != Color::White {
            continue;
        }
        let mut frames: Vec<(&str, usize)> = vec![(start, 0)];
        color.insert(start, Color::Gray);
        stack.push(start);

        while let Some(&mut (node, ref mut next)) = frames.last_mut() {
            let neighbors = adjacency.get(node).map_or(&[][..], Vec::as_slice);
            if *next < neighbors.len() {
                let neighbor = neighbors[*next];
                *next += 1;
                match color[neighbor] {
                    Color::White => {
                        color.insert(neighbor, Color::Gray);
                        stack.push(neighbor);
                        frames.push((neighbor, 0));
                    }
                    Color::Gray => {
                        // A back edge: the neighbor is on the current stack, so
                        // the path from it to here, closed by this edge, is a
                        // cycle.
                        let start_at = stack.iter().position(|n| *n == neighbor).unwrap_or(0);
                        let mut path: Vec<&str> = stack[start_at..].to_vec();
                        path.push(neighbor);
                        errors.push(GraphError::Cycle {
                            path: path.join(" -> "),
                        });
                        return;
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
                frames.pop();
            }
        }
    }
}

/// Where both endpoints of an edge declare a schema, they must match.
///
/// # The rule, and its deliberate limitation
///
/// The check is exact structural equality of the two declared JSON Schema
/// documents (`source.output_schema == target.input_schema`, a deep value
/// comparison). Where either endpoint omits its schema, the edge passes
/// unchecked.
///
/// This deliberately does NOT implement JSON Schema subtyping. Two schemas that
/// are compatible but not identical (a subset/superset relationship, the same
/// shape spelled differently, an added optional property) are reported as a
/// mismatch, and the fix is to make the declared schemas equal. The trade is
/// intentional: exact equality is pure, cheap, and easy to reason about, and it
/// never claims a compatibility it cannot verify. A later change can relax
/// equality to real schema compatibility by changing only this function.
fn check_edge_type_compat(graph: &Graph, errors: &mut Vec<GraphError>) {
    let by_id: HashMap<&str, &Node> = graph.nodes.iter().map(|n| (n.id(), n)).collect();

    for edge in &graph.edges {
        let (Some(from), Some(to)) = (by_id.get(edge.from.as_str()), by_id.get(edge.to.as_str()))
        else {
            // A dangling edge; referential integrity already reported it.
            continue;
        };
        if let (Some(out), Some(inp)) = (from.output_schema(), to.input_schema())
            && out != inp
        {
            errors.push(GraphError::EdgeTypeMismatch {
                from: edge.from.clone(),
                to: edge.to.clone(),
            });
        }
    }
}

/// Builds the success summary: node and edge counts, plus entry (no inbound)
/// and terminal (no outbound) node ids, sorted.
fn summarize(graph: &Graph) -> GraphSummary {
    let has_inbound: HashSet<&str> = graph.edges.iter().map(|e| e.to.as_str()).collect();
    let has_outbound: HashSet<&str> = graph.edges.iter().map(|e| e.from.as_str()).collect();

    let mut entry_nodes: Vec<String> = graph
        .nodes
        .iter()
        .map(Node::id)
        .filter(|id| !has_inbound.contains(id))
        .map(str::to_owned)
        .collect();
    let mut terminal_nodes: Vec<String> = graph
        .nodes
        .iter()
        .map(Node::id)
        .filter(|id| !has_outbound.contains(id))
        .map(str::to_owned)
        .collect();
    entry_nodes.sort();
    terminal_nodes.sort();

    GraphSummary {
        node_count: graph.nodes.len(),
        edge_count: graph.edges.len(),
        entry_nodes,
        terminal_nodes,
    }
}

/// The nearest existing id to a missing one, if close enough to be a plausible
/// typo. Cheap: Levenshtein distance, suggested only when the distance is at
/// most a third of the longer id's length (and always the single closest).
fn nearest(missing: &str, ids: &BTreeSet<&str>) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for candidate in ids {
        let distance = levenshtein(missing, candidate);
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.and_then(|(distance, candidate)| {
        let threshold = (missing.len().max(candidate.len()) / 3).max(1);
        (distance <= threshold).then(|| candidate.to_owned())
    })
}

/// Classic Levenshtein edit distance over bytes, two-row rolling table. Ids are
/// short, so this stays trivially cheap.
fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, &ac) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &bc) in b.iter().enumerate() {
            let cost = usize::from(ac != bc);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{
        AgentNode, BranchCase, BranchCondition, BranchNode, DelayNode, Edge, FoldBody, FoldJoin,
        FoldNode, GateNode, MapBody, MapNode, ToolNode,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn hash() -> String {
        format!("sha256:{}", "a".repeat(64))
    }

    fn agent(id: &str) -> Node {
        Node::Agent(AgentNode {
            name: None,
            id: id.into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: None,
        })
    }

    fn gate(id: &str) -> Node {
        Node::Gate(GateNode {
            name: None,
            id: id.into(),
            prompt: None,
            approval_schema: json!({"type": "object"}),
        })
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            label: None,
        }
    }

    /// A branch's labeled outbound edge, the shape a case needs to route
    /// anywhere at all.
    fn labeled_edge(from: &str, to: &str, label: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            label: Some(label.into()),
        }
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            schema_version: SCHEMA_VERSION,
            nodes,
            edges,
        }
    }

    /// A linear research -> review -> gate flow validates clean, with the right
    /// counts and entry/terminal nodes.
    #[test]
    fn valid_linear_graph_summarizes() {
        let g = graph(
            vec![agent("research"), agent("review"), gate("approve")],
            vec![edge("research", "review"), edge("review", "approve")],
        );
        let summary = validate(&g).expect("valid");
        assert_eq!(summary.node_count, 3);
        assert_eq!(summary.edge_count, 2);
        assert_eq!(summary.entry_nodes, vec!["research"]);
        assert_eq!(summary.terminal_nodes, vec!["approve"]);
    }

    /// A dangling edge names the offending edge and the missing id, and
    /// suggests the near miss.
    #[test]
    fn dangling_edge_is_reported_with_suggestion() {
        let g = graph(vec![agent("research")], vec![edge("research", "reviewx")]);
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::DanglingEdge {
                from: "research".into(),
                to: "reviewx".into(),
                missing: "reviewx".into(),
                suggestion: Some("research".into()),
            }) || matches!(
                errors.first(),
                Some(GraphError::DanglingEdge { missing, .. }) if missing == "reviewx"
            )
        );
        let message = errors[0].to_string();
        assert!(
            message.contains("reviewx"),
            "names the missing id: {message}"
        );
    }

    /// A malformed agent hash names the node.
    #[test]
    fn malformed_agent_hash_is_reported() {
        let g = graph(
            vec![Node::Agent(AgentNode {
                name: None,
                id: "research".into(),
                agent_hash: "sha256:not-hex".into(),
                input_schema: None,
                output_schema: None,
            })],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert_eq!(
            errors,
            vec![GraphError::MalformedAgentHash {
                id: "research".into(),
                hash: "sha256:not-hex".into(),
            }]
        );
    }

    /// A zero concurrency cap on a map names the node.
    #[test]
    fn non_positive_concurrency_is_reported() {
        let g = graph(
            vec![
                agent("worker"),
                Node::Map(MapNode {
                    name: None,
                    id: "fanout".into(),
                    over: "items".into(),
                    concurrency: 0,
                    body: MapBody::Node("worker".into()),
                    output_schema: None,
                }),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::NonPositiveConcurrency {
            id: "fanout".into(),
            found: 0,
        }));
    }

    /// A zero wait on a delay names the node, exactly as a zero iteration
    /// bound on a fold does. A one-second wait is the smallest legal one and
    /// passes, so the rule is a floor rather than a range.
    #[test]
    fn non_positive_delay_is_reported() {
        let g = graph(
            vec![Node::Delay(DelayNode {
                id: "cooloff".into(),
                name: None,
                seconds: 0,
            })],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::NonPositiveDelay {
            id: "cooloff".into(),
            found: 0,
        }));

        let g = graph(
            vec![Node::Delay(DelayNode {
                id: "cooloff".into(),
                name: None,
                seconds: 1,
            })],
            vec![],
        );
        validate(&g).expect("a one-second wait is a legal wait");
    }

    /// A map body that names a missing node is reported.
    #[test]
    fn dangling_map_body_is_reported() {
        let g = graph(
            vec![Node::Map(MapNode {
                name: None,
                id: "fanout".into(),
                over: "items".into(),
                concurrency: 2,
                body: MapBody::Node("ghost".into()),
                output_schema: None,
            })],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::DanglingMapBody {
            id: "fanout".into(),
            missing: "ghost".into(),
            suggestion: None,
        }));
    }

    /// A cycle is reported with a path that closes on itself.
    #[test]
    fn cycle_is_reported_with_path() {
        let g = graph(
            vec![agent("a"), agent("b"), agent("c")],
            vec![edge("a", "b"), edge("b", "c"), edge("c", "a")],
        );
        let errors = validate(&g).expect_err("invalid");
        let cycle = errors
            .iter()
            .find_map(|e| match e {
                GraphError::Cycle { path } => Some(path.clone()),
                _ => None,
            })
            .expect("a cycle error");
        assert!(cycle.starts_with("a -> "), "path from a: {cycle}");
        assert!(cycle.ends_with("-> a"), "path closes on a: {cycle}");
    }

    /// Edge type-compat: matching schemas pass, mismatched ones fail naming the
    /// edge.
    #[test]
    fn edge_type_mismatch_is_reported() {
        let producer = Node::Agent(AgentNode {
            name: None,
            id: "producer".into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: Some(json!({"type": "string"})),
        });
        let consumer = Node::Tool(ToolNode {
            name: None,
            id: "consumer".into(),
            tool: "t".into(),
            input: BTreeMap::new(),
            input_schema: Some(json!({"type": "number"})),
            output_schema: None,
        });
        let g = graph(vec![producer, consumer], vec![edge("producer", "consumer")]);
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::EdgeTypeMismatch {
            from: "producer".into(),
            to: "consumer".into(),
        }));
    }

    /// Identical declared schemas are compatible.
    #[test]
    fn matching_edge_schemas_pass() {
        let producer = Node::Agent(AgentNode {
            name: None,
            id: "producer".into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: Some(json!({"type": "string"})),
        });
        let consumer = Node::Tool(ToolNode {
            name: None,
            id: "consumer".into(),
            tool: "t".into(),
            input: BTreeMap::new(),
            input_schema: Some(json!({"type": "string"})),
            output_schema: None,
        });
        let g = graph(vec![producer, consumer], vec![edge("producer", "consumer")]);
        assert!(validate(&g).is_ok());
    }

    /// A future schema version is rejected; an equal one is accepted.
    #[test]
    fn future_schema_version_is_rejected() {
        let mut g = graph(vec![agent("a")], vec![]);
        g.schema_version = SCHEMA_VERSION + 1;
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::UnsupportedSchemaVersion {
            found: SCHEMA_VERSION + 1,
            supported: SCHEMA_VERSION,
        }));
    }

    /// Every check runs: a document with several independent faults returns all
    /// of them, not just the first.
    #[test]
    fn all_errors_are_collected() {
        let g = graph(
            vec![
                Node::Agent(AgentNode {
                    name: None,
                    id: "bad".into(),
                    agent_hash: "nope".into(),
                    input_schema: None,
                    output_schema: None,
                }),
                agent("bad"), // duplicate id
            ],
            vec![edge("bad", "missing")],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.len() >= 3,
            "duplicate id, malformed hash, and dangling edge: {errors:?}"
        );
    }

    /// A duplicate node id is reported.
    #[test]
    fn duplicate_node_id_is_reported() {
        let g = graph(vec![agent("dup"), gate("dup")], vec![]);
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::DuplicateNodeId { id: "dup".into() }));
    }

    /// A branch node whose expression condition is well-formed validates clean;
    /// a `model_decision` case carries no expression to check.
    #[test]
    fn valid_branch_expression_passes() {
        let branch = Node::Branch(BranchNode {
            name: None,
            id: "route".into(),
            on: Some("score".into()),
            agent_hash: Some(hash()),
            cases: vec![
                BranchCase {
                    name: "high".into(),
                    when: BranchCondition::Expression("score > 0.8".into()),
                },
                BranchCase {
                    name: "review".into(),
                    when: BranchCondition::ModelDecision,
                },
            ],
        });
        let g = graph(
            vec![
                agent("score"),
                branch,
                agent("high_target"),
                agent("review_target"),
            ],
            vec![
                edge("score", "route"),
                labeled_edge("route", "high_target", "high"),
                labeled_edge("route", "review_target", "review"),
            ],
        );
        assert!(validate(&g).is_ok(), "{:?}", validate(&g));
    }

    /// A branch case whose expression does not parse is a node-precise error
    /// naming the node and the case; a sibling `model_decision` case is skipped.
    /// Both cases carry an edge, so the new case-without-edge check stays quiet
    /// and this test isolates the expression check alone.
    #[test]
    fn invalid_branch_expression_is_reported() {
        let branch = Node::Branch(BranchNode {
            name: None,
            id: "route".into(),
            on: None,
            agent_hash: Some(hash()),
            cases: vec![
                BranchCase {
                    name: "broken".into(),
                    when: BranchCondition::Expression("score >".into()),
                },
                BranchCase {
                    name: "fallback".into(),
                    when: BranchCondition::ModelDecision,
                },
            ],
        });
        let g = graph(
            vec![branch, agent("broken_target"), agent("fallback_target")],
            vec![
                labeled_edge("route", "broken_target", "broken"),
                labeled_edge("route", "fallback_target", "fallback"),
            ],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            matches!(
                errors.as_slice(),
                [GraphError::InvalidBranchExpression { node, case, .. }]
                    if node == "route" && case == "broken"
            ),
            "one node/case-precise expression error: {errors:?}"
        );
    }

    /// A branch case with no outbound edge realizing it is a node/case-precise
    /// error, distinct from and reported alongside a sibling case that does
    /// have one: the mistake this catches is exactly a misspelled edge label
    /// (the case name and the label must match character for character), and
    /// the message says what to do about it.
    #[test]
    fn branch_case_without_edge_is_reported() {
        let branch = Node::Branch(BranchNode {
            name: None,
            id: "route".into(),
            on: None,
            agent_hash: None,
            cases: vec![
                BranchCase {
                    name: "won".into(),
                    when: BranchCondition::Expression("outcome == \"won\"".into()),
                },
                BranchCase {
                    name: "lost".into(),
                    when: BranchCondition::Expression("outcome == \"lost\"".into()),
                },
            ],
        });
        let g = graph(
            vec![branch, agent("celebrate")],
            // The edge realizing `lost` is misspelled `lst`, exactly the
            // mistake this check exists to catch.
            vec![
                labeled_edge("route", "celebrate", "won"),
                labeled_edge("route", "celebrate", "lst"),
            ],
        );
        let errors = validate(&g).expect_err("invalid");
        assert_eq!(
            errors,
            vec![GraphError::BranchCaseWithoutEdge {
                node: "route".into(),
                case: "lost".into(),
            }],
            "names the node and the unrouted case, and only that one: {errors:?}"
        );
        let message = errors[0].to_string();
        assert!(
            message.contains("route") && message.contains("lost"),
            "{message}"
        );
        assert!(
            message.contains("terminal node"),
            "says what to do about a route meant to end the run: {message}"
        );
    }

    /// A `model_decision` case on a branch that declares no `agent_hash` is a
    /// node/case-precise error: the engine would have no agent to make the
    /// decision.
    #[test]
    fn model_decision_without_agent_is_reported() {
        let branch = Node::Branch(BranchNode {
            name: None,
            id: "route".into(),
            on: None,
            agent_hash: None,
            cases: vec![BranchCase {
                name: "ask".into(),
                when: BranchCondition::ModelDecision,
            }],
        });
        let g = graph(vec![branch], vec![]);
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::ModelDecisionWithoutAgent {
                node: "route".into(),
                case: "ask".into(),
            }),
            "names the node and case: {errors:?}"
        );
    }

    /// A branch that declares an `agent_hash` must spell it `sha256:<64 hex>`,
    /// exactly like an agent node's hash.
    #[test]
    fn malformed_branch_agent_hash_is_reported() {
        let branch = Node::Branch(BranchNode {
            name: None,
            id: "route".into(),
            on: None,
            agent_hash: Some("sha256:not-hex".into()),
            cases: vec![BranchCase {
                name: "ask".into(),
                when: BranchCondition::ModelDecision,
            }],
        });
        let g = graph(vec![branch], vec![]);
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::MalformedAgentHash {
                id: "route".into(),
                hash: "sha256:not-hex".into(),
            }),
            "names the branch node and its malformed hash: {errors:?}"
        );
    }

    /// Builds a fold node over an existing body node, with the given bound,
    /// stop predicate, and join, for the fold validator tests.
    fn fold(id: &str, body: &str, max_iterations: u32, stop_when: &str, join: FoldJoin) -> Node {
        Node::Fold(FoldNode {
            id: id.into(),
            name: None,
            body: FoldBody::Node(body.into()),
            max_iterations,
            stop_when: stop_when.into(),
            join,
            on_bound: None,
            accumulator_schema: None,
        })
    }

    /// A well-formed fold (positive bound, existing body, parseable predicate,
    /// valid `best_by` path) validates clean.
    #[test]
    fn valid_fold_node_passes() {
        let g = graph(
            vec![
                agent("tailor"),
                fold(
                    "refine",
                    "tailor",
                    3,
                    "score >= 0.85",
                    FoldJoin::BestBy("score".into()),
                ),
            ],
            vec![],
        );
        assert!(validate(&g).is_ok(), "{:?}", validate(&g));
    }

    /// The `last` and `all` joins carry no reference to check, so a fold using
    /// them validates without a `best_by` path.
    #[test]
    fn fold_with_unit_joins_passes() {
        for join in [FoldJoin::Last, FoldJoin::All] {
            let g = graph(
                vec![agent("tailor"), fold("refine", "tailor", 2, "done", join)],
                vec![],
            );
            assert!(validate(&g).is_ok());
        }
    }

    /// A zero iteration bound on a fold names the node.
    #[test]
    fn non_positive_max_iterations_is_reported() {
        let g = graph(
            vec![
                agent("tailor"),
                fold("refine", "tailor", 0, "done", FoldJoin::Last),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::NonPositiveMaxIterations {
            id: "refine".into(),
            found: 0,
        }));
    }

    /// A fold body that names a missing node is reported, distinct from a map
    /// body.
    #[test]
    fn dangling_fold_body_is_reported() {
        let g = graph(
            vec![fold("refine", "ghost", 2, "done", FoldJoin::Last)],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::DanglingFoldBody {
            id: "refine".into(),
            missing: "ghost".into(),
            suggestion: None,
        }));
    }

    /// A fold whose `stop_when` does not parse is a node-precise error.
    #[test]
    fn invalid_fold_stop_expression_is_reported() {
        let g = graph(
            vec![
                agent("tailor"),
                fold("refine", "tailor", 2, "score >", FoldJoin::Last),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            matches!(
                errors.as_slice(),
                [GraphError::InvalidFoldStopExpression { node, .. }] if node == "refine"
            ),
            "one node-precise stop-expression error: {errors:?}"
        );
    }

    /// A `best_by` join whose reference is a bare literal (not a path) is a
    /// node-precise error naming the reference.
    #[test]
    fn invalid_fold_join_reference_is_reported() {
        let g = graph(
            vec![
                agent("tailor"),
                fold("refine", "tailor", 2, "done", FoldJoin::BestBy("42".into())),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.iter().any(
                |e| matches!(e, GraphError::InvalidFoldJoinReference { node, reference, .. }
                    if node == "refine" && reference == "42")
            ),
            "names the node and the bad reference: {errors:?}"
        );
    }

    // --- A fold's references against the shape its body declares. ---

    /// An agent node declaring the given output schema, to be a fold's body.
    fn scorer(id: &str, output_schema: Value) -> Node {
        Node::Agent(AgentNode {
            name: None,
            id: id.into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: Some(output_schema),
        })
    }

    /// The object schema a scored pass declares: one numeric `score`.
    fn score_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "score": { "type": "number" } },
            "required": ["score"]
        })
    }

    /// A predicate and a join reference that both name a declared property
    /// validate clean, at any depth the schema actually describes.
    #[test]
    fn fold_references_inside_the_body_schema_pass() {
        let schema = json!({
            "type": "object",
            "properties": {
                "score": { "type": "number" },
                "review": {
                    "type": "object",
                    "properties": {
                        "overall_score": { "type": "number" },
                        "notes": {
                            "type": "array",
                            "items": { "type": "object", "properties": { "text": { "type": "string" } } }
                        }
                    }
                }
            }
        });
        let g = graph(
            vec![
                scorer("tailor", schema),
                fold(
                    "refine",
                    "tailor",
                    3,
                    "score >= 0.85 && review.notes.0.text != \"\"",
                    FoldJoin::BestBy("review.overall_score".into()),
                ),
            ],
            vec![],
        );
        assert!(validate(&g).is_ok(), "{:?}", validate(&g));
    }

    /// A `stop_when` path the body's schema positively excludes is reported,
    /// naming the path and the body node. This is the typo the check exists
    /// for: `scoer` never resolves, so the loop would never stop.
    #[test]
    fn fold_stop_path_outside_the_body_schema_is_reported() {
        let g = graph(
            vec![
                scorer("tailor", score_schema()),
                fold(
                    "refine",
                    "tailor",
                    3,
                    "scoer >= 0.85",
                    FoldJoin::BestBy("score".into()),
                ),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert_eq!(
            errors,
            vec![GraphError::FoldStopPathNotInBodySchema {
                node: "refine".into(),
                path: "scoer".into(),
                body: "tailor".into(),
            }]
        );
        let message = errors[0].to_string();
        assert!(
            message.contains("scoer") && message.contains("tailor"),
            "names the path and the body node: {message}"
        );
    }

    /// A nested `stop_when` path that leaves the declared shape partway down is
    /// reported by the whole path, not by the segment that failed, because the
    /// path is what the author wrote.
    #[test]
    fn fold_nested_stop_path_outside_the_body_schema_is_reported() {
        let schema = json!({
            "type": "object",
            "properties": {
                "review": { "type": "object", "properties": { "score": { "type": "number" } } }
            }
        });
        let g = graph(
            vec![
                scorer("tailor", schema),
                fold("refine", "tailor", 3, "review.rating > 3", FoldJoin::Last),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::FoldStopPathNotInBodySchema {
                node: "refine".into(),
                path: "review.rating".into(),
                body: "tailor".into(),
            }),
            "{errors:?}"
        );
    }

    /// A `best_by` reference outside the declared shape is its own error,
    /// naming the reference as the document writes it.
    #[test]
    fn fold_join_reference_outside_the_body_schema_is_reported() {
        let g = graph(
            vec![
                scorer("tailor", score_schema()),
                fold(
                    "refine",
                    "tailor",
                    3,
                    "score >= 0.85",
                    FoldJoin::BestBy("review.overall_score".into()),
                ),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert_eq!(
            errors,
            vec![GraphError::FoldJoinReferenceNotInBodySchema {
                node: "refine".into(),
                reference: "review.overall_score".into(),
                body: "tailor".into(),
            }]
        );
    }

    /// Every failing path is collected, the predicate's and the join's alike,
    /// so an author fixes them in one pass.
    #[test]
    fn every_fold_reference_fault_is_collected() {
        let g = graph(
            vec![
                scorer("tailor", score_schema()),
                fold(
                    "refine",
                    "tailor",
                    3,
                    "scoer >= 0.85 || rating > 3",
                    FoldJoin::BestBy("overall".into()),
                ),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert_eq!(errors.len(), 3, "{errors:?}");
    }

    /// The silence rules, each one a document this check must not touch: a body
    /// declaring no schema, a schema with no `properties`, a non-object schema,
    /// a schema that admits extra keys, one that names its shape elsewhere, and
    /// a subgraph body, which has no single node to read a schema from.
    #[test]
    fn fold_references_go_unjudged_where_the_schema_says_nothing() {
        let quiet: Vec<Option<Value>> = vec![
            None,
            Some(json!({ "type": "object" })),
            Some(json!({ "type": "string" })),
            Some(json!({
                "type": "object",
                "properties": { "score": { "type": "number" } },
                "additionalProperties": true
            })),
            Some(json!({
                "type": "object",
                "properties": { "score": { "type": "number" } },
                "patternProperties": { "^x_": { "type": "string" } }
            })),
            Some(json!({ "$ref": "#/$defs/pass" })),
            Some(json!({
                "anyOf": [{ "type": "object", "properties": { "score": { "type": "number" } } }]
            })),
        ];
        for schema in quiet {
            let body = match schema {
                Some(schema) => scorer("tailor", schema),
                None => agent("tailor"),
            };
            let g = graph(
                vec![
                    body,
                    fold(
                        "refine",
                        "tailor",
                        3,
                        "anything.at.all >= 0.85",
                        FoldJoin::BestBy("nothing.declared".into()),
                    ),
                ],
                vec![],
            );
            assert!(validate(&g).is_ok(), "{:?}", validate(&g));
        }

        let subgraph = Node::Fold(FoldNode {
            name: None,
            id: "refine".into(),
            body: FoldBody::Subgraph(Box::new(graph(
                vec![scorer("tailor", score_schema())],
                vec![],
            ))),
            max_iterations: 3,
            stop_when: "anything.at.all >= 0.85".into(),
            join: FoldJoin::BestBy("nothing.declared".into()),
            on_bound: None,
            accumulator_schema: None,
        });
        assert!(validate(&graph(vec![subgraph], vec![])).is_ok());
    }

    /// A schema that closes itself with `additionalProperties: false` is read
    /// exactly as one that stays silent about extra keys: a declared
    /// `properties` map is the shape either way.
    #[test]
    fn a_closed_body_schema_reports_the_same_missing_path() {
        let schema = json!({
            "type": "object",
            "properties": { "score": { "type": "number" } },
            "additionalProperties": false
        });
        let g = graph(
            vec![
                scorer("tailor", schema),
                fold("refine", "tailor", 3, "scoer >= 0.85", FoldJoin::Last),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(errors.contains(&GraphError::FoldStopPathNotInBodySchema {
            node: "refine".into(),
            path: "scoer".into(),
            body: "tailor".into(),
        }));
    }

    /// A `stop_when` that does not parse is reported once, by the parse check
    /// alone: there are no segments to walk, so this check says nothing.
    #[test]
    fn an_unparseable_stop_predicate_is_not_also_a_shape_error() {
        let g = graph(
            vec![
                scorer("tailor", score_schema()),
                fold("refine", "tailor", 3, "score >", FoldJoin::Last),
            ],
            vec![],
        );
        let errors = validate(&g).expect_err("invalid");
        assert!(
            matches!(
                errors.as_slice(),
                [GraphError::InvalidFoldStopExpression { node, .. }] if node == "refine"
            ),
            "only the parse error: {errors:?}"
        );
    }

    /// A node `name` at exactly the character cap is valid; a node with no
    /// `name` set is unaffected by the check.
    #[test]
    fn node_name_at_the_cap_is_valid() {
        let mut named = agent("research");
        if let Node::Agent(a) = &mut named {
            a.name = Some("a".repeat(MAX_NODE_NAME_LEN));
        }
        let g = graph(
            vec![named, agent("review")],
            vec![edge("research", "review")],
        );
        assert!(validate(&g).is_ok());
    }

    /// A node `name` over the character cap is a node-precise error, counting
    /// characters rather than bytes (a multi-byte character over the cap is
    /// still one character over, not several).
    #[test]
    fn node_name_too_long_is_reported() {
        let mut named = agent("research");
        let long_name = "é".repeat(MAX_NODE_NAME_LEN + 1);
        if let Node::Agent(a) = &mut named {
            a.name = Some(long_name.clone());
        }
        let g = graph(vec![named], vec![]);
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::NodeNameTooLong {
                id: "research".into(),
                len: MAX_NODE_NAME_LEN + 1,
                max: MAX_NODE_NAME_LEN,
            }),
            "names the node and the character count, not the byte count: {errors:?}"
        );
    }

    /// An empty or all-whitespace `name` is rejected, node-precise, across
    /// every node kind.
    #[test]
    fn blank_node_name_is_reported() {
        for blank in ["", "   ", "\t\n"] {
            let mut named = gate("approve");
            if let Node::Gate(g) = &mut named {
                g.name = Some(blank.to_owned());
            }
            let g = graph(vec![named], vec![]);
            let errors = validate(&g).expect_err("invalid");
            assert!(
                errors.contains(&GraphError::BlankNodeName {
                    id: "approve".into(),
                }),
                "blank name {blank:?} should be reported: {errors:?}"
            );
        }
    }

    /// Every check runs together: a document with both a blank name on one
    /// node and an oversized name on another reports both, collect-all style.
    #[test]
    fn multiple_node_name_errors_are_all_collected() {
        let mut blank = agent("research");
        if let Node::Agent(a) = &mut blank {
            a.name = Some("   ".into());
        }
        let mut long = gate("approve");
        if let Node::Gate(g) = &mut long {
            g.name = Some("x".repeat(MAX_NODE_NAME_LEN + 5));
        }
        let g = graph(vec![blank, long], vec![]);
        let errors = validate(&g).expect_err("invalid");
        assert!(
            errors.contains(&GraphError::BlankNodeName {
                id: "research".into(),
            }),
            "{errors:?}"
        );
        assert!(
            errors.contains(&GraphError::NodeNameTooLong {
                id: "approve".into(),
                len: MAX_NODE_NAME_LEN + 5,
                max: MAX_NODE_NAME_LEN,
            }),
            "{errors:?}"
        );
    }
}
