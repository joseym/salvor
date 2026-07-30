//! The replay cursor: a pure state machine that answers every operation
//! orchestration requests either from recorded history or with a typed
//! permission to execute live.
//!
//! On resume, the runtime replays a run's event log. While recorded history
//! remains, each request is matched against the log and answered from it; the
//! call is never executed again. The first request the log does not cover
//! flips the cursor into live mode, where the cursor hands back a permit: a
//! value that says "execute this yourself, then record the result through
//! me". The recorded/live distinction is encoded in [`Outcome`], so code that
//! only holds a replayed value has no way to execute anything, and code that
//! wants to execute must first be handed a permit by the cursor.
//!
//! # Purity
//!
//! Nothing in this module performs IO, reads a clock, or draws randomness.
//! Recorded values come from the log passed to [`ReplayCursor::new`]; live
//! values are supplied by the caller when a permit is redeemed. The cursor
//! never touches `salvor-store` or any executor: it consumes an in-memory
//! `Vec<EventEnvelope>` and emits [`Emitted`] values the caller persists.
//! This is deliberate. It is why the cursor lives in the pure `salvor-replay`
//! crate, which builds for wasm32 so the v0.3 browser inspector can drive
//! replay client-side from the same code the runtime uses.
//!
//! # Divergence detection is always on
//!
//! Replay is only sound if orchestration is deterministic, so every match
//! against the log compares the full request payload (tool name, input,
//! effect, idempotency key, request hash) with what was recorded, and any
//! mismatch fails with [`ReplayError::Divergence`] naming the position, what
//! the log recorded, and what orchestration produced. The comparison is plain
//! equality on values already in memory, which costs microseconds against
//! model calls that cost seconds; gating it behind a debug flag would trade
//! that trivial cost for silently corrupted resumes. So there is no flag.
//!
//! # How `RunCtx` sits on this seam
//!
//! `RunCtx` will own three things the cursor refuses to own: an event store,
//! executors (model client, tool dispatch), and the ambient clock/RNG. Each
//! `RunCtx` method is a thin wrapper over one cursor request:
//!
//! - `ctx.now()` calls [`ReplayCursor::now`]. Replayed: return the value.
//!   Live: read the real clock, redeem the permit, persist the emitted event,
//!   return the reading.
//! - `ctx.random()` does the same over [`ReplayCursor::random`].
//! - A recorded model call maps to [`ReplayCursor::model_call`]. Live: persist
//!   the permit's intent event, call the provider, redeem, persist.
//! - Recorded tool dispatch maps to [`ReplayCursor::tool_call`]. Live: persist
//!   the intent event **before** executing (that write-ahead is what makes a
//!   crashed `Write` detectable), execute, redeem, persist.
//! - Suspension maps to [`ReplayCursor::suspend`] plus
//!   [`ReplayCursor::await_resume`]; a live [`Parked`] value means the run
//!   parks durably and the process may exit.
//! - Budget enforcement between events maps to
//!   [`ReplayCursor::budget_exceeded`] plus [`ReplayCursor::await_resume`].
//!   Budget checks must be computed from replayed data (recorded usage,
//!   recorded timestamps), so that a check that fired live fires again at the
//!   same position on replay.
//!
//! The built-in agent loop consumes only this same public surface.

use core::fmt;
use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;

use crate::effect::Effect;
use crate::event::{Budget, Event, EventEnvelope, ForkOrigin, TokenUsage};
use crate::id::{RunId, SequenceNumber};

/// How the cursor answered a request: from recorded history, or with a
/// permission to execute live.
///
/// This enum is the type-level encoding of the replay rule. A `Replayed`
/// value carries the recorded result and nothing else, so holding one gives
/// no way to execute. A `Live` value carries a permit (or, for operations
/// with no result, the [`Emitted`] event to persist), and permits are the
/// only path to recording a fresh result.
#[derive(Debug)]
pub enum Outcome<T, P> {
    /// The log already holds the answer. Use it; execute nothing.
    Replayed(T),
    /// History is exhausted at this operation. Execute it and record the
    /// result through the carried permit.
    Live(P),
}

/// An event the cursor produced in live mode, with the log position it must
/// occupy.
///
/// The cursor cannot persist anything and cannot read a clock, so the caller
/// wraps this in an [`EventEnvelope`] (supplying the run id and timestamp at
/// the IO edge) and appends it to durable storage.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// The log position this event must be appended at.
    pub seq: SequenceNumber,
    /// The event payload to persist.
    pub event: Event,
}

/// A replayed or freshly executed model call result.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    /// The model response.
    pub response: Value,
    /// Token usage reported for the call.
    pub usage: TokenUsage,
}

/// What the log holds at a divergence position.
#[derive(Debug, Clone, PartialEq)]
pub enum LoggedStep {
    /// The recorded event orchestration's request failed to match.
    Event(Event),
    /// The log ended with this terminal event, but orchestration kept
    /// producing steps. Continuing live here would execute steps a completed
    /// run never took, so it is refused.
    RunAlreadyTerminal(Event),
}

/// What orchestration produced at a divergence position.
///
/// One variant per cursor request, carrying the request's identifying
/// payload, so a divergence error can show both sides of the mismatch.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestedStep {
    /// [`ReplayCursor::begin`] with this agent definition hash.
    Begin {
        /// The hash orchestration presented.
        agent_def_hash: String,
    },
    /// [`ReplayCursor::begin_graph`] with this graph document hash.
    BeginGraph {
        /// The graph hash orchestration presented.
        graph_hash: String,
    },
    /// [`ReplayCursor::node_entered`] for this node.
    NodeEntered {
        /// The node id orchestration presented.
        node: String,
    },
    /// [`ReplayCursor::node_exited`] for this node.
    NodeExited {
        /// The node id orchestration presented.
        node: String,
    },
    /// [`ReplayCursor::node_skipped`] for this node.
    NodeSkipped {
        /// The node id orchestration presented.
        node: String,
        /// The skip reason orchestration presented.
        reason: String,
    },
    /// [`ReplayCursor::branch_taken`] for this branch node.
    BranchTaken {
        /// The branch node id orchestration presented.
        node: String,
        /// The case name orchestration presented.
        case: String,
    },
    /// [`ReplayCursor::map_fanned_out`] for this map node.
    MapFannedOut {
        /// The map node id orchestration presented.
        node: String,
        /// The resolved item list orchestration presented.
        items: Value,
    },
    /// [`ReplayCursor::map_iteration_started`] for this map node and index.
    MapIterationStarted {
        /// The map node id orchestration presented.
        node: String,
        /// The zero-based iteration index orchestration presented.
        index: u64,
        /// The derived child run id orchestration presented.
        child_run: String,
    },
    /// [`ReplayCursor::map_iteration_joined`] for this map node and index.
    MapIterationJoined {
        /// The map node id orchestration presented.
        node: String,
        /// The zero-based iteration index orchestration presented.
        index: u64,
    },
    /// [`ReplayCursor::now`].
    Now,
    /// [`ReplayCursor::random`].
    Random,
    /// [`ReplayCursor::model_call`] with this request hash.
    ModelCall {
        /// The request hash orchestration presented.
        request_hash: String,
    },
    /// [`ReplayCursor::tool_call`] with these parameters.
    ToolCall {
        /// The tool name orchestration presented.
        tool: String,
        /// The input orchestration presented.
        input: Value,
        /// The effect class orchestration presented.
        effect: Effect,
        /// The idempotency key orchestration presented.
        idempotency_key: Option<String>,
    },
    /// [`ReplayCursor::suspend`] with these parameters.
    Suspend {
        /// The suspension reason orchestration presented.
        reason: String,
        /// The resume-input schema orchestration presented.
        input_schema: Value,
    },
    /// [`ReplayCursor::await_resume`].
    AwaitResume,
    /// [`ReplayCursor::budget_exceeded`] with these parameters.
    BudgetExceeded {
        /// The budget the runtime reported as crossed.
        budget: Budget,
        /// The observed value the runtime reported.
        observed: f64,
    },
    /// [`ReplayCursor::complete_run`] with this output.
    CompleteRun {
        /// The final output orchestration produced.
        output: Value,
    },
    /// [`ReplayCursor::fail_run`] with this error.
    FailRun {
        /// The failure description orchestration produced.
        error: String,
    },
}

