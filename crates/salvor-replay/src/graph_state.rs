//! The per-node projection: a pure fold from a graph run's event log to the
//! progress of each node the walk has reached.
//!
//! [`crate::derive_state`] folds a log to the run-level [`crate::RunState`] —
//! one status for the whole run, the same vocabulary an agent run uses. This
//! module answers the orthogonal question a graph run raises: *where in the
//! graph is it, and what has each node done so far*. The two are deliberately
//! separate. The run-level fold stays agent-run-shaped (a graph run reads as
//! `Running` between its recorded steps); the graph shape lives here.
//!
//! # What it reads, and what it refuses to infer
//!
//! The projection is built only from the graph markers the log actually
//! records ([`Event::GraphRunStarted`], the `Node*` events, [`Event::BranchTaken`],
//! and the `Map*` events). Two invariants follow from that discipline:
//!
//! - **Skipped is not the same as not-reached.** A node the run recorded a
//!   [`Event::NodeSkipped`] for appears in [`GraphProjection::nodes`] with
//!   [`NodeState::Skipped`]: it was reached on the walk and deliberately not
//!   run. A node that appears in *no* event is simply absent from the
//!   projection. A dashboard zips the frozen document's full node list against
//!   this projection: a node the projection never mentions is "not yet
//!   reached", which is a different fact from "skipped".
//! - **The executed path comes only from [`Event::BranchTaken`].** Which case a
//!   branch fired is read from the recorded [`NodeProgress::branch_case`],
//!   never reconstructed by guessing from which sibling nodes were skipped.
//!   The recorded fact is the authority; inference is not.
//!
//! # Purity
//!
//! No IO, no clock, no randomness, and — critically — no dependency on
//! `salvor-graph`. The projection names nodes by the opaque string ids the log
//! carries; it never loads or interprets the graph document. That is what keeps
//! this crate a leaf that compiles to `wasm32-unknown-unknown`, so the browser
//! inspector can project a graph run's node progress client-side from the same
//! code the server runs.
//!
//! # Total over every prefix
//!
//! Like [`crate::derive_state`], the fold is total: it never panics and never
//! executes anything, whatever event boundary the log was cut at. Feeding it
//! growing prefixes walks the graph run's progress history, which is exactly
//! what a canvas scrubber does. Out-of-order or orphaned markers (a join with
//! no matching iteration, a marker for a node no `NodeEntered` opened) are
//! folded defensively rather than trusted to be well formed.

use serde_json::Value;

use crate::event::{Event, EventEnvelope, ForkOrigin};

/// Where one node stands, as derived from the graph markers alone.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    /// The node was entered and has not yet exited: the run's current position
    /// (or, for a map node, fanning out its iterations).
    Entered,
    /// The node exited, having produced its output.
    Exited,
    /// The node was skipped: reached on the walk but deliberately not run.
    /// Distinct from a node the projection never mentions (not yet reached).
    Skipped {
        /// The recorded reason the node was skipped.
        reason: String,
    },
}

/// One iteration of a map node's fan-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapIteration {
    /// The zero-based position of this iteration in the fanned-out list.
    pub index: u64,
    /// The derived id of the child run executing this iteration.
    pub child_run: String,
    /// Whether the iteration has joined back into the map's output. Joins are
    /// recorded in index order, so a projection at any prefix shows a
    /// contiguous run of joined iterations followed by still-running ones.
    pub joined: bool,
}

/// A map node's fan-out progress, present only once the node has fanned out.
#[derive(Debug, Clone, PartialEq)]
pub struct MapProgress {
    /// The resolved list of items the node fanned out over, recorded verbatim
    /// on [`Event::MapFannedOut`].
    pub items: Value,
    /// The iterations that have started, in the order they were recorded (which
    /// is index order for starts).
    pub iterations: Vec<MapIteration>,
}

/// Everything the log implies about one graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeProgress {
    /// The node's id, the opaque string the log carries.
    pub node: String,
    /// Where the node stands.
    pub state: NodeState,
    /// The branch case that fired here, from a recorded [`Event::BranchTaken`].
    /// `Some` only for a branch node that has routed. This is the sole
    /// authority for the executed path; it is never inferred from skips.
    pub branch_case: Option<String>,
    /// Map fan-out progress. `Some` only for a map node that has fanned out.
    pub map: Option<MapProgress>,
}

