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

use crate::document::{BranchCondition, Graph, MapBody, Node, SCHEMA_VERSION};
use crate::expr;

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
    check_branch_expressions(graph, &mut errors);
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
    }
}

/// Per-node required-field checks: an agent hash is well-formed, a map cap is
/// positive, a gate's approval schema is an object. Each rule is a small,
/// independent block so a rule can be relaxed on its own.
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
            // Tool and branch carry no field rule beyond the strict parse.
            Node::Tool(_) | Node::Branch(_) => {}
        }
    }
}

/// Every `branch` case whose condition is an expression must parse in the
/// [`crate::expr`] condition language.
///
/// This is where the opaque expression string earns its meaning: the language
/// is parsed AT SUBMIT, so a malformed condition is a node-precise error the
/// author sees now, never a run-time failure inside a durable, replayed run.
/// Each bad expression is one collected error naming the node and the case;
/// `model_decision` conditions carry no expression and are skipped.
fn check_branch_expressions(graph: &Graph, errors: &mut Vec<GraphError>) {
    for node in &graph.nodes {
        let Node::Branch(branch) = node else {
            continue;
        };
        for case in &branch.cases {
            if let BranchCondition::Expression(source) = &case.when
                && let Err(error) = expr::parse(source)
            {
                errors.push(GraphError::InvalidBranchExpression {
                    node: branch.id.clone(),
                    case: case.name.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
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
        AgentNode, BranchCase, BranchCondition, BranchNode, Edge, GateNode, MapBody, MapNode,
        ToolNode,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn hash() -> String {
        format!("sha256:{}", "a".repeat(64))
    }

    fn agent(id: &str) -> Node {
        Node::Agent(AgentNode {
            id: id.into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: None,
        })
    }

    fn gate(id: &str) -> Node {
        Node::Gate(GateNode {
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

    /// A map body that names a missing node is reported.
    #[test]
    fn dangling_map_body_is_reported() {
        let g = graph(
            vec![Node::Map(MapNode {
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
            id: "producer".into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: Some(json!({"type": "string"})),
        });
        let consumer = Node::Tool(ToolNode {
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
            id: "producer".into(),
            agent_hash: hash(),
            input_schema: None,
            output_schema: Some(json!({"type": "string"})),
        });
        let consumer = Node::Tool(ToolNode {
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
            id: "route".into(),
            on: Some("score".into()),
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
        let g = graph(vec![agent("score"), branch], vec![edge("score", "route")]);
        assert!(validate(&g).is_ok());
    }

    /// A branch case whose expression does not parse is a node-precise error
    /// naming the node and the case; a sibling `model_decision` case is skipped.
    #[test]
    fn invalid_branch_expression_is_reported() {
        let branch = Node::Branch(BranchNode {
            id: "route".into(),
            on: None,
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
        let g = graph(vec![branch], vec![]);
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
}
