//! The walk order: a deterministic topological sort of a graph's nodes.
//!
//! The engine executes nodes in dependency order — every node runs after all of
//! its edge predecessors. Where several nodes are ready at once, the smallest
//! node id wins, so the order is a pure function of the document and reproduces
//! bit for bit on replay. For a linear chain of edges the order is simply the
//! chain, but the sort handles the general DAG so branching and fan-out
//! reuse it unchanged.
//!
//! # Why a general sort for a linear chain
//!
//! A linear chain has one ready node at every step, so the tiebreak never
//! fires and the walk is trivially the chain. Writing Kahn's algorithm
//! (rather than following edges naively) means branch and map node handling
//! in the executor is all that is needed; the ordering they need is already
//! here, already deterministic, and already cycle-rejecting.

use std::collections::{BTreeSet, HashMap};

use salvor_graph::{Graph, Node};

use crate::error::EngineError;

/// Returns the graph's nodes in deterministic topological order: dependency
/// order, with the smallest node id breaking ties among ready nodes.
///
/// # Errors
///
/// [`EngineError::MalformedGraph`] when an edge references a node id not in the
/// document, or when the edges contain a cycle (so no topological order
/// exists). The document validator rejects both at submit; this is the engine's
/// defensive re-check.
pub(crate) fn walk_order(graph: &Graph) -> Result<Vec<&Node>, EngineError> {
    let mut by_id: HashMap<&str, &Node> = HashMap::with_capacity(graph.nodes.len());
    for node in &graph.nodes {
        by_id.insert(node.id(), node);
    }

    // In-degree per node, and the forward adjacency the sort walks. Both key on
    // ids that must exist; an edge to or from an unknown id is malformed.
    let mut in_degree: HashMap<&str, usize> = graph.nodes.iter().map(|n| (n.id(), 0)).collect();
    let mut successors: HashMap<&str, Vec<&str>> =
        graph.nodes.iter().map(|n| (n.id(), Vec::new())).collect();
    for edge in &graph.edges {
        if !by_id.contains_key(edge.from.as_str()) {
            return Err(EngineError::MalformedGraph {
                detail: format!("edge references unknown source node `{}`", edge.from),
            });
        }
        if !by_id.contains_key(edge.to.as_str()) {
            return Err(EngineError::MalformedGraph {
                detail: format!("edge references unknown destination node `{}`", edge.to),
            });
        }
        successors
            .get_mut(edge.from.as_str())
            .expect("source id is a known node")
            .push(edge.to.as_str());
        *in_degree
            .get_mut(edge.to.as_str())
            .expect("destination id is a known node") += 1;
    }

    // Ready set as a BTreeSet so the minimum id is always taken next: that is
    // the node-id tiebreak, and it makes the order independent of node or edge
    // declaration order.
    let mut ready: BTreeSet<&str> = in_degree
        .iter()
        .filter_map(|(id, deg)| (*deg == 0).then_some(*id))
        .collect();

    let mut order: Vec<&Node> = Vec::with_capacity(graph.nodes.len());
    while let Some(&id) = ready.iter().next() {
        ready.remove(id);
        order.push(by_id[id]);
        for &next in &successors[id] {
            let deg = in_degree.get_mut(next).expect("successor is a known node");
            *deg -= 1;
            if *deg == 0 {
                ready.insert(next);
            }
        }
    }

    if order.len() != graph.nodes.len() {
        return Err(EngineError::MalformedGraph {
            detail: "the edges contain a cycle, so no topological order exists".to_owned(),
        });
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use salvor_graph::{AgentNode, Edge, Node, SCHEMA_VERSION, ToolNode};
    use std::collections::BTreeMap;

    fn agent(id: &str) -> Node {
        Node::Agent(AgentNode {
            id: id.to_owned(),
            agent_hash: format!("sha256:{}", "1".repeat(64)),
            input_schema: None,
            output_schema: None,
        })
    }

    fn tool(id: &str) -> Node {
        Node::Tool(ToolNode {
            id: id.to_owned(),
            tool: "publish".to_owned(),
            input: BTreeMap::new(),
            input_schema: None,
            output_schema: None,
        })
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_owned(),
            to: to.to_owned(),
            label: None,
        }
    }

    fn ids(order: &[&Node]) -> Vec<String> {
        order.iter().map(|n| n.id().to_owned()).collect()
    }

    /// A linear chain sorts to the chain, whatever order the nodes are declared.
    #[test]
    fn linear_chain_sorts_to_the_chain() {
        let graph = Graph {
            schema_version: SCHEMA_VERSION,
            // Declared out of execution order on purpose.
            nodes: vec![tool("publish"), agent("research"), agent("review")],
            edges: vec![edge("research", "review"), edge("review", "publish")],
        };
        assert_eq!(
            ids(&walk_order(&graph).unwrap()),
            ["research", "review", "publish"]
        );
    }

    /// Among nodes with no dependency the smallest id is taken first: the walk
    /// is a pure function of the document, not of declaration order.
    #[test]
    fn ready_nodes_break_ties_by_id() {
        let graph = Graph {
            schema_version: SCHEMA_VERSION,
            nodes: vec![agent("zebra"), agent("alpha"), agent("mango")],
            edges: vec![],
        };
        assert_eq!(
            ids(&walk_order(&graph).unwrap()),
            ["alpha", "mango", "zebra"]
        );
    }

    /// A cycle has no topological order and is rejected, not looped over.
    #[test]
    fn a_cycle_is_rejected() {
        let graph = Graph {
            schema_version: SCHEMA_VERSION,
            nodes: vec![agent("a"), agent("b")],
            edges: vec![edge("a", "b"), edge("b", "a")],
        };
        assert!(matches!(
            walk_order(&graph),
            Err(EngineError::MalformedGraph { .. })
        ));
    }

    /// An edge to a node not in the document is malformed.
    #[test]
    fn a_dangling_edge_is_rejected() {
        let graph = Graph {
            schema_version: SCHEMA_VERSION,
            nodes: vec![agent("a")],
            edges: vec![edge("a", "ghost")],
        };
        assert!(matches!(
            walk_order(&graph),
            Err(EngineError::MalformedGraph { .. })
        ));
    }
}