/// Everything a graph run's log prefix implies about its node-level progress.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphProjection {
    /// The hash of the graph document this run executes, from the
    /// [`Event::GraphRunStarted`] head. `None` for an empty prefix or a log
    /// that is not a graph run (an ordinary agent run projects to an empty
    /// projection).
    pub graph_hash: Option<String>,
    /// The fork link, when this run forked from another. `None` for an
    /// ordinary (non-forked) run. Carried straight through from the head.
    pub forked_from: Option<ForkOrigin>,
    /// The nodes the walk has reached, in first-mention (execution) order. A
    /// node that appears in no event is absent — "not yet reached", which the
    /// dashboard distinguishes from a [`NodeState::Skipped`] node by zipping
    /// this list against the frozen document's node list.
    pub nodes: Vec<NodeProgress>,
    /// The node currently executing: the most recent [`Event::NodeEntered`]
    /// that has not since exited. `None` between nodes and at the run's head.
    pub current_node: Option<String>,
}

impl GraphProjection {
    /// Looks up a node's recorded progress by id, or `None` if the walk has
    /// not reached it.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&NodeProgress> {
        self.nodes.iter().find(|n| n.node == id)
    }
}

/// Folds a graph run's event log into the [`GraphProjection`] it implies.
///
/// Total over every prefix of a valid log: it never panics and never executes
/// anything, whatever boundary the log was cut at. An agent run (no graph
/// markers) folds to an empty projection with `graph_hash` `None`. Later
/// events overwrite what earlier ones implied, so folding growing prefixes
/// walks the run's whole progress history.
#[must_use]
pub fn derive_graph_projection(log: &[EventEnvelope]) -> GraphProjection {
    let mut projection = GraphProjection {
        graph_hash: None,
        forked_from: None,
        nodes: Vec::new(),
        current_node: None,
    };
    for envelope in log {
        match &envelope.event {
            Event::GraphRunStarted {
                graph_hash,
                forked_from,
                ..
            } => {
                projection.graph_hash = Some(graph_hash.clone());
                projection.forked_from = forked_from.clone();
            }
            Event::NodeEntered { node } => {
                node_entry(&mut projection.nodes, node).state = NodeState::Entered;
                projection.current_node = Some(node.clone());
            }
            Event::NodeExited { node } => {
                node_entry(&mut projection.nodes, node).state = NodeState::Exited;
                if projection.current_node.as_deref() == Some(node.as_str()) {
                    projection.current_node = None;
                }
            }
            Event::NodeSkipped { node, reason } => {
                node_entry(&mut projection.nodes, node).state = NodeState::Skipped {
                    reason: reason.clone(),
                };
            }
            Event::BranchTaken { node, case } => {
                node_entry(&mut projection.nodes, node).branch_case = Some(case.clone());
            }
            Event::MapFannedOut { node, items } => {
                node_entry(&mut projection.nodes, node).map = Some(MapProgress {
                    items: items.clone(),
                    iterations: Vec::new(),
                });
            }
            Event::MapIterationStarted {
                node,
                index,
                child_run,
            } => {
                let map = map_entry(node_entry(&mut projection.nodes, node));
                map.iterations.push(MapIteration {
                    index: *index,
                    child_run: child_run.clone(),
                    joined: false,
                });
            }
            Event::MapIterationJoined { node, index } => {
                let map = map_entry(node_entry(&mut projection.nodes, node));
                if let Some(iteration) = map.iterations.iter_mut().find(|it| it.index == *index) {
                    iteration.joined = true;
                }
            }
            // Every non-graph event (the agent-run vocabulary and the
            // deterministic-context observations) is a no-op for the per-node
            // picture: those boundaries belong to the run-level fold, not this
            // one. A catch-all rather than an exhaustive list so a future
            // non-graph variant needs no change here.
            _ => {}
        }
    }
    projection
}

/// Returns a mutable reference to the node's progress, creating a fresh entry
/// in first-mention order if the walk has not touched it before.
///
/// A newly created entry defaults to [`NodeState::Entered`]: any marker that
/// mentions a node means the walk reached it, and `Entered` is the
/// least-committal "reached and active" state. Well-formed logs open a node
/// with [`Event::NodeEntered`] first, so the default is only a defensive
/// fallback for an out-of-order or orphaned marker.
fn node_entry<'a>(nodes: &'a mut Vec<NodeProgress>, id: &str) -> &'a mut NodeProgress {
    if let Some(pos) = nodes.iter().position(|n| n.node == id) {
        &mut nodes[pos]
    } else {
        nodes.push(NodeProgress {
            node: id.to_owned(),
            state: NodeState::Entered,
            branch_case: None,
            map: None,
        });
        nodes.last_mut().expect("just pushed")
    }
}