/// Why replay could not continue.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ReplayError {
    /// Orchestration produced a step that does not match the recorded event
    /// at this position. Replay assumes deterministic orchestration; this
    /// error is that assumption failing loudly instead of silently resuming
    /// a different run than the one recorded.
    #[error(
        "replay divergence at seq {position}: log recorded {recorded}, orchestration requested {requested}"
    )]
    Divergence {
        /// The log position where the mismatch occurred. For a request made
        /// after the recorded run already ended, this is the position the
        /// extra step would have occupied.
        position: SequenceNumber,
        /// What the log holds at that position. Boxed to keep the error type
        /// small on the happy path; it carries a full event.
        recorded: Box<LoggedStep>,
        /// What orchestration produced instead. Boxed for the same reason.
        requested: Box<RequestedStep>,
    },
    /// The log records a [`Effect::Write`] intent with no completion. The
    /// write may or may not have reached the provider, so the runtime refuses
    /// to guess: no retry, no re-execution, a human resolves it. The recorded
    /// intent is carried in full as the evidence that human needs.
    #[error(
        "write intent at seq {position} (tool `{tool}`) has no recorded completion; needs human reconciliation"
    )]
    NeedsReconciliation {
        /// The log position of the dangling write intent.
        position: SequenceNumber,
        /// The tool the intent named.
        tool: String,
        /// The input the intent recorded.
        input: Value,
        /// The idempotency key the intent recorded, if any.
        idempotency_key: Option<String>,
    },
    /// The log itself is not a well-formed run history (non-contiguous
    /// positions, mixed run ids, a completion that does not follow its
    /// intent, events after a terminal event). This is storage corruption or
    /// a writer bug, not orchestration divergence.
    #[error("malformed event log at seq {position}: {detail}")]
    MalformedLog {
        /// The log position where the malformation was noticed.
        position: SequenceNumber,
        /// What was wrong.
        detail: String,
    },
}

/// The pure replay/live cursor over one run's event log.
///
/// Construct it with the recorded log (possibly empty, for a fresh run) and
/// drive it with one request per orchestration step. Each request returns an
/// [`Outcome`]: [`Outcome::Replayed`] while history covers the step,
/// [`Outcome::Live`] from the first uncovered step onward. See the module
/// docs for the full protocol and how `RunCtx` wraps it.
#[derive(Debug)]
pub struct ReplayCursor {
    log: Vec<EventEnvelope>,
    /// Index into `log` of the next unconsumed recorded event.
    pos: usize,
    /// The position the next live emission will occupy.
    next_live_seq: SequenceNumber,
    /// Set once a terminal event has been consumed or emitted; any request
    /// after that is a divergence.
    terminal: Option<Event>,
}

impl ReplayCursor {
    /// Builds a cursor over a run's recorded log.
    ///
    /// An empty log is a fresh run: every request will be live. A non-empty
    /// log must be well formed: it starts with a run head ([`Event::RunStarted`]
    /// for an agent run or [`Event::GraphRunStarted`] for a graph run) at
    /// position 0, all envelopes share one run id, positions are contiguous,
    /// and nothing follows a terminal event.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::MalformedLog`] when the log violates any of
    /// those shape rules.
    pub fn new(log: Vec<EventEnvelope>) -> Result<Self, ReplayError> {
        if let Some(first) = log.first() {
            // A log may open with an agent run's `RunStarted` or a graph run's
            // `GraphRunStarted`; both are legal run heads. This is the sole
            // wire-adjacent change the graph events require of the cursor.
            if !matches!(
                first.event,
                Event::RunStarted { .. } | Event::GraphRunStarted { .. }
            ) {
                return Err(ReplayError::MalformedLog {
                    position: first.seq,
                    detail: format!(
                        "a run log must start with RunStarted or GraphRunStarted, found {}",
                        kind_name(&first.event)
                    ),
                });
            }
            if first.seq != SequenceNumber::new(0) {
                return Err(ReplayError::MalformedLog {
                    position: first.seq,
                    detail: format!(
                        "a run log must start at seq 0, found seq {}; the head of the log is missing",
                        first.seq
                    ),
                });
            }
        }
        for pair in log.windows(2) {
            let (prev, next) = (&pair[0], &pair[1]);
            if next.seq != prev.seq.next() {
                return Err(ReplayError::MalformedLog {
                    position: next.seq,
                    detail: format!("non-contiguous position after seq {}", prev.seq),
                });
            }
            if next.run_id != prev.run_id {
                return Err(ReplayError::MalformedLog {
                    position: next.seq,
                    detail: "mixed run ids in one log".to_owned(),
                });
            }
            if matches!(
                prev.event,
                Event::RunCompleted { .. } | Event::RunFailed { .. } | Event::RunAbandoned { .. }
            ) {
                return Err(ReplayError::MalformedLog {
                    position: next.seq,
                    detail: format!("event recorded after terminal {}", kind_name(&prev.event)),
                });
            }
        }
        let next_live_seq = log
            .last()
            .map_or(SequenceNumber::new(0), |last| last.seq.next());
        Ok(Self {
            log,
            pos: 0,
            next_live_seq,
            terminal: None,
        })
    }

    /// The run id the log carries, or `None` for a fresh (empty) log.
    #[must_use]
    pub fn run_id(&self) -> Option<RunId> {
        self.log.first().map(|env| env.run_id)
    }

    /// Whether recorded history remains to be consumed.
    ///
    /// `true` means the next request will be answered from the log (or fail
    /// as a divergence); `false` means the cursor has handed off to live
    /// mode.
    #[must_use]
    pub fn is_replaying(&self) -> bool {
        self.pos < self.log.len()
    }

