//! Fork planning: the pure computation behind fork-from-node.
//!
//! A fork is a NEW run whose log opens with the origin's prefix: every event
//! below the [`Event::NodeEntered`] the fork restarts from, rewritten under the
//! fork's own run id, with the seq-0 [`Event::GraphRunStarted`] carrying a
//! [`ForkOrigin`] as the durable record of that descent. The origin is never
//! touched: [`plan_fork`] only reads it. Everything in this module is pure; no
//! store, no clock, no randomness, so the server and the CLI share one honest
//! story for where the fork boundary sits, which writes the re-walked segment
//! would re-fire, and what bytes the child's prefix holds.
//!
//! # Node boundaries are the only legal fork points
//!
//! The boundary is the position `k` of the [`Event::NodeEntered`] for the
//! requested node. The prefix is every event with `seq < k`; the child restarts
//! by re-walking from that node. A node the origin never entered is refused
//! ([`ForkError::NodeNeverEntered`]) rather than guessed at: a fork must land on
//! a real node boundary the run actually reached, so the prefix is always a
//! faithful sub-log the child replays byte for byte before continuing live.
//!
//! # Refuse-then-record lives one layer up
//!
//! This module surfaces the write hazard set ([`ForkPlan::hazards`]): the
//! [`Effect::Write`] tool intents in the segment the fork will re-walk. Deciding
//! whether the operator acknowledged them, and recording that acknowledgement
//! into [`ForkOrigin::acknowledged_writes`], is the caller's policy (the server's
//! `409 write_replay_hazard`, the CLI's `--acknowledge-writes`). This module
//! computes the set and copies the acknowledgement into the child; it does not
//! judge it.

use salvor_core::{Effect, Event, EventEnvelope, ForkOrigin, RunId, SequenceNumber};
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;

/// A recorded [`Effect::Write`] tool intent that a fork's re-walked segment would
/// re-execute: the evidence the refuse-then-record policy shows the operator.
///
/// `seq` is the intent's position in the ORIGIN log: the number the operator
/// acknowledges and the number recorded into [`ForkOrigin::acknowledged_writes`].
#[derive(Clone, Debug)]
pub struct WriteHazard {
    /// The intent's origin log position.
    pub seq: u64,
    /// The tool the intent names.
    pub tool: String,
    /// The typed input the intent recorded.
    pub input: Value,
    /// The intent's idempotency key, if the tool declared one (a `Write`
    /// normally carries none).
    pub idempotency_key: Option<String>,
    /// When the intent was recorded, carried so the caller can render it.
    pub recorded_at: OffsetDateTime,
}

/// Why a fork cannot be planned at all, independent of write acknowledgement.
///
/// These are the structural refusals: the run is not a graph run, or the fork
/// point is not a node the origin entered. The acknowledgement refusal is not
/// here: it is the caller's policy over [`ForkPlan::hazards`].
#[derive(Debug, Error)]
pub enum ForkError {
    /// The origin's log does not open with [`Event::GraphRunStarted`], so it is
    /// an ordinary agent run and has no node boundaries to fork from.
    #[error("run is not a graph run: its log does not open with GraphRunStarted")]
    NotAGraphRun,
    /// The origin never entered the requested node (it does not exist in the
    /// graph, or the walk routed past it). A fork point must be a node the run
    /// actually reached.
    #[error(
        "the origin never entered node `{node}`; a fork point must be a node boundary the run reached"
    )]
    NodeNeverEntered {
        /// The requested node id.
        node: String,
    },
}

/// A planned fork of an origin run: the boundary, the prefix to copy, and the
/// write hazard set in the segment the fork will re-walk.
///
/// Produced by [`plan_fork`] from a read-only view of the origin log. It borrows
/// nothing from the origin afterward: the prefix envelopes are cloned, so the
/// caller can build the child ([`build_child_prefix`](Self::build_child_prefix))
/// without holding the origin log open.
#[derive(Debug)]
pub struct ForkPlan {
    origin_run: RunId,
    graph_hash: String,
    from_node: String,
    // The position of the NodeEntered the fork restarts from. The prefix is every
    // origin event with seq strictly below this.
    boundary: SequenceNumber,
    prefix: Vec<EventEnvelope>,
    hazards: Vec<WriteHazard>,
}