/// Returns a mutable reference to a node's map progress, creating an empty one
/// (with a null item list) if no [`Event::MapFannedOut`] preceded the
/// iteration marker. The null-item fallback keeps the fold total for an
/// out-of-order log; a well-formed graph run always fans out first.
fn map_entry(node: &mut NodeProgress) -> &mut MapProgress {
    node.map.get_or_insert_with(|| MapProgress {
        items: Value::Null,
        iterations: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{RunId, SequenceNumber};
    use time::macros::datetime;
    use uuid::Uuid;

    fn run_id() -> RunId {
        RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap())
    }

    fn log(events: Vec<Event>) -> Vec<EventEnvelope> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| {
                EventEnvelope::new(
                    run_id(),
                    SequenceNumber::new(i as u64),
                    datetime!(2026-07-14 12:00:00 UTC),
                    event,
                )
            })
            .collect()
    }

    fn graph_started() -> Event {
        Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            forked_from: None,
        }
    }

    /// An empty prefix projects to nothing: no graph hash, no nodes, no current
    /// node.
    #[test]
    fn empty_log_projects_to_empty() {
        let projection = derive_graph_projection(&[]);
        assert_eq!(projection.graph_hash, None);
        assert_eq!(projection.forked_from, None);
        assert!(projection.nodes.is_empty());
        assert_eq!(projection.current_node, None);
    }

    /// An ordinary agent run (no graph markers) projects to an empty
    /// projection: the per-node view is simply vacant for it.
    #[test]
    fn agent_run_projects_to_empty() {
        let projection = derive_graph_projection(&log(vec![
            Event::RunStarted {
                agent_def_hash: "sha256:agent".into(),
                input: serde_json::json!({}),
                labels: None,
            },
            Event::RunCompleted {
                output: serde_json::json!({"done": true}),
            },
        ]));
        assert_eq!(projection.graph_hash, None);
        assert!(projection.nodes.is_empty());
    }

    /// The head carries the graph hash and, when present, the fork origin
    /// straight into the projection.
    #[test]
    fn head_carries_graph_hash_and_fork_origin() {
        let origin = ForkOrigin {
            run_id: run_id(),
            through_seq: SequenceNumber::new(4),
            from_node: "review".into(),
            graph_hash: "sha256:graph".into(),
            acknowledged_writes: vec![2],
        };
        let projection = derive_graph_projection(&log(vec![Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({}),
            labels: None,
            forked_from: Some(origin.clone()),
        }]));
        assert_eq!(projection.graph_hash, Some("sha256:graph".to_owned()));
        assert_eq!(projection.forked_from, Some(origin));
    }

    /// Enter then exit: the node ends `Exited` and is no longer current.
    #[test]
    fn enter_then_exit_clears_current() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "research".into(),
            },
            Event::NodeExited {
                node: "research".into(),
            },
        ]));
        assert_eq!(projection.current_node, None);
        assert_eq!(
            projection.node("research").unwrap().state,
            NodeState::Exited
        );
    }

    /// While a node is entered and not yet exited it is the current node.
    #[test]
    fn entered_node_is_current() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "research".into(),
            },
        ]));
        assert_eq!(projection.current_node.as_deref(), Some("research"));
        assert_eq!(
            projection.node("research").unwrap().state,
            NodeState::Entered
        );
    }

    /// A skipped node is recorded as reached-but-skipped, distinct from a node
    /// the projection never mentions.
    #[test]
    fn skipped_is_recorded_not_absent() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeSkipped {
                node: "publish".into(),
                reason: "gate rejected".into(),
            },
        ]));
        assert_eq!(
            projection.node("publish").unwrap().state,
            NodeState::Skipped {
                reason: "gate rejected".into(),
            }
        );
        // A never-mentioned node is simply absent (not yet reached).
        assert_eq!(projection.node("archive"), None);
    }

    /// The executed path is read only from `BranchTaken`: the fired case lands
    /// on the branch node's `branch_case`, and the skip of the other route is a
    /// separate recorded fact, never the source of the routing decision.
    #[test]
    fn branch_case_comes_from_branch_taken() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "gate".into(),
            },
            Event::BranchTaken {
                node: "gate".into(),
                case: "approved".into(),
            },
            Event::NodeExited {
                node: "gate".into(),
            },
            Event::NodeSkipped {
                node: "reject".into(),
                reason: "case approved fired".into(),
            },
        ]));
        assert_eq!(
            projection.node("gate").unwrap().branch_case.as_deref(),
            Some("approved")
        );
        assert!(matches!(
            projection.node("reject").unwrap().state,
            NodeState::Skipped { .. }
        ));
    }

    /// A full map fan-out projects the resolved items and per-index iterations,
    /// with joins landing in index order.
    #[test]
    fn map_fan_out_projects_items_and_iterations() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "fanout".into(),
            },
            Event::MapFannedOut {
                node: "fanout".into(),
                items: serde_json::json!(["a", "b"]),
            },
            Event::MapIterationStarted {
                node: "fanout".into(),
                index: 0,
                child_run: "sha256:child-0".into(),
            },
            Event::MapIterationStarted {
                node: "fanout".into(),
                index: 1,
                child_run: "sha256:child-1".into(),
            },
            Event::MapIterationJoined {
                node: "fanout".into(),
                index: 0,
            },
        ]));
        let map = projection.node("fanout").unwrap().map.as_ref().unwrap();
        assert_eq!(map.items, serde_json::json!(["a", "b"]));
        assert_eq!(map.iterations.len(), 2);
        assert_eq!(
            map.iterations[0],
            MapIteration {
                index: 0,
                child_run: "sha256:child-0".into(),
                joined: true,
            }
        );
        assert_eq!(
            map.iterations[1],
            MapIteration {
                index: 1,
                child_run: "sha256:child-1".into(),
                joined: false,
            }
        );
    }

    /// Nodes appear in first-mention (execution) order regardless of which
    /// marker first named them.
    #[test]
    fn nodes_are_in_first_mention_order() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "first".into(),
            },
            Event::NodeExited {
                node: "first".into(),
            },
            Event::NodeEntered {
                node: "second".into(),
            },
            Event::NodeSkipped {
                node: "third".into(),
                reason: "x".into(),
            },
        ]));
        let ids: Vec<&str> = projection.nodes.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }

    /// Every-boundary discipline: every prefix of a rich graph-run log folds
    /// without panicking, and the projection only ever grows monotonically in
    /// the nodes it has reached (a node never disappears as the prefix extends).
    #[test]
    fn every_prefix_folds_coherently() {
        let full = log(vec![
            graph_started(),
            Event::NodeEntered {
                node: "research".into(),
            },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(2),
                request_hash: "sha256:req".into(),
                request_body: None,
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(2),
                response: serde_json::json!({"text": "hi"}),
                usage: crate::event::TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            },
            Event::NodeExited {
                node: "research".into(),
            },
            Event::NodeEntered {
                node: "gate".into(),
            },
            Event::BranchTaken {
                node: "gate".into(),
                case: "approved".into(),
            },
            Event::NodeExited {
                node: "gate".into(),
            },
            Event::NodeEntered {
                node: "fanout".into(),
            },
            Event::MapFannedOut {
                node: "fanout".into(),
                items: serde_json::json!([1, 2]),
            },
            Event::MapIterationStarted {
                node: "fanout".into(),
                index: 0,
                child_run: "sha256:c0".into(),
            },
            Event::MapIterationJoined {
                node: "fanout".into(),
                index: 0,
            },
            Event::NodeExited {
                node: "fanout".into(),
            },
            Event::RunCompleted {
                output: serde_json::json!({"done": true}),
            },
        ]);

        let mut prev_len = 0;
        for n in 0..=full.len() {
            let projection = derive_graph_projection(&full[..n]);
            // Reached-node count is monotonic non-decreasing across prefixes.
            assert!(
                projection.nodes.len() >= prev_len,
                "node set shrank at prefix {n}"
            );
            prev_len = projection.nodes.len();
        }

        // The full fold is the expected end state.
        let end = derive_graph_projection(&full);
        assert_eq!(end.current_node, None);
        assert_eq!(end.nodes.len(), 3);
        assert_eq!(
            end.node("gate").unwrap().branch_case.as_deref(),
            Some("approved")
        );
        let map = end.node("fanout").unwrap().map.as_ref().unwrap();
        assert!(map.iterations[0].joined);
    }

    /// A join with no matching iteration and a fan-out marker with no prior
    /// `MapFannedOut` are folded defensively, never panicking.
    #[test]
    fn orphaned_map_markers_are_total() {
        let projection = derive_graph_projection(&log(vec![
            graph_started(),
            // A join before any start: no iteration to mark, folded as a no-op
            // on an empty (null-item) map.
            Event::MapIterationJoined {
                node: "fanout".into(),
                index: 9,
            },
            // A start with no prior fan-out: the map is created with a null
            // item list.
            Event::MapIterationStarted {
                node: "fanout".into(),
                index: 0,
                child_run: "sha256:c".into(),
            },
        ]));
        let map = projection.node("fanout").unwrap().map.as_ref().unwrap();
        assert_eq!(map.items, Value::Null);
        assert_eq!(map.iterations.len(), 1);
        assert!(!map.iterations[0].joined);
    }
}