    /// Whether a terminal event ([`Event::RunCompleted`] or
    /// [`Event::RunFailed`]) has been consumed or emitted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.terminal.is_some()
    }

    /// The position the next consumed or emitted event occupies.
    #[must_use]
    pub fn next_seq(&self) -> SequenceNumber {
        if self.pos < self.log.len() {
            self.log[self.pos].seq
        } else {
            self.next_live_seq
        }
    }

    /// Requests the start of the run.
    ///
    /// Replayed: verifies `agent_def_hash` against the recorded
    /// [`Event::RunStarted`] (a changed agent definition must not silently
    /// resume an old run) and returns the recorded input. Live: returns a
    /// [`BeginPermit`] the caller redeems with the run's input.
    ///
    /// `labels` is the optional correlation tags to stamp on a genuinely
    /// fresh [`Event::RunStarted`]; see the field's docs. It plays no part in
    /// replay: a recorded `RunStarted` is matched on `agent_def_hash` alone,
    /// and whatever `labels` the caller passes here is ignored on that path,
    /// exactly as [`ReplayCursor::model_call`]'s `request_body` is ignored
    /// when replaying. Bounds on `labels` are not checked here (see the
    /// field's docs for why); a caller enforces them before calling.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a hash mismatch, a different recorded
    /// event, or a request after the run already ended.
    pub fn begin(
        &mut self,
        agent_def_hash: &str,
        labels: Option<BTreeMap<String, String>>,
    ) -> Result<Outcome<Value, BeginPermit<'_>>, ReplayError> {
        let requested = RequestedStep::Begin {
            agent_def_hash: agent_def_hash.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            // The passed-in `labels` plays no part in this match: only the
            // hash is compared, and the recorded event (labels included)
            // wins, exactly as the recorded `input` does below.
            if let Event::RunStarted {
                agent_def_hash: recorded_hash,
                input,
                ..
            } = &self.log[self.pos].event
                && recorded_hash == agent_def_hash
            {
                let input = input.clone();
                self.pos += 1;
                return Ok(Outcome::Replayed(input));
            }
            return Err(self.mismatch(requested));
        }
        Ok(Outcome::Live(BeginPermit {
            agent_def_hash: agent_def_hash.to_owned(),
            labels,
            cursor: self,
        }))
    }

    /// Requests the start of a graph run.
    ///
    /// The graph-run counterpart of [`begin`](ReplayCursor::begin): the log
    /// opens with [`Event::GraphRunStarted`] rather than [`Event::RunStarted`],
    /// because a graph run coordinates many agent hashes and has none at its
    /// head. Replayed: verifies `graph_hash` against the recorded head (a
    /// changed graph document must not silently resume an old run) and returns
    /// the recorded input. Live: returns a [`GraphBeginPermit`] the caller
    /// redeems with the run's input.
    ///
    /// `labels` and `forked_from` land on a genuinely fresh
    /// [`Event::GraphRunStarted`]; they play no part in replay, exactly as
    /// [`begin`](ReplayCursor::begin)'s `labels` do. Bounds on `labels` are not
    /// checked here; a caller enforces them before calling.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a hash mismatch, a different recorded
    /// event, or a request after the run already ended.
    pub fn begin_graph(
        &mut self,
        graph_hash: &str,
        labels: Option<BTreeMap<String, String>>,
        forked_from: Option<ForkOrigin>,
    ) -> Result<Outcome<Value, GraphBeginPermit<'_>>, ReplayError> {
        let requested = RequestedStep::BeginGraph {
            graph_hash: graph_hash.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            // Only the graph hash is compared; the recorded event (labels and
            // fork origin included) wins, exactly as the recorded input does.
            if let Event::GraphRunStarted {
                graph_hash: recorded_hash,
                input,
                ..
            } = &self.log[self.pos].event
                && recorded_hash == graph_hash
            {
                let input = input.clone();
                self.pos += 1;
                return Ok(Outcome::Replayed(input));
            }
            return Err(self.mismatch(requested));
        }
        Ok(Outcome::Live(GraphBeginPermit {
            graph_hash: graph_hash.to_owned(),
            labels,
            forked_from,
            cursor: self,
        }))
    }

    /// Requests entry into a graph node.
    ///
    /// Replayed: matches the recorded [`Event::NodeEntered`] for `node`. Live:
    /// returns the event to persist. A graph node's own events (an agent loop's
    /// model calls, a tool call) are recorded between this and the matching
    /// [`node_exited`](ReplayCursor::node_exited).
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id mismatch, a different recorded
    /// event, or a request after the run already ended.
    pub fn node_entered(&mut self, node: &str) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::NodeEntered {
            node: node.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::NodeEntered { node: recorded } = &self.log[self.pos].event
                && recorded == node
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::NodeEntered {
            node: node.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Requests exit from a graph node, having produced its output.
    ///
    /// Replayed: matches the recorded [`Event::NodeExited`] for `node`. Live:
    /// returns the event to persist. The counterpart of
    /// [`node_entered`](ReplayCursor::node_entered).
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id mismatch, a different recorded
    /// event, or a request after the run already ended.
    pub fn node_exited(&mut self, node: &str) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::NodeExited {
            node: node.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::NodeExited { node: recorded } = &self.log[self.pos].event
                && recorded == node
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::NodeExited {
            node: node.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Records (or replays) that a graph node was skipped: reached on the walk
    /// but deliberately not run (a branch routed past it). No
    /// [`node_entered`](ReplayCursor::node_entered) precedes a skip; the skip is
    /// the sole marker for the node.
    ///
    /// Replayed: matches the recorded [`Event::NodeSkipped`] for `node` (reason
    /// included, so a skip that recorded one reason live cannot replay under
    /// another). Live: returns the event to persist. The `reason` a caller
    /// passes must be a pure function of the document and recorded values, like
    /// every other emitted payload, so it reproduces on replay.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id or reason mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn node_skipped(
        &mut self,
        node: &str,
        reason: &str,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::NodeSkipped {
            node: node.to_owned(),
            reason: reason.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::NodeSkipped {
                node: recorded_node,
                reason: recorded_reason,
            } = &self.log[self.pos].event
                && recorded_node == node
                && recorded_reason == reason
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::NodeSkipped {
            node: node.to_owned(),
            reason: reason.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Records (or replays) that a branch node routed: the named `case` fired.
    /// This is the sole recorded authority for which way a branch went; the
    /// executed path is read from these events, never inferred from skips.
    ///
    /// Recorded between the branch's [`node_entered`](ReplayCursor::node_entered)
    /// and [`node_exited`](ReplayCursor::node_exited). Replayed: matches the
    /// recorded [`Event::BranchTaken`] for `node` and `case` (a branch that fired
    /// one case live cannot replay having fired another). Live: returns the event
    /// to persist. The chosen `case` must be a deterministic function of recorded
    /// values (a pure expression over the routed value, or a decision recomputed
    /// from a replayed model reply), so replay reproduces the same route.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id or case mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn branch_taken(
        &mut self,
        node: &str,
        case: &str,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::BranchTaken {
            node: node.to_owned(),
            case: case.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::BranchTaken {
                node: recorded_node,
                case: recorded_case,
            } = &self.log[self.pos].event
                && recorded_node == node
                && recorded_case == case
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::BranchTaken {
            node: node.to_owned(),
            case: case.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Records (or replays) that a map node fanned out over a resolved item list.
    ///
    /// Recorded between the map node's [`node_entered`](ReplayCursor::node_entered)
    /// and its per-iteration markers. Replayed: matches the recorded
    /// [`Event::MapFannedOut`] for `node` and `items` (the resolved list is part of
    /// the recorded fact, so a fan-out that recorded one list live cannot replay
    /// under another). Live: returns the event to persist. The `items` a caller
    /// passes must be a deterministic function of recorded values (the `over`
    /// reference resolved against the recorded routed value) so replay reproduces
    /// the identical fan-out, which is what makes the derived per-iteration child
    /// ids reproducible.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id or item-list mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn map_fanned_out(
        &mut self,
        node: &str,
        items: &Value,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::MapFannedOut {
            node: node.to_owned(),
            items: items.clone(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::MapFannedOut {
                node: recorded_node,
                items: recorded_items,
            } = &self.log[self.pos].event
                && recorded_node == node
                && recorded_items == items
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::MapFannedOut {
            node: node.to_owned(),
            items: items.clone(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Records (or replays) that one iteration of a map fan-out started, as a child
    /// run with the derived id `child_run`.
    ///
    /// Replayed: matches the recorded [`Event::MapIterationStarted`] on its
    /// STRUCTURAL position (`node` and `index`) and the RECORDED `child_run`
    /// wins, exactly as the recorded input wins in [`begin`](ReplayCursor::begin).
    /// The passed `child_run` is compared only loosely (it is derived data), for a
    /// deliberate reason: the id is `sha256:` over the parent run id, the node, and
    /// the index, and a FORK replays the origin's prefix under a NEW run id, so a
    /// caller re-deriving the id there produces a different value than the origin
    /// recorded. Trusting the recorded id keeps a fork's inherited map markers
    /// replayable while `node`/`index` still pin the structural position. Live:
    /// returns the event to persist with the caller's derived `child_run`.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id or index mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn map_iteration_started(
        &mut self,
        node: &str,
        index: u64,
        child_run: &str,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::MapIterationStarted {
            node: node.to_owned(),
            index,
            child_run: child_run.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::MapIterationStarted {
                node: recorded_node,
                index: recorded_index,
                ..
            } = &self.log[self.pos].event
                && recorded_node == node
                && *recorded_index == index
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::MapIterationStarted {
            node: node.to_owned(),
            index,
            child_run: child_run.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Records (or replays) that one iteration of a map fan-out joined back into
    /// the map node's output.
    ///
    /// Joins are recorded in index order, never completion order, so the
    /// concurrency of the fan-out never influences the parent log's byte sequence.
    /// Replayed: matches the recorded [`Event::MapIterationJoined`] for `node` and
    /// `index`. Live: returns the event to persist.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a node-id or index mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn map_iteration_joined(
        &mut self,
        node: &str,
        index: u64,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::MapIterationJoined {
            node: node.to_owned(),
            index,
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::MapIterationJoined {
                node: recorded_node,
                index: recorded_index,
            } = &self.log[self.pos].event
                && recorded_node == node
                && *recorded_index == index
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::MapIterationJoined {
            node: node.to_owned(),
            index,
        });
        Ok(Outcome::Live(emitted))
    }

    /// Requests the current time.
    ///
    /// Replayed: returns the recorded instant, bit for bit. Live: returns a
    /// [`NowPermit`]; the caller reads a real clock and redeems the permit
    /// with the reading.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] when the log records a different kind of
    /// event here, or the run already ended.
    pub fn now(&mut self) -> Result<Outcome<OffsetDateTime, NowPermit<'_>>, ReplayError> {
        let requested = RequestedStep::Now;
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::NowObserved { now } = self.log[self.pos].event {
                self.pos += 1;
                return Ok(Outcome::Replayed(now));
            }
            return Err(self.mismatch(requested));
        }
        Ok(Outcome::Live(NowPermit { cursor: self }))
    }

    /// Requests random bits.
    ///
    /// Replayed: returns the recorded bits, exactly. Live: returns a
    /// [`RandomPermit`]; the caller draws from a real random source and
    /// redeems the permit with the draw.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] when the log records a different kind of
    /// event here, or the run already ended.
    pub fn random(&mut self) -> Result<Outcome<u64, RandomPermit<'_>>, ReplayError> {
        let requested = RequestedStep::Random;
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::RandomObserved { value } = self.log[self.pos].event {
                self.pos += 1;
                return Ok(Outcome::Replayed(value));
            }
            return Err(self.mismatch(requested));
        }
        Ok(Outcome::Live(RandomPermit { cursor: self }))
    }

    /// Requests a model call identified by its request hash.
    ///
    /// Replayed: matches the recorded intent/completion pair and returns the
    /// recorded [`ModelReply`]. A recorded intent with no completion (the
    /// process died mid-call) hands back a live [`ModelCallPermit`] with no
    /// intent event: re-issuing an unanswered model request is safe, and the
    /// fresh completion correlates to the recorded intent. Live: the permit
    /// carries the intent event to persist before calling the provider.
    ///
    /// `request_body` is the optional full request to record on the intent,
    /// off by default (see the field on [`Event::ModelCallRequested`]). It is
    /// stored only on a genuinely fresh live intent and is ignored everywhere
    /// else: correlation keys on `request_hash` alone, so passing a body never
    /// changes which recorded call matches or what replays.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a hash mismatch or a different recorded
    /// event; [`ReplayError::MalformedLog`] when the event after the intent
    /// is not its completion.
    pub fn model_call(
        &mut self,
        request_hash: &str,
        request_body: Option<Value>,
    ) -> Result<Outcome<ModelReply, ModelCallPermit<'_>>, ReplayError> {
        let requested = RequestedStep::ModelCall {
            request_hash: request_hash.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            // The recorded body plays no part in correlation: only the hash is
            // matched, and the `request_body` the caller passed is ignored on
            // this replay path. That is what keeps a log captured with bodies
            // replaying identically to one captured without.
            let correlation = match &self.log[self.pos].event {
                Event::ModelCallRequested {
                    seq,
                    request_hash: recorded,
                    ..
                } if recorded == request_hash => *seq,
                _ => return Err(self.mismatch(requested)),
            };
            return match self.log.get(self.pos + 1) {
                Some(next) => {
                    if let Event::ModelCallCompleted {
                        seq,
                        response,
                        usage,
                    } = &next.event
                        && *seq == correlation
                    {
                        let reply = ModelReply {
                            response: response.clone(),
                            usage: *usage,
                        };
                        self.pos += 2;
                        Ok(Outcome::Replayed(reply))
                    } else {
                        Err(ReplayError::MalformedLog {
                            position: next.seq,
                            detail: format!(
                                "expected ModelCallCompleted correlated to seq {correlation}, found {}",
                                kind_name(&next.event)
                            ),
                        })
                    }
                }
                None => {
                    self.pos += 1;
                    Ok(Outcome::Live(ModelCallPermit {
                        correlation,
                        intent: None,
                        cursor: self,
                    }))
                }
            };
        }
        let correlation = self.reserve_seq();
        let intent = Emitted {
            seq: correlation,
            event: Event::ModelCallRequested {
                seq: correlation,
                request_hash: request_hash.to_owned(),
                // The only place the body is recorded: a genuinely fresh live
                // intent. Recording off leaves this `None` (byte-identical to
                // before the field existed); recording on stores the same
                // value that was hashed.
                request_body,
            },
        };
        Ok(Outcome::Live(ModelCallPermit {
            correlation,
            intent: Some(intent),
            cursor: self,
        }))
    }

    /// Requests a tool call.
    ///
    /// Replayed: matches the recorded intent/completion pair (tool, input,
    /// effect, and idempotency key must all be equal) and returns the
    /// recorded output. A recorded intent with no completion is the
    /// effect-table case:
    ///
    /// - [`Effect::Read`] and [`Effect::Idempotent`] hand back a live
    ///   [`ToolCallPermit`] to re-execute; the permit surfaces the recorded
    ///   idempotency key so the retry reuses it.
    /// - [`Effect::Write`] fails with [`ReplayError::NeedsReconciliation`].
    ///   The write may have landed; only a human may decide.
    ///
    /// Live: the permit carries the intent event, which the caller must
    /// persist **before** executing the tool. That write-ahead ordering is
    /// what makes a crash between intent and completion detectable.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on any payload mismatch or a different
    /// recorded event; [`ReplayError::NeedsReconciliation`] for a dangling
    /// write intent; [`ReplayError::MalformedLog`] when the event after the
    /// intent is not its completion.
    pub fn tool_call(
        &mut self,
        tool: &str,
        input: &Value,
        effect: Effect,
        idempotency_key: Option<&str>,
    ) -> Result<Outcome<Value, ToolCallPermit<'_>>, ReplayError> {
        let requested = RequestedStep::ToolCall {
            tool: tool.to_owned(),
            input: input.clone(),
            effect,
            idempotency_key: idempotency_key.map(ToOwned::to_owned),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            let correlation = match &self.log[self.pos].event {
                Event::ToolCallRequested {
                    seq,
                    tool: recorded_tool,
                    input: recorded_input,
                    effect: recorded_effect,
                    idempotency_key: recorded_key,
                    ..
                } if recorded_tool == tool
                    && recorded_input == input
                    && *recorded_effect == effect
                    && recorded_key.as_deref() == idempotency_key =>
                {
                    *seq
                }
                _ => return Err(self.mismatch(requested)),
            };
            return match self.log.get(self.pos + 1) {
                Some(next) => {
                    if let Event::ToolCallCompleted { seq, output } = &next.event
                        && *seq == correlation
                    {
                        let output = output.clone();
                        self.pos += 2;
                        Ok(Outcome::Replayed(output))
                    } else {
                        Err(ReplayError::MalformedLog {
                            position: next.seq,
                            detail: format!(
                                "expected ToolCallCompleted correlated to seq {correlation}, found {}",
                                kind_name(&next.event)
                            ),
                        })
                    }
                }
                None => match effect {
                    Effect::Write => Err(ReplayError::NeedsReconciliation {
                        position: self.log[self.pos].seq,
                        tool: tool.to_owned(),
                        input: input.clone(),
                        idempotency_key: idempotency_key.map(ToOwned::to_owned),
                    }),
                    Effect::Read | Effect::Idempotent => {
                        self.pos += 1;
                        Ok(Outcome::Live(ToolCallPermit {
                            correlation,
                            intent: None,
                            effect,
                            idempotency_key: idempotency_key.map(ToOwned::to_owned),
                            cursor: self,
                        }))
                    }
                },
            };
        }
        let correlation = self.reserve_seq();
        let key = idempotency_key.map(ToOwned::to_owned);
        let intent = Emitted {
            seq: correlation,
            event: Event::ToolCallRequested {
                seq: correlation,
                tool: tool.to_owned(),
                input: input.clone(),
                effect,
                idempotency_key: key.clone(),
                performed_by: None,
            },
        };
        Ok(Outcome::Live(ToolCallPermit {
            correlation,
            intent: Some(intent),
            effect,
            idempotency_key: key,
            cursor: self,
        }))
    }

    /// Requests suspension of the run.
    ///
    /// Replayed: matches the recorded [`Event::Suspended`]. Live: returns the
    /// event to persist. Either way, follow with [`ReplayCursor::await_resume`]
    /// to obtain the resume input (or learn the run is still parked).
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a payload mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn suspend(
        &mut self,
        reason: &str,
        input_schema: &Value,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::Suspend {
            reason: reason.to_owned(),
            input_schema: input_schema.clone(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::Suspended {
                reason: recorded_reason,
                input_schema: recorded_schema,
            } = &self.log[self.pos].event
                && recorded_reason == reason
                && recorded_schema == input_schema
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::Suspended {
            reason: reason.to_owned(),
            input_schema: input_schema.clone(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Requests the input a parked run was resumed with.
    ///
    /// Replayed: returns the recorded [`Event::Resumed`] input. Live: the
    /// resume has not happened yet; the [`Parked`] value can be redeemed with
    /// the input once a human provides it, or simply dropped, in which case
    /// the run stays parked and the process may exit.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] when the log records a different kind of
    /// event here, or the run already ended.
    pub fn await_resume(&mut self) -> Result<Outcome<Value, Parked<'_>>, ReplayError> {
        let requested = RequestedStep::AwaitResume;
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::Resumed { input } = &self.log[self.pos].event {
                let input = input.clone();
                self.pos += 1;
                return Ok(Outcome::Replayed(input));
            }
            return Err(self.mismatch(requested));
        }
        Ok(Outcome::Live(Parked { cursor: self }))
    }

    /// Reports a budget crossing, recorded so replay parks at the same point.
    ///
    /// This is a runtime request, not an orchestration one: the budget gate
    /// between events calls it when a declared limit is crossed. Checks must
    /// be computed from replayed data (recorded token usage, recorded
    /// timestamps), so a crossing that fired live is recomputed identically
    /// on replay and matched here. Follow with
    /// [`ReplayCursor::await_resume`], exactly like a suspension.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on a payload mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn budget_exceeded(
        &mut self,
        budget: Budget,
        observed: f64,
    ) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::BudgetExceeded { budget, observed };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::BudgetExceeded {
                budget: recorded_budget,
                observed: recorded_observed,
            } = &self.log[self.pos].event
                && *recorded_budget == budget
                && *recorded_observed == observed
            {
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::BudgetExceeded { budget, observed });
        Ok(Outcome::Live(emitted))
    }

    /// Requests successful completion of the run.
    ///
    /// Replayed: verifies the produced output equals the recorded one (a
    /// deterministic orchestration over replayed values must reproduce its
    /// own final answer). Live: returns the terminal event to persist. After
    /// either, every further request is a divergence.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on an output mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn complete_run(&mut self, output: &Value) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::CompleteRun {
            output: output.clone(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::RunCompleted {
                output: recorded_output,
            } = &self.log[self.pos].event
                && recorded_output == output
            {
                self.terminal = Some(self.log[self.pos].event.clone());
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::RunCompleted {
            output: output.clone(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Requests failure of the run.
    ///
    /// The failing counterpart of [`ReplayCursor::complete_run`], with the
    /// same replay/live/terminal behavior.
    ///
    /// # Errors
    ///
    /// [`ReplayError::Divergence`] on an error-message mismatch, a different
    /// recorded event, or a request after the run already ended.
    pub fn fail_run(&mut self, error: &str) -> Result<Outcome<(), Emitted>, ReplayError> {
        let requested = RequestedStep::FailRun {
            error: error.to_owned(),
        };
        self.guard_terminal(&requested)?;
        if self.pos < self.log.len() {
            if let Event::RunFailed {
                error: recorded_error,
            } = &self.log[self.pos].event
                && recorded_error == error
            {
                self.terminal = Some(self.log[self.pos].event.clone());
                self.pos += 1;
                return Ok(Outcome::Replayed(()));
            }
            return Err(self.mismatch(requested));
        }
        let emitted = self.emit(Event::RunFailed {
            error: error.to_owned(),
        });
        Ok(Outcome::Live(emitted))
    }

    /// Fails a request made after the run already reached a terminal event.
    fn guard_terminal(&self, requested: &RequestedStep) -> Result<(), ReplayError> {
        match &self.terminal {
            Some(terminal) => Err(ReplayError::Divergence {
                position: self.next_seq(),
                recorded: Box::new(LoggedStep::RunAlreadyTerminal(terminal.clone())),
                requested: Box::new(requested.clone()),
            }),
            None => Ok(()),
        }
    }

    /// Builds the divergence error for a mismatch at the current position.
    fn mismatch(&self, requested: RequestedStep) -> ReplayError {
        let env = &self.log[self.pos];
        ReplayError::Divergence {
            position: env.seq,
            recorded: Box::new(LoggedStep::Event(env.event.clone())),
            requested: Box::new(requested),
        }
    }

    /// Claims the next live position.
    fn reserve_seq(&mut self) -> SequenceNumber {
        let seq = self.next_live_seq;
        self.next_live_seq = seq.next();
        seq
    }

    /// Wraps an event with its live position and tracks terminal state.
    fn emit(&mut self, event: Event) -> Emitted {
        if matches!(event, Event::RunCompleted { .. } | Event::RunFailed { .. }) {
            self.terminal = Some(event.clone());
        }
        let seq = self.reserve_seq();
        Emitted { seq, event }
    }
}

/// Live permission to start a fresh run.
///
/// Handed out by [`ReplayCursor::begin`] when the log is empty. Redeem it
/// with the run's input to obtain the [`Event::RunStarted`] to persist. The
/// `labels` passed to [`begin`](ReplayCursor::begin) ride along on the permit
/// and land on the recorded event unchanged.
#[derive(Debug)]
pub struct BeginPermit<'c> {
    agent_def_hash: String,
    labels: Option<BTreeMap<String, String>>,
    cursor: &'c mut ReplayCursor,
}

impl BeginPermit<'_> {
    /// Records the run start with this input, returning the event to persist.
    /// Orchestration proceeds with the same input it passed here.
    #[must_use]
    pub fn record(self, input: Value) -> Emitted {
        let event = Event::RunStarted {
            agent_def_hash: self.agent_def_hash,
            input,
            labels: self.labels,
        };
        self.cursor.emit(event)
    }
}

/// Live permission to start a fresh graph run.
///
/// Handed out by [`ReplayCursor::begin_graph`] when the log is empty. Redeem it
/// with the run's input to obtain the [`Event::GraphRunStarted`] to persist.
/// The `labels` and `forked_from` passed to
/// [`begin_graph`](ReplayCursor::begin_graph) ride along and land on the
/// recorded event unchanged.
#[derive(Debug)]
pub struct GraphBeginPermit<'c> {
    graph_hash: String,
    labels: Option<BTreeMap<String, String>>,
    forked_from: Option<ForkOrigin>,
    cursor: &'c mut ReplayCursor,
}

impl GraphBeginPermit<'_> {
    /// Records the graph-run start with this input, returning the event to
    /// persist. Orchestration proceeds with the same input it passed here.
    #[must_use]
    pub fn record(self, input: Value) -> Emitted {
        let event = Event::GraphRunStarted {
            graph_hash: self.graph_hash,
            input,
            labels: self.labels,
            forked_from: self.forked_from,
        };
        self.cursor.emit(event)
    }
}

/// Live permission to observe the clock.
///
/// Handed out by [`ReplayCursor::now`] once history is exhausted. The caller
/// reads a real clock (the one side effect the cursor cannot perform) and
/// redeems the permit with the reading.
#[derive(Debug)]
pub struct NowPermit<'c> {
    cursor: &'c mut ReplayCursor,
}

impl NowPermit<'_> {
    /// Records the clock reading, returning the event to persist.
    #[must_use]
    pub fn record(self, now: OffsetDateTime) -> Emitted {
        self.cursor.emit(Event::NowObserved { now })
    }
}

/// Live permission to draw random bits.
///
/// Handed out by [`ReplayCursor::random`] once history is exhausted. The
/// caller draws from a real random source and redeems the permit with the
/// draw.
#[derive(Debug)]
pub struct RandomPermit<'c> {
    cursor: &'c mut ReplayCursor,
}

impl RandomPermit<'_> {
    /// Records the drawn bits, returning the event to persist.
    #[must_use]
    pub fn record(self, value: u64) -> Emitted {
        self.cursor.emit(Event::RandomObserved { value })
    }
}

/// Live permission to execute a model call.
///
/// While a permit borrows the cursor, no other request can be made, so a
/// call in flight cannot interleave with further steps. Persist
/// [`ModelCallPermit::intent`] (when present) before calling the provider,
/// then redeem with the response.
#[derive(Debug)]
pub struct ModelCallPermit<'c> {
    correlation: SequenceNumber,
    intent: Option<Emitted>,
    cursor: &'c mut ReplayCursor,
}

impl ModelCallPermit<'_> {
    /// The intent event to persist before executing, or `None` when the
    /// intent is already in the log (a call the process died in the middle
    /// of, now being re-issued).
    #[must_use]
    pub fn intent(&self) -> Option<&Emitted> {
        self.intent.as_ref()
    }

    /// The position of the intent this call correlates to.
    #[must_use]
    pub fn seq(&self) -> SequenceNumber {
        self.correlation
    }

    /// Records the model's response, returning the completion event to
    /// persist.
    #[must_use]
    pub fn record(self, response: Value, usage: TokenUsage) -> Emitted {
        self.cursor.emit(Event::ModelCallCompleted {
            seq: self.correlation,
            response,
            usage,
        })
    }
}

/// Live permission to execute a tool call.
///
/// Persist [`ToolCallPermit::intent`] (when present) **before** executing:
/// for [`Effect::Write`] tools that ordering is the whole reconciliation
/// story. Then execute and redeem with the output.
#[derive(Debug)]
pub struct ToolCallPermit<'c> {
    correlation: SequenceNumber,
    intent: Option<Emitted>,
    effect: Effect,
    idempotency_key: Option<String>,
    cursor: &'c mut ReplayCursor,
}