impl ForkPlan {
    /// The run this fork descends from.
    #[must_use]
    pub fn origin_run(&self) -> RunId {
        self.origin_run
    }

    /// The origin's recorded graph hash, which the fork reuses unchanged.
    #[must_use]
    pub fn graph_hash(&self) -> &str {
        &self.graph_hash
    }

    /// The node the fork restarts execution from.
    #[must_use]
    pub fn from_node(&self) -> &str {
        &self.from_node
    }

    /// The boundary the prefix was taken through: the highest `seq` the prefix
    /// carries, one below the restart node's [`Event::NodeEntered`]. Recorded in
    /// [`ForkOrigin::through_seq`]. The restart node's entry is always at least
    /// `seq` 1 (`seq` 0 is the head), so this never underflows.
    #[must_use]
    pub fn through_seq(&self) -> SequenceNumber {
        SequenceNumber::new(self.boundary.get() - 1)
    }

    /// How many events the child's prefix holds (the origin events with
    /// `seq < boundary`).
    #[must_use]
    pub fn prefix_len(&self) -> usize {
        self.prefix.len()
    }

    /// The [`Effect::Write`] intents the fork's re-walked segment would
    /// re-execute, in origin log order. Empty when the fork boundary sat before
    /// any write intent, which is the no-acknowledgement-needed case.
    #[must_use]
    pub fn hazards(&self) -> &[WriteHazard] {
        &self.hazards
    }

    /// The origin log positions of every hazard write, sorted ascending: the set
    /// an acknowledgement must cover, and the value recorded into
    /// [`ForkOrigin::acknowledged_writes`] when the fork proceeds.
    #[must_use]
    pub fn hazard_seqs(&self) -> Vec<u64> {
        let mut seqs: Vec<u64> = self.hazards.iter().map(|h| h.seq).collect();
        seqs.sort_unstable();
        seqs
    }

    /// Builds the child's log prefix: the origin's `[0, boundary)` events
    /// rewritten under `child`, with the seq-0 [`Event::GraphRunStarted`]
    /// carrying `forked_from`. Every other field of every envelope (`seq`,
    /// `schema_version`, `recorded_at`, and the payload) is copied verbatim, so
    /// the child's prefix is byte-identical to the origin's modulo the run id and
    /// that one seq-0 field. `acknowledged_writes` is recorded verbatim into the
    /// [`ForkOrigin`].
    #[must_use]
    pub fn build_child_prefix(
        &self,
        child: RunId,
        acknowledged_writes: Vec<u64>,
    ) -> Vec<EventEnvelope> {
        let origin = ForkOrigin {
            run_id: self.origin_run,
            through_seq: self.through_seq(),
            from_node: self.from_node.clone(),
            graph_hash: self.graph_hash.clone(),
            acknowledged_writes,
        };
        self.prefix
            .iter()
            .enumerate()
            .map(|(index, envelope)| {
                let mut event = envelope.event.clone();
                if index == 0
                    && let Event::GraphRunStarted { forked_from, .. } = &mut event
                {
                    *forked_from = Some(origin.clone());
                }
                EventEnvelope {
                    run_id: child,
                    seq: envelope.seq,
                    schema_version: envelope.schema_version,
                    recorded_at: envelope.recorded_at,
                    event,
                }
            })
            .collect()
    }
}