impl ToolCallPermit<'_> {
    /// The intent event to persist before executing, or `None` when the
    /// intent is already in the log (a read or idempotent call being
    /// re-executed after a crash).
    #[must_use]
    pub fn intent(&self) -> Option<&Emitted> {
        self.intent.as_ref()
    }

    /// The position of the intent this call correlates to.
    #[must_use]
    pub fn seq(&self) -> SequenceNumber {
        self.correlation
    }

    /// The effect class of the call being executed.
    #[must_use]
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// The idempotency key to execute under. For a re-executed idempotent
    /// call this is the recorded key, surfaced so the retry collapses into
    /// the original attempt at the provider.
    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    /// Records the tool's output, returning the completion event to persist.
    #[must_use]
    pub fn record(self, output: Value) -> Emitted {
        self.cursor.emit(Event::ToolCallCompleted {
            seq: self.correlation,
            output,
        })
    }
}

/// A run parked awaiting resume input that has not arrived.
///
/// Handed out by [`ReplayCursor::await_resume`] when the log ends at a
/// suspension or budget crossing. Drop it to leave the run parked (the
/// process may exit; the log already holds everything), or redeem it once
/// the input arrives.
#[derive(Debug)]
pub struct Parked<'c> {
    cursor: &'c mut ReplayCursor,
}

impl Parked<'_> {
    /// Records the resume input, returning the event to persist.
    #[must_use]
    pub fn resume(self, input: Value) -> Emitted {
        self.cursor.emit(Event::Resumed { input })
    }
}

/// The stable name of an event's kind, for error messages.
fn kind_name(event: &Event) -> &'static str {
    match event {
        Event::RunStarted { .. } => "RunStarted",
        Event::ModelCallRequested { .. } => "ModelCallRequested",
        Event::ModelCallCompleted { .. } => "ModelCallCompleted",
        Event::ToolCallRequested { .. } => "ToolCallRequested",
        Event::ToolCallCompleted { .. } => "ToolCallCompleted",
        Event::NowObserved { .. } => "NowObserved",
        Event::RandomObserved { .. } => "RandomObserved",
        Event::Suspended { .. } => "Suspended",
        Event::Resumed { .. } => "Resumed",
        Event::BudgetExceeded { .. } => "BudgetExceeded",
        Event::RunCompleted { .. } => "RunCompleted",
        Event::RunFailed { .. } => "RunFailed",
        Event::RunAbandoned { .. } => "RunAbandoned",
        Event::GraphRunStarted { .. } => "GraphRunStarted",
        Event::NodeEntered { .. } => "NodeEntered",
        Event::NodeExited { .. } => "NodeExited",
        Event::NodeSkipped { .. } => "NodeSkipped",
        Event::BranchTaken { .. } => "BranchTaken",
        Event::MapFannedOut { .. } => "MapFannedOut",
        Event::MapIterationStarted { .. } => "MapIterationStarted",
        Event::MapIterationJoined { .. } => "MapIterationJoined",
        Event::FoldIterationStarted { .. } => "FoldIterationStarted",
        Event::FoldIterationJoined { .. } => "FoldIterationJoined",
        Event::FoldConverged { .. } => "FoldConverged",
    }
}