/// Plans a fork of `origin_log` restarting from `from_node`.
///
/// Reads the origin log only. Finds the [`Event::NodeEntered`] for `from_node`,
/// takes the prefix below it, and scans the segment at and after it for
/// [`Effect::Write`] intents. Deciding whether those hazards were acknowledged is
/// the caller's policy; see the module docs.
///
/// # Errors
///
/// [`ForkError::NotAGraphRun`] when the log does not open with
/// [`Event::GraphRunStarted`]; [`ForkError::NodeNeverEntered`] when no
/// [`Event::NodeEntered`] names `from_node`.
pub fn plan_fork(origin_log: &[EventEnvelope], from_node: &str) -> Result<ForkPlan, ForkError> {
    let graph_hash = match origin_log.first().map(|envelope| &envelope.event) {
        Some(Event::GraphRunStarted { graph_hash, .. }) => graph_hash.clone(),
        _ => return Err(ForkError::NotAGraphRun),
    };

    let boundary = origin_log
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::NodeEntered { node } if node == from_node => Some(envelope.seq),
            _ => None,
        })
        .ok_or_else(|| ForkError::NodeNeverEntered {
            node: from_node.to_owned(),
        })?;

    let prefix: Vec<EventEnvelope> = origin_log
        .iter()
        .filter(|envelope| envelope.seq < boundary)
        .cloned()
        .collect();

    // The re-walked segment is every origin event at or beyond the boundary. The
    // boundary event itself is the restart node's NodeEntered (never a write), so
    // scanning from it is equivalent to scanning strictly past it; `>=` is used
    // for a precise, self-evident "the segment the fork re-walks" definition.
    let hazards: Vec<WriteHazard> = origin_log
        .iter()
        .filter(|envelope| envelope.seq >= boundary)
        .filter_map(|envelope| match &envelope.event {
            Event::ToolCallRequested {
                tool,
                input,
                effect: Effect::Write,
                idempotency_key,
                ..
            } => Some(WriteHazard {
                seq: envelope.seq.get(),
                tool: tool.clone(),
                input: input.clone(),
                idempotency_key: idempotency_key.clone(),
                recorded_at: envelope.recorded_at,
            }),
            _ => None,
        })
        .collect();

    Ok(ForkPlan {
        origin_run: origin_log[0].run_id,
        graph_hash,
        from_node: from_node.to_owned(),
        boundary,
        prefix,
        hazards,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;

    fn run(tag: u8) -> RunId {
        let mut bytes = [0u8; 16];
        bytes[15] = tag;
        bytes[6] = 0x40;
        bytes[8] = 0x80;
        RunId::from_uuid(Uuid::from_bytes(bytes))
    }

    fn envelope(run_id: RunId, seq: u64, event: Event) -> EventEnvelope {
        EventEnvelope::new(
            run_id,
            SequenceNumber::new(seq),
            datetime!(2026-07-14 12:00:00 UTC),
            event,
        )
    }

    // research (agent) -> publish (write tool). The log: head, research
    // entered/exited, publish entered + a Write intent + completion + exited,
    // terminal. Forking from `publish` re-walks the write.
    fn origin_log() -> Vec<EventEnvelope> {
        let r = run(1);
        vec![
            envelope(
                r,
                0,
                Event::GraphRunStarted {
                    graph_hash: "sha256:graph".into(),
                    input: json!({"topic": "otters"}),
                    labels: None,
                    forked_from: None,
                },
            ),
            envelope(
                r,
                1,
                Event::NodeEntered {
                    node: "research".into(),
                },
            ),
            envelope(
                r,
                2,
                Event::NodeExited {
                    node: "research".into(),
                },
            ),
            envelope(
                r,
                3,
                Event::NodeEntered {
                    node: "publish".into(),
                },
            ),
            envelope(
                r,
                4,
                Event::ToolCallRequested {
                    seq: SequenceNumber::new(4),
                    tool: "http_post".into(),
                    input: json!({"body": "draft"}),
                    effect: Effect::Write,
                    idempotency_key: None,
                    performed_by: None,
                },
            ),
            envelope(
                r,
                5,
                Event::ToolCallCompleted {
                    seq: SequenceNumber::new(4),
                    output: json!({"ok": true}),
                    deduplicated_from: None,
                },
            ),
            envelope(
                r,
                6,
                Event::NodeExited {
                    node: "publish".into(),
                },
            ),
            envelope(
                r,
                7,
                Event::RunCompleted {
                    output: json!(null),
                },
            ),
        ]
    }

    #[test]
    fn a_non_graph_run_is_refused() {
        let log = vec![envelope(
            run(1),
            0,
            Event::RunStarted {
                agent_def_hash: "sha256:a".into(),
                input: json!(null),
                labels: None,
            },
        )];
        assert!(matches!(
            plan_fork(&log, "anything"),
            Err(ForkError::NotAGraphRun)
        ));
    }

    #[test]
    fn a_node_the_run_never_entered_is_refused() {
        let error = plan_fork(&origin_log(), "ghost").expect_err("ghost was never entered");
        match error {
            ForkError::NodeNeverEntered { node } => assert_eq!(node, "ghost"),
            other => panic!("expected NodeNeverEntered, got {other:?}"),
        }
    }

    #[test]
    fn forking_before_the_write_finds_no_hazard() {
        // Fork from research: the prefix is [head, research-entered] and the
        // re-walked segment (research-exited onward) still contains the write,
        // so research's fork DOES see the downstream write as a hazard.
        let plan = plan_fork(&origin_log(), "research").expect("research was entered");
        assert_eq!(plan.through_seq().get(), 0, "prefix is just the head");
        assert_eq!(plan.prefix_len(), 1);
        assert_eq!(
            plan.hazard_seqs(),
            vec![4],
            "the downstream write is still in the re-walked segment"
        );
    }

    #[test]
    fn forking_at_the_write_node_lists_the_write() {
        let plan = plan_fork(&origin_log(), "publish").expect("publish was entered");
        assert_eq!(
            plan.through_seq().get(),
            2,
            "prefix is through research-exited"
        );
        assert_eq!(plan.prefix_len(), 3, "head + research entered + exited");
        let hazards = plan.hazards();
        assert_eq!(hazards.len(), 1);
        assert_eq!(hazards[0].seq, 4);
        assert_eq!(hazards[0].tool, "http_post");
        assert_eq!(hazards[0].input, json!({"body": "draft"}));
    }

    #[test]
    fn the_child_prefix_is_byte_identical_modulo_run_id_and_forked_from() {
        let origin = origin_log();
        let plan = plan_fork(&origin, "publish").expect("publish was entered");
        let child = run(2);
        let child_prefix = plan.build_child_prefix(child, plan.hazard_seqs());

        // Every child envelope carries the child run id and the origin's seq.
        for (index, env) in child_prefix.iter().enumerate() {
            assert_eq!(env.run_id, child);
            assert_eq!(env.seq, origin[index].seq);
            assert_eq!(env.recorded_at, origin[index].recorded_at);
        }

        // The seq-0 head carries the fork origin; nothing else does.
        match &child_prefix[0].event {
            Event::GraphRunStarted { forked_from, .. } => {
                let origin_link = forked_from.as_ref().expect("head carries the fork origin");
                assert_eq!(origin_link.run_id, run(1));
                assert_eq!(origin_link.through_seq.get(), 2);
                assert_eq!(origin_link.from_node, "publish");
                assert_eq!(origin_link.graph_hash, "sha256:graph");
                assert_eq!(origin_link.acknowledged_writes, vec![4]);
            }
            other => panic!("head is not GraphRunStarted: {other:?}"),
        }

        // The byte-identical proof, constructed explicitly: rewrite the parent
        // prefix under the child id, blank the seq-0 forked_from, and compare
        // serialized bytes to the child prefix with its forked_from blanked.
        let blank = |envelopes: &[EventEnvelope]| -> String {
            let mut copy: Vec<EventEnvelope> = envelopes.to_vec();
            for env in &mut copy {
                env.run_id = child;
            }
            if let Event::GraphRunStarted { forked_from, .. } = &mut copy[0].event {
                *forked_from = None;
            }
            serde_json::to_string(&copy).unwrap()
        };
        let parent_prefix: Vec<EventEnvelope> = origin[..3].to_vec();
        let mut child_blanked = child_prefix.clone();
        if let Event::GraphRunStarted { forked_from, .. } = &mut child_blanked[0].event {
            *forked_from = None;
        }
        assert_eq!(
            blank(&parent_prefix),
            serde_json::to_string(&child_blanked).unwrap(),
            "child prefix is byte-identical to the parent's modulo run id and forked_from"
        );
    }
}