impl fmt::Display for LoggedStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event(event) => write!(f, "{}", event_summary(event)),
            Self::RunAlreadyTerminal(event) => {
                write!(f, "run already ended with {}", event_summary(event))
            }
        }
    }
}

impl fmt::Display for RequestedStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Begin { agent_def_hash } => write!(f, "Begin(agent_def_hash={agent_def_hash})"),
            Self::BeginGraph { graph_hash } => write!(f, "BeginGraph(graph_hash={graph_hash})"),
            Self::NodeEntered { node } => write!(f, "NodeEntered(node={node})"),
            Self::NodeExited { node } => write!(f, "NodeExited(node={node})"),
            Self::NodeSkipped { node, reason } => {
                write!(f, "NodeSkipped(node={node}, reason={reason})")
            }
            Self::BranchTaken { node, case } => write!(f, "BranchTaken(node={node}, case={case})"),
            Self::MapFannedOut { node, .. } => write!(f, "MapFannedOut(node={node})"),
            Self::MapIterationStarted {
                node,
                index,
                child_run,
            } => write!(
                f,
                "MapIterationStarted(node={node}, index={index}, child_run={child_run})"
            ),
            Self::MapIterationJoined { node, index } => {
                write!(f, "MapIterationJoined(node={node}, index={index})")
            }
            Self::Now => write!(f, "Now"),
            Self::Random => write!(f, "Random"),
            Self::ModelCall { request_hash } => write!(f, "ModelCall(request_hash={request_hash})"),
            Self::ToolCall { tool, effect, .. } => {
                write!(f, "ToolCall(tool={tool}, effect={effect:?})")
            }
            Self::Suspend { reason, .. } => write!(f, "Suspend(reason={reason})"),
            Self::AwaitResume => write!(f, "AwaitResume"),
            Self::BudgetExceeded { budget, observed } => {
                write!(
                    f,
                    "BudgetExceeded(kind={:?}, limit={}, observed={observed})",
                    budget.kind, budget.limit
                )
            }
            Self::CompleteRun { .. } => write!(f, "CompleteRun"),
            Self::FailRun { error } => write!(f, "FailRun(error={error})"),
        }
    }
}

/// A one-line summary of a recorded event, for divergence messages.
fn event_summary(event: &Event) -> String {
    let name = kind_name(event);
    match event {
        Event::RunStarted { agent_def_hash, .. } => {
            format!("{name}(agent_def_hash={agent_def_hash})")
        }
        Event::ModelCallRequested { request_hash, .. } => {
            format!("{name}(request_hash={request_hash})")
        }
        Event::ToolCallRequested { tool, effect, .. } => {
            format!("{name}(tool={tool}, effect={effect:?})")
        }
        Event::Suspended { reason, .. } => format!("{name}(reason={reason})"),
        Event::BudgetExceeded { budget, observed } => format!(
            "{name}(kind={:?}, limit={}, observed={observed})",
            budget.kind, budget.limit
        ),
        Event::RunFailed { error } => format!("{name}(error={error})"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::RunId;
    use time::macros::datetime;
    use uuid::Uuid;

    /// A non-empty log must start at position 0. A well-formed tail whose
    /// head rows went missing in storage would otherwise replay with every
    /// position silently shifted.
    #[test]
    fn log_not_starting_at_seq_zero_is_malformed() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000005").unwrap());
        let log = vec![EventEnvelope::new(
            run_id,
            SequenceNumber::new(5),
            datetime!(2026-07-09 12:00:00 UTC),
            Event::RunStarted {
                agent_def_hash: "sha256:agent".into(),
                input: serde_json::json!({}),
                labels: None,
            },
        )];
        let err = ReplayCursor::new(log).expect_err("a truncated head must be rejected");
        assert_eq!(
            err,
            ReplayError::MalformedLog {
                position: SequenceNumber::new(5),
                detail:
                    "a run log must start at seq 0, found seq 5; the head of the log is missing"
                        .to_owned(),
            }
        );
    }

    /// A graph run's log opens with `GraphRunStarted`, and the cursor accepts
    /// it as a legal run head: the sole wire-adjacent change graph runs make
    /// to the cursor.
    #[test]
    fn graph_run_started_is_a_legal_first_event() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000006").unwrap());
        let log = vec![EventEnvelope::new(
            run_id,
            SequenceNumber::new(0),
            datetime!(2026-07-14 12:00:00 UTC),
            Event::GraphRunStarted {
                graph_hash: "sha256:graph".into(),
                input: serde_json::json!({"topic": "otters"}),
                labels: None,
                forked_from: None,
            },
        )];
        let cursor = ReplayCursor::new(log).expect("a graph run head is a legal first event");
        assert_eq!(cursor.run_id(), Some(run_id));
        assert!(cursor.is_replaying());
    }

    /// Driving `begin_graph`, `node_entered`, `node_exited` live emits the graph
    /// markers at contiguous positions, and replaying over the emitted log
    /// answers each from history with no divergence.
    #[test]
    fn graph_markers_emit_live_and_replay_from_history() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000007").unwrap());

        // Live: a fresh cursor emits the head and one node's enter/exit.
        let mut cursor = ReplayCursor::new(vec![]).expect("empty log is a fresh run");
        let mut emitted = Vec::new();
        match cursor
            .begin_graph("sha256:graph", None, None)
            .expect("begin")
        {
            Outcome::Live(permit) => emitted.push(permit.record(serde_json::json!({"t": 1}))),
            Outcome::Replayed(_) => panic!("fresh run must be live"),
        }
        match cursor.node_entered("research").expect("enter") {
            Outcome::Live(e) => emitted.push(e),
            Outcome::Replayed(()) => panic!("fresh run must be live"),
        }
        match cursor.node_exited("research").expect("exit") {
            Outcome::Live(e) => emitted.push(e),
            Outcome::Replayed(()) => panic!("fresh run must be live"),
        }
        let positions: Vec<u64> = emitted.iter().map(|e| e.seq.get()).collect();
        assert_eq!(positions, [0, 1, 2], "markers occupy contiguous positions");

        // Replay: build a log from the emitted events and drive the same calls.
        let log: Vec<EventEnvelope> = emitted
            .iter()
            .map(|e| {
                EventEnvelope::new(
                    run_id,
                    e.seq,
                    datetime!(2026-07-14 12:00:00 UTC),
                    e.event.clone(),
                )
            })
            .collect();
        let mut replay = ReplayCursor::new(log).expect("emitted log is well formed");
        let input = match replay
            .begin_graph("sha256:graph", None, None)
            .expect("begin")
        {
            Outcome::Replayed(input) => input,
            Outcome::Live(_) => panic!("recorded head must replay"),
        };
        assert_eq!(input, serde_json::json!({"t": 1}), "recorded input replays");
        assert!(matches!(
            replay.node_entered("research").expect("enter"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay.node_exited("research").expect("exit"),
            Outcome::Replayed(())
        ));
        assert!(!replay.is_replaying(), "history fully consumed");
    }

    /// `branch_taken` and `node_skipped` emit their events live at contiguous
    /// positions and replay from history with no divergence.
    #[test]
    fn branch_and_skip_markers_emit_live_and_replay() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000009").unwrap());

        let mut cursor = ReplayCursor::new(vec![]).expect("fresh run");
        let mut emitted = Vec::new();
        match cursor.begin_graph("sha256:g", None, None).expect("begin") {
            Outcome::Live(p) => emitted.push(p.record(serde_json::json!({}))),
            Outcome::Replayed(_) => panic!("fresh must be live"),
        }
        match cursor.branch_taken("route", "high").expect("branch") {
            Outcome::Live(e) => emitted.push(e),
            Outcome::Replayed(()) => panic!("fresh must be live"),
        }
        match cursor
            .node_skipped("reject", "route fired high")
            .expect("skip")
        {
            Outcome::Live(e) => emitted.push(e),
            Outcome::Replayed(()) => panic!("fresh must be live"),
        }
        let positions: Vec<u64> = emitted.iter().map(|e| e.seq.get()).collect();
        assert_eq!(positions, [0, 1, 2], "contiguous positions");

        let log: Vec<EventEnvelope> = emitted
            .iter()
            .map(|e| {
                EventEnvelope::new(
                    run_id,
                    e.seq,
                    datetime!(2026-07-14 12:00:00 UTC),
                    e.event.clone(),
                )
            })
            .collect();
        let mut replay = ReplayCursor::new(log).expect("well formed");
        replay.begin_graph("sha256:g", None, None).expect("begin");
        assert!(matches!(
            replay.branch_taken("route", "high").expect("branch"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay
                .node_skipped("reject", "route fired high")
                .expect("skip"),
            Outcome::Replayed(())
        ));
        assert!(!replay.is_replaying(), "history fully consumed");
    }

    /// A branch-case that does not match the recorded case at its position is a
    /// divergence: the recorded route is authoritative.
    #[test]
    fn a_mismatched_branch_case_diverges() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-00000000000a").unwrap());
        let log = vec![
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(0),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::GraphRunStarted {
                    graph_hash: "sha256:g".into(),
                    input: serde_json::json!({}),
                    labels: None,
                    forked_from: None,
                },
            ),
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(1),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::BranchTaken {
                    node: "route".into(),
                    case: "high".into(),
                },
            ),
        ];
        let mut cursor = ReplayCursor::new(log).expect("well formed");
        cursor.begin_graph("sha256:g", None, None).expect("begin");
        let err = cursor
            .branch_taken("route", "low")
            .expect_err("a different case must diverge");
        assert!(
            matches!(err, ReplayError::Divergence { position, .. } if position == SequenceNumber::new(1))
        );
    }

    /// A graph-marker request that does not match the recorded event at its
    /// position is a divergence naming both sides.
    #[test]
    fn a_mismatched_node_id_diverges() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000008").unwrap());
        let log = vec![
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(0),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::GraphRunStarted {
                    graph_hash: "sha256:graph".into(),
                    input: serde_json::json!({}),
                    labels: None,
                    forked_from: None,
                },
            ),
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(1),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::NodeEntered {
                    node: "research".into(),
                },
            ),
        ];
        let mut cursor = ReplayCursor::new(log).expect("well formed");
        cursor
            .begin_graph("sha256:graph", None, None)
            .expect("begin");
        let err = cursor
            .node_entered("publish")
            .expect_err("a different node id must diverge");
        assert!(
            matches!(err, ReplayError::Divergence { position, .. } if position == SequenceNumber::new(1))
        );
    }

    /// The three map markers emit live at contiguous positions and replay from
    /// history with no divergence: a fan-out of two, joined in index order.
    #[test]
    fn map_markers_emit_live_and_replay() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-00000000000b").unwrap());
        let items = serde_json::json!(["a", "b"]);

        let mut cursor = ReplayCursor::new(vec![]).expect("fresh run");
        let mut emitted = Vec::new();
        match cursor.begin_graph("sha256:g", None, None).expect("begin") {
            Outcome::Live(p) => emitted.push(p.record(serde_json::json!({}))),
            Outcome::Replayed(_) => panic!("fresh must be live"),
        }
        let mut push_live = |outcome: Outcome<(), Emitted>| match outcome {
            Outcome::Live(e) => emitted.push(e),
            Outcome::Replayed(()) => panic!("fresh must be live"),
        };
        push_live(cursor.node_entered("fanout").expect("enter"));
        push_live(cursor.map_fanned_out("fanout", &items).expect("fanout"));
        push_live(
            cursor
                .map_iteration_started("fanout", 0, "sha256:c0")
                .expect("start 0"),
        );
        push_live(
            cursor
                .map_iteration_started("fanout", 1, "sha256:c1")
                .expect("start 1"),
        );
        push_live(cursor.map_iteration_joined("fanout", 0).expect("join 0"));
        push_live(cursor.map_iteration_joined("fanout", 1).expect("join 1"));
        push_live(cursor.node_exited("fanout").expect("exit"));

        let positions: Vec<u64> = emitted.iter().map(|e| e.seq.get()).collect();
        assert_eq!(positions, [0, 1, 2, 3, 4, 5, 6, 7], "contiguous positions");

        let log: Vec<EventEnvelope> = emitted
            .iter()
            .map(|e| {
                EventEnvelope::new(
                    run_id,
                    e.seq,
                    datetime!(2026-07-14 12:00:00 UTC),
                    e.event.clone(),
                )
            })
            .collect();
        let mut replay = ReplayCursor::new(log).expect("well formed");
        replay.begin_graph("sha256:g", None, None).expect("begin");
        assert!(matches!(
            replay.node_entered("fanout").expect("enter"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay.map_fanned_out("fanout", &items).expect("fanout"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay
                .map_iteration_started("fanout", 0, "sha256:c0")
                .expect("start 0"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay
                .map_iteration_started("fanout", 1, "sha256:c1")
                .expect("start 1"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay.map_iteration_joined("fanout", 0).expect("join 0"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay.map_iteration_joined("fanout", 1).expect("join 1"),
            Outcome::Replayed(())
        ));
        assert!(matches!(
            replay.node_exited("fanout").expect("exit"),
            Outcome::Replayed(())
        ));
        assert!(!replay.is_replaying(), "history fully consumed");
    }

    /// A map iteration replays on its STRUCTURAL position (node + index); the
    /// RECORDED child run id wins even when a caller re-derives a different one.
    /// This is what keeps a fork's inherited map markers replayable: the fork
    /// replays the origin's prefix under a new run id, so its re-derived child ids
    /// differ from the origin's, and trusting the recorded id is correct.
    #[test]
    fn a_replayed_map_iteration_honors_the_recorded_child_run() {
        let run_id =
            RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-00000000000c").unwrap());
        let log = vec![
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(0),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::GraphRunStarted {
                    graph_hash: "sha256:g".into(),
                    input: serde_json::json!({}),
                    labels: None,
                    forked_from: None,
                },
            ),
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(1),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::MapIterationStarted {
                    node: "fanout".into(),
                    index: 0,
                    child_run: "sha256:origin-derived".into(),
                },
            ),
        ];
        let mut cursor = ReplayCursor::new(log).expect("well formed");
        cursor.begin_graph("sha256:g", None, None).expect("begin");
        // A different (fork-re-derived) child id still replays: node + index pin
        // the position, and the recorded id stands.
        assert!(matches!(
            cursor
                .map_iteration_started("fanout", 0, "sha256:fork-re-derived")
                .expect("a re-derived child id must still replay on node+index"),
            Outcome::Replayed(())
        ));
        // But a wrong INDEX at this position is a genuine divergence.
        let mut cursor = ReplayCursor::new(vec![
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(0),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::GraphRunStarted {
                    graph_hash: "sha256:g".into(),
                    input: serde_json::json!({}),
                    labels: None,
                    forked_from: None,
                },
            ),
            EventEnvelope::new(
                run_id,
                SequenceNumber::new(1),
                datetime!(2026-07-14 12:00:00 UTC),
                Event::MapIterationStarted {
                    node: "fanout".into(),
                    index: 0,
                    child_run: "sha256:origin-derived".into(),
                },
            ),
        ])
        .expect("well formed");
        cursor.begin_graph("sha256:g", None, None).expect("begin");
        let err = cursor
            .map_iteration_started("fanout", 1, "sha256:origin-derived")
            .expect_err("a different index must diverge");
        assert!(
            matches!(err, ReplayError::Divergence { position, .. } if position == SequenceNumber::new(1))
        );
    }
}
