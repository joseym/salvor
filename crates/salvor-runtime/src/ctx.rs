//! [`RunCtx`]: the public durability substrate. One recorded run, one
//! context; every operation is answered from history or executed live and
//! persisted immediately.
//!
//! This is the library-first tier: a Rust team
//! that wants to own its control flow writes an ordinary async function
//! against this type and gets the same durability, replay, and budget
//! guarantees as the built-in loop, which is itself written against exactly
//! this surface.
//!
//! # What it owns, and what it wraps
//!
//! The pure replay cursor in `salvor-core` refuses to own three things: a
//! store, executors, and the ambient clock/RNG. `RunCtx` owns all three and
//! wraps each cursor request one to one:
//!
//! | `RunCtx` method | cursor request | live side effect |
//! |---|---|---|
//! | [`begin`](RunCtx::begin) | `begin` | persist `RunStarted` |
//! | [`now`](RunCtx::now) | `now` | read the injected clock, persist |
//! | [`random`](RunCtx::random) | `random` | draw from the injected RNG, persist |
//! | [`model_call`](RunCtx::model_call) | `model_call` | persist intent, call provider, persist completion |
//! | [`tool_call`](RunCtx::tool_call) | `tool_call` | persist intent **before executing**, execute, persist completion |
//! | [`suspend`](RunCtx::suspend) | `suspend` | persist `Suspended` |
//! | [`suspend_for_signal`](RunCtx::suspend_for_signal) | `suspend_for_signal` | persist `Suspended` marked as a signal wait |
//! | [`await_resume`](RunCtx::await_resume) | `await_resume` | persist `Resumed` when input was provided |
//! | [`sleep_until`](RunCtx::sleep_until) | `sleep_started` | persist `SleepStarted` |
//! | [`await_wake`](RunCtx::await_wake) | `sleep_completed` | read the injected clock; persist `SleepCompleted` once the instant has passed |
//! | [`budget_exceeded`](RunCtx::budget_exceeded) | `budget_exceeded` | persist `BudgetExceeded` |
//! | [`complete_run`](RunCtx::complete_run) / [`fail_run`](RunCtx::fail_run) | same | persist the terminal event |
//!
//! Every live permit redemption persists its event *immediately*, with a
//! timestamp read from the injected clock at this IO edge. Nothing is
//! buffered: when a method returns `Ok`, the event is durable.
//!
//! # Injected clock and randomness
//!
//! The constructor takes the clock and RNG as functions. The defaults read
//! the real clock and the operating system's randomness; tests inject
//! deterministic ones, which makes whole event logs (envelopes included)
//! comparable across runs. Note the injection covers the *envelope
//! timestamps and observations*, not replay: replayed values always come
//! from the log, whatever functions are installed.
//!
//! # Write-ahead ordering
//!
//! [`tool_call`](RunCtx::tool_call) persists the intent event and only then
//! executes the tool. For a `Write`-effect tool this ordering is the whole
//! reconciliation story: a crash between intent and completion leaves
//! evidence, and resume refuses to guess. Model intents persist before the
//! provider call for the same reason (though a dangling model intent is
//! safely re-issued rather than reconciled).
//!
//! # Nothing happens twice, across runs as well as within one
//!
//! Replay is what keeps a resumed run from repeating itself: a recorded
//! completion is read back, never re-executed. Two *independent* runs share no
//! log, so replay has nothing to say about them, and something else has to
//! hold the line. That something is the idempotency key, arbitrated by the
//! store.
//!
//! The whole decision happens in [`tool_call`](RunCtx::tool_call), live,
//! before the intent is written and before the tool runs; see that method for
//! the mechanism and its boundaries. What matters here is the boundary it does
//! not cross: **replay never consults the store about another run.** A
//! recorded log is a complete description of its run, and folding it back
//! produces the same result on a machine that has never seen the store the run
//! was recorded against.
//!
//! # Retries inside one tool call
//!
//! One `tool_call` is one intent/completion pair, so retries of a failed
//! live execution happen *inside* the call, between the two events, honoring
//! `RetryPolicy`: `Read` and `Idempotent` handler failures re-execute up to
//! [`MAX_TOOL_ATTEMPTS`] total attempts (idempotent retries reuse the same
//! key, carried on `ToolCtx`), `Write` failures never re-execute, and input
//! validation or output serialization failures never retry because they
//! would fail identically again. Whatever the final result, the completion
//! is recorded: an output, a suspension sentinel, or a failure object (see
//! [`crate::wire`]).
//!
//! # Sleeping belongs between calls, never inside one
//!
//! [`sleep_until`](RunCtx::sleep_until) and [`sleep_for`](RunCtx::sleep_for)
//! must not be called between a claimed tool call's intent and its
//! completion. A claim is held for the whole span between the two, so every
//! other run presenting that idempotency key gets `CallInFlight` for as long
//! as the sleep lasts, and a durable timer lasts hours or weeks where a call
//! lasts seconds. A process death inside such a sleep is worse: it leaves a
//! dangling `Write` intent, which derives to
//! [`RunStatus::NeedsReconciliation`](salvor_core::RunStatus::NeedsReconciliation)
//! and stops the run until a human answers for the write by hand.
//!
//! Nothing in `RunCtx` can enforce the ordering, because the caller owns it
//! and this type sees one request at a time. Sleep between completed calls.
//!
//! A tool that asks for the sleep itself is not an exception to that rule, it
//! is the rule mechanized. A tool returning `ToolOutcome::Sleep` has its
//! request encoded into its own `ToolCallCompleted` (see [`crate::wire`]), so
//! the call settles, the claim releases, and only then does the driver call
//! [`sleep_until`](RunCtx::sleep_until). The recorded order is intent,
//! completion, `SleepStarted`, and a sleeping run therefore holds no claim.

use std::collections::BTreeMap;
use std::sync::Arc;

use salvor_core::{
    Budget, DedupOrigin, Effect, Emitted, Event, EventEnvelope, ModelReply, Outcome, PendingCall,
    ReplayCursor, RunId, SequenceNumber, SuspensionKind, TokenUsage,
};
use salvor_llm::{Client, MessageAccumulator, MessageRequest, MessageResponse, StreamEvent};
use salvor_store::{CallClaim, CallClaimant, CallCommitment, EventStore};
use salvor_tools::{DynTool, RetryPolicy, Sleep, Suspension, ToolCtx, ToolError, ToolOutcome};
use serde_json::Value;
use time::{Duration, OffsetDateTime, PrimitiveDateTime};
use uuid::Uuid;

use crate::error::RuntimeError;
use crate::hash::hash_value;
use crate::labels::validate_labels;
use crate::model::{response_value, usage_of};
use crate::wire::{
    ToolFailure, decode_failure, decode_sleep, decode_suspension, encode_failure, encode_sleep,
    encode_suspension,
};

/// The injected clock: called once per persisted event (for the envelope
/// timestamp) and once per live [`RunCtx::now`] observation.
pub type ClockFn = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// The injected random source: called once per live [`RunCtx::random`]
/// observation, returning 64 raw bits.
pub type RandomFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The cap on total executions of one live tool call, counting the first
/// attempt. Applies only where `RetryPolicy` allows retrying at all.
pub const MAX_TOOL_ATTEMPTS: u32 = 3;

/// A model call's result: the typed response plus the token usage recorded
/// for it. Identical whether the call was executed live or replayed.
#[derive(Debug, Clone)]
pub struct ModelTurn {
    /// The model's response.
    pub response: MessageResponse,
    /// The recorded token usage for this call.
    pub usage: TokenUsage,
}

/// A tool call's result, decoded from the recorded completion output (the
/// decoding is identical live and on replay, which is what keeps a resumed
/// orchestration on the recorded path).
#[derive(Debug, Clone)]
pub enum ToolCallResult {
    /// The tool produced this output.
    Output(Value),
    /// The tool failed after exhausting its retry policy; the full error is
    /// recorded in the completion. See [`crate::wire`] for the shape.
    Failed(ToolFailure),
    /// The tool asked to park the run. Follow with [`RunCtx::suspend`] and
    /// [`RunCtx::await_resume`].
    Suspended(Suspension),
    /// The tool asked to park the run until an instant. Follow with
    /// [`RunCtx::sleep_until`] and [`RunCtx::await_wake`].
    ///
    /// The call itself is finished when this is returned: its completion is
    /// recorded and any idempotency claim is settled, so the sleep that
    /// follows holds nothing. See [`crate::wire`] for why the request travels
    /// in the completion rather than as an event of its own.
    Sleeping(Sleep),
}

/// What [`RunCtx::await_resume`] produced.
#[derive(Debug, Clone)]
pub enum Resumption {
    /// The resume input, recorded or just persisted. Continue the run.
    Resumed(Value),
    /// No resume input exists yet. The run is parked durably; the log
    /// already holds everything, so the process may simply stop driving it.
    Parked,
}

/// What [`RunCtx::await_wake`] produced: the timer counterpart of
/// [`Resumption`], and separate from it because the two park for different
/// reasons and end differently. A suspension ends when someone supplies an
/// input; a sleep ends when an instant arrives and carries no input at all.
#[derive(Debug, Clone, Copy)]
pub enum Waking {
    /// The wake is recorded, whether it was replayed from the log or just
    /// persisted. Continue the run.
    Woken,
    /// The wake instant has not arrived. The run is parked durably; the
    /// recorded `SleepStarted` already holds the deadline, so the process may
    /// simply stop driving it and come back at `wake_at` or later.
    ///
    /// Named for the state the run is in rather than for the non-event that
    /// left it there, exactly as [`Resumption::Parked`] is.
    Asleep {
        /// The recorded instant the run may continue at, so a caller deciding
        /// when to come back does not have to re-derive the log.
        wake_at: OffsetDateTime,
    },
}

/// The public durability substrate for one run. See the module docs.
pub struct RunCtx {
    cursor: ReplayCursor,
    store: Arc<dyn EventStore>,
    run_id: RunId,
    clock: ClockFn,
    random: RandomFn,
    resume_input: Option<Value>,
    /// The wake instant of the sleep this drive last recorded or replayed,
    /// set by [`sleep_until`](Self::sleep_until) and read by
    /// [`await_wake`](Self::await_wake) to decide whether the deadline has
    /// arrived. Not state about the run (the log holds that); state about
    /// where this drive is, which is why it is not persisted and why a fresh
    /// context starts without it.
    sleeping_until: Option<OffsetDateTime>,
    /// Whether to record the full model request body on each
    /// `ModelCallRequested`. Off unless [`with_record_prompts`](Self::with_record_prompts)
    /// turns it on. See that method for the PII rationale.
    record_prompts: bool,
    /// Correlation tags to stamp on a genuinely fresh `RunStarted`. Unset
    /// unless [`with_labels`](Self::with_labels) sets them. See that method.
    labels: Option<BTreeMap<String, String>>,
}

impl RunCtx {
    /// Builds a context over a run's recorded log (empty for a fresh run),
    /// with the default clock (the real UTC clock) and the default random
    /// source (operating-system randomness).
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Replay`] when the log is not a well-formed
    /// run history.
    pub fn new(
        store: Arc<dyn EventStore>,
        run_id: RunId,
        log: Vec<EventEnvelope>,
    ) -> Result<Self, RuntimeError> {
        Self::with_hooks(
            store,
            run_id,
            log,
            Arc::new(OffsetDateTime::now_utc),
            Arc::new(os_random),
        )
    }

    /// Builds a context with an injected clock and random source.
    ///
    /// The clock stamps every persisted envelope and answers live
    /// [`now`](Self::now) observations; the random source answers live
    /// [`random`](Self::random) observations. Injecting deterministic
    /// functions makes complete event logs comparable across runs, which is
    /// how the kill/resume tests prove byte-identical recovery.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Replay`] when the log is not a well-formed
    /// run history.
    pub fn with_hooks(
        store: Arc<dyn EventStore>,
        run_id: RunId,
        log: Vec<EventEnvelope>,
        clock: ClockFn,
        random: RandomFn,
    ) -> Result<Self, RuntimeError> {
        let cursor = ReplayCursor::new(log)?;
        Ok(Self {
            cursor,
            store,
            run_id,
            clock,
            random,
            resume_input: None,
            sleeping_until: None,
            record_prompts: false,
            labels: None,
        })
    }

    /// Turns on recording of the full model request body into the durable log.
    ///
    /// Additive and off by default: the existing [`new`](Self::new) and
    /// [`with_hooks`](Self::with_hooks) constructors leave it off, so no
    /// caller that predates this method changes behavior. Chained builder
    /// style keeps those signatures intact, which is why the flag arrives this
    /// way rather than as a new constructor argument.
    ///
    /// When on, each live [`model_call`](Self::model_call) records the exact
    /// request it sent on the `ModelCallRequested` event, so the v0.3 dashboard
    /// inspector can show the prompt. This is PII-sensitive: the body can hold
    /// user data and secrets, which is why the default is off and turning it on
    /// is a deliberate per-agent or operator choice. The recorded body lands
    /// only in the event log; it never reaches the progress stream or any
    /// console output. It does not affect replay: the request hash is computed
    /// the same either way, and replay ignores the body.
    #[must_use]
    pub fn with_record_prompts(mut self, record_prompts: bool) -> Self {
        self.record_prompts = record_prompts;
        self
    }

    /// Sets the correlation tags to stamp on a genuinely fresh `RunStarted`.
    ///
    /// Additive and unset by default: the existing [`new`](Self::new) and
    /// [`with_hooks`](Self::with_hooks) constructors leave it unset, so no
    /// caller that predates this method changes behavior. Chained builder
    /// style, mirroring [`with_record_prompts`](Self::with_record_prompts).
    ///
    /// Labels are checked against the sanity bounds (see
    /// [`crate::validate_labels`]) only on [`begin`](Self::begin)'s live path,
    /// the moment a `RunStarted` is actually about to be created;
    /// [`RuntimeError::InvalidLabels`] surfaces there, not here, so this
    /// setter itself is infallible. A replayed `begin` never re-checks them:
    /// whatever the log already holds is trusted and returned as recorded.
    /// Labels never enter `agent_def_hash` or any request hash; they are a
    /// tag on the run, not part of its identity.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// Provides the input a parked run is being resumed with. The next
    /// [`await_resume`](Self::await_resume) that reaches live mode records
    /// it as the `Resumed` event and returns it; without one, a live
    /// `await_resume` reports [`Resumption::Parked`].
    pub fn set_resume_input(&mut self, input: Value) {
        self.resume_input = Some(input);
    }

    /// The resume input staged by [`set_resume_input`](Self::set_resume_input)
    /// and not yet consumed, without consuming it.
    ///
    /// This is the read-only half of the accept edge. A driver that needs to
    /// vet a resume input against something only it knows (the graph engine
    /// checks a gate's declared `approval_schema`) has to see the value BEFORE
    /// [`await_resume`](Self::await_resume) turns it into a `Resumed` event,
    /// because after that it is history and refusing it would mean an appended
    /// event the run has to live with. Peeking here and refusing leaves the log
    /// untouched and the run parked exactly as it was.
    ///
    /// `None` once `await_resume` has taken the value, or when none was staged.
    #[must_use]
    pub fn staged_resume_input(&self) -> Option<&Value> {
        self.resume_input.as_ref()
    }

    /// The run this context drives.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Whether recorded history remains to be consumed.
    #[must_use]
    pub fn is_replaying(&self) -> bool {
        self.cursor.is_replaying()
    }

    /// The log position the next consumed or emitted event occupies.
    #[must_use]
    pub fn next_seq(&self) -> SequenceNumber {
        self.cursor.next_seq()
    }

    /// Starts (or replays the start of) the run.
    ///
    /// Live: records `RunStarted` with `input` and the labels set through
    /// [`with_labels`](Self::with_labels) (if any), and returns `input`.
    /// Replayed: verifies `agent_def_hash` against the recorded event and
    /// returns the *recorded* input, which always wins; the `input` argument
    /// is only used when the log is empty, exactly like `labels`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on a definition-hash mismatch or any other
    /// divergence; [`RuntimeError::InvalidLabels`] when the labels set
    /// through [`with_labels`](Self::with_labels) violate the sanity bounds
    /// (only checked on the live path; see that method); [`RuntimeError::Store`]
    /// when persistence fails.
    pub async fn begin(
        &mut self,
        agent_def_hash: &str,
        input: &Value,
    ) -> Result<Value, RuntimeError> {
        match self.cursor.begin(agent_def_hash, self.labels.clone())? {
            Outcome::Replayed(recorded) => Ok(recorded),
            Outcome::Live(permit) => {
                if let Some(labels) = &self.labels {
                    validate_labels(labels).map_err(RuntimeError::InvalidLabels)?;
                }
                let emitted = permit.record(input.clone());
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                Ok(input.clone())
            }
        }
    }

    /// Starts (or replays the start of) a graph run: the graph-document
    /// counterpart of [`begin`](Self::begin).
    ///
    /// Live: records [`salvor_core::Event::GraphRunStarted`] with `input`, the
    /// labels set through [`with_labels`](Self::with_labels) (if any), and no
    /// fork origin, then returns `input`. Replayed: verifies `graph_hash`
    /// against the recorded head (a changed graph document must not silently
    /// resume an old run) and returns the *recorded* input, which always wins.
    ///
    /// A graph run's log opens with this event rather than `RunStarted` because
    /// a graph coordinates many agent hashes and has none at its head. The graph
    /// engine calls this once, then frames each node with
    /// [`node_entered`](Self::node_entered) / [`node_exited`](Self::node_exited)
    /// and records the single terminal itself after the last node.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on a graph-hash mismatch or any other
    /// divergence; [`RuntimeError::InvalidLabels`] when the labels set through
    /// [`with_labels`](Self::with_labels) violate the sanity bounds (only
    /// checked on the live path, exactly as [`begin`](Self::begin) does);
    /// [`RuntimeError::Store`] when persistence fails.
    pub async fn begin_graph(
        &mut self,
        graph_hash: &str,
        input: &Value,
    ) -> Result<Value, RuntimeError> {
        match self
            .cursor
            .begin_graph(graph_hash, self.labels.clone(), None)?
        {
            Outcome::Replayed(recorded) => Ok(recorded),
            Outcome::Live(permit) => {
                if let Some(labels) = &self.labels {
                    validate_labels(labels).map_err(RuntimeError::InvalidLabels)?;
                }
                let emitted = permit.record(input.clone());
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                Ok(input.clone())
            }
        }
    }

    /// Records (or replays) entry into a graph node. A graph node's own events
    /// (an agent loop's model calls, a tool call) are recorded between this and
    /// the matching [`node_exited`](Self::node_exited).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn node_entered(&mut self, node: &str) -> Result<(), RuntimeError> {
        match self.cursor.node_entered(node)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) exit from a graph node, having produced its output.
    /// The counterpart of [`node_entered`](Self::node_entered).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn node_exited(&mut self, node: &str) -> Result<(), RuntimeError> {
        match self.cursor.node_exited(node)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that a graph node was skipped: reached on the walk
    /// but deliberately not run (a branch routed past it). Unlike an executed
    /// node there is no [`node_entered`](Self::node_entered)/[`node_exited`](Self::node_exited)
    /// pair; the skip is the node's sole marker, which is what lets a projection
    /// tell "skipped" apart from "never reached". `reason` must be a pure
    /// function of the document and recorded values so it reproduces on replay.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn node_skipped(&mut self, node: &str, reason: &str) -> Result<(), RuntimeError> {
        match self.cursor.node_skipped(node, reason)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that a branch node routed: the named `case` fired.
    /// Recorded between the branch's [`node_entered`](Self::node_entered) and
    /// [`node_exited`](Self::node_exited), it is the sole authority for which way
    /// the branch went. The chosen `case` must be a deterministic function of
    /// recorded values (a pure expression over the routed value, or a decision
    /// recomputed from a replayed model reply) so replay reproduces the route.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn branch_taken(&mut self, node: &str, case: &str) -> Result<(), RuntimeError> {
        match self.cursor.branch_taken(node, case)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that a map node fanned out over a resolved item list.
    ///
    /// Recorded between the map node's [`node_entered`](Self::node_entered) and its
    /// per-iteration markers. The `items` must be a deterministic function of
    /// recorded values (the map's `over` reference resolved against the recorded
    /// routed value), so replay reproduces the identical fan-out, which is what
    /// makes the derived per-iteration child ids reproducible.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn map_fanned_out(&mut self, node: &str, items: &Value) -> Result<(), RuntimeError> {
        match self.cursor.map_fanned_out(node, items)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that one iteration of a map fan-out started, as a child
    /// run with the derived id `child_run`. The `child_run` is derived from the
    /// parent run id, the node id, and the index. On replay the RECORDED id wins
    /// and the match is on `node` + `index` alone, so a fork (which replays the
    /// origin's prefix under a new run id and thus re-derives a different id)
    /// still replays its inherited map markers cleanly.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn map_iteration_started(
        &mut self,
        node: &str,
        index: u64,
        child_run: &str,
    ) -> Result<(), RuntimeError> {
        match self.cursor.map_iteration_started(node, index, child_run)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that one iteration of a map fan-out joined back into
    /// the map node's output. Joins must be recorded in index order, never
    /// completion order, so the concurrency of the fan-out never influences the
    /// parent log's byte sequence.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn map_iteration_joined(
        &mut self,
        node: &str,
        index: u64,
    ) -> Result<(), RuntimeError> {
        match self.cursor.map_iteration_joined(node, index)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that a fold node began one bounded pass of its
    /// accumulate-and-refine loop. A fold's passes run inline in this log rather
    /// than as child runs, so `index` is both the pass position and its recorded
    /// order, and replay matches it exactly: a replayed pass returns without
    /// re-recording anything.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn fold_iteration_started(
        &mut self,
        node: &str,
        index: u64,
    ) -> Result<(), RuntimeError> {
        match self.cursor.fold_iteration_started(node, index)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that one fold pass joined back into the fold node's
    /// accumulated value. Recorded in index order, which for a fold is already
    /// completion order because its passes are sequential. A replayed join
    /// returns without re-recording anything.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn fold_iteration_joined(
        &mut self,
        node: &str,
        index: u64,
    ) -> Result<(), RuntimeError> {
        match self.cursor.fold_iteration_joined(node, index)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Records (or replays) that a fold node settled: its loop stopped and its
    /// `join` rule selected the pass at `winner_index`, for the recorded
    /// `reason`. This is the sole authority for which pass the fold's output
    /// came from, as [`branch_taken`](Self::branch_taken) is for a branch's
    /// route. Both the winner and the reason must be deterministic functions of
    /// the recorded pass values, because replay matches all three fields and a
    /// replayed convergence returns without re-recording anything.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn fold_converged(
        &mut self,
        node: &str,
        winner_index: u64,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        match self.cursor.fold_converged(node, winner_index, reason)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// The recorded clock: reads the injected clock once, live, and replays
    /// the identical instant forever after.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn now(&mut self) -> Result<OffsetDateTime, RuntimeError> {
        match self.cursor.now()? {
            Outcome::Replayed(instant) => Ok(instant),
            Outcome::Live(permit) => {
                let instant = (self.clock)();
                let emitted = permit.record(instant);
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                Ok(instant)
            }
        }
    }

    /// The recorded random source: draws 64 bits from the injected source
    /// once, live, and replays the identical bits forever after. Richer
    /// random values must be derived from these bits deterministically.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn random(&mut self) -> Result<u64, RuntimeError> {
        match self.cursor.random()? {
            Outcome::Replayed(bits) => Ok(bits),
            Outcome::Live(permit) => {
                let bits = (self.random)();
                let emitted = permit.record(bits);
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                Ok(bits)
            }
        }
    }

    /// A recorded model call.
    ///
    /// The request is identified by its content hash
    /// (`sha256:` over the canonical serialization; see [`crate::hash`]).
    /// Replayed: the recorded response is decoded and returned; the provider
    /// is never contacted. Live: the intent event is persisted, the provider
    /// is called through `client`, and the completion (response plus usage)
    /// is persisted. A recorded intent with no completion (a call the
    /// process died inside) is re-issued safely: the fresh completion
    /// correlates to the recorded intent.
    ///
    /// When [`with_record_prompts`](Self::with_record_prompts) is on, the exact
    /// request body is recorded alongside the hash on the fresh live intent.
    /// It is the same value the hash was computed over, it never feeds into the
    /// hash, and replay ignores it, so recording it changes nothing about how
    /// the run replays.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence, [`RuntimeError::Store`] when
    /// persistence fails, [`RuntimeError::Model`] when the live provider
    /// call fails (the log stays intact and the run is recoverable),
    /// [`RuntimeError::RequestEncode`] / [`RuntimeError::RecordedResponseDecode`]
    /// on the JSON edges.
    pub async fn model_call(
        &mut self,
        client: &Client,
        request: &MessageRequest,
    ) -> Result<ModelTurn, RuntimeError> {
        let request_value = serde_json::to_value(request).map_err(RuntimeError::RequestEncode)?;
        let request_hash = hash_value(&request_value);
        // The hash is computed above from `request_value` and is unaffected by
        // what follows. When prompt recording is on, the body handed to the
        // cursor is that same `request_value`, so the recorded body is exactly
        // what was hashed; when off it is `None` and nothing is recorded.
        let request_body = if self.record_prompts {
            Some(request_value)
        } else {
            None
        };
        match self.cursor.model_call(&request_hash, request_body)? {
            Outcome::Replayed(ModelReply { response, usage }) => {
                let response = serde_json::from_value(response)
                    .map_err(RuntimeError::RecordedResponseDecode)?;
                Ok(ModelTurn { response, usage })
            }
            Outcome::Live(permit) => {
                if let Some(intent) = permit.intent().cloned() {
                    persist(self.store.as_ref(), self.run_id, &self.clock, &intent).await?;
                }
                let response = client.send_message(request).await?;
                let usage = usage_of(&response);
                let completion = permit.record(response_value(&response), usage);
                persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
                Ok(ModelTurn { response, usage })
            }
        }
    }

    /// A recorded model call that streams live events to `on_event` while it
    /// runs, recording the identical completion [`model_call`](Self::model_call)
    /// would record.
    ///
    /// This is a live-progress affordance layered on top of the durable record,
    /// not a different kind of call. The recorded log is byte-for-byte what
    /// [`model_call`](Self::model_call) writes for the same underlying response:
    /// the request is hashed the same way (see [`crate::hash`]), the intent is
    /// the same `ModelCallRequested`, and the completion carries the same
    /// `response` value and `usage`. A run does not care which path recorded it,
    /// and replay is deterministic either way.
    ///
    /// Replayed: the recorded response is decoded and returned, exactly as
    /// [`model_call`](Self::model_call) does. The provider is never contacted and
    /// `on_event` never fires, because there are no live tokens to report; the
    /// caller gets the final result at once.
    ///
    /// Live: the intent event is persisted first (write-ahead, the same ordering
    /// [`model_call`](Self::model_call) uses), then the provider stream is opened
    /// through `client`. Each [`StreamEvent`] is handed to `on_event` for a live
    /// ticker (text deltas ride [`StreamEvent::ContentBlockDelta`], token counts
    /// ride [`StreamEvent::MessageDelta`]) and, in the same pass, applied to a
    /// [`MessageAccumulator`]. When the stream ends, the assembled
    /// [`MessageResponse`] is converted with the same `response_value` and usage
    /// logic [`model_call`](Self::model_call) uses, the completion is persisted,
    /// and the [`ModelTurn`] is returned.
    ///
    /// All persistence lives inside this method, so a caller cannot record a
    /// partial or wrong completion: the completion is written only after the
    /// stream is fully assembled. A caller that drops the returned future before
    /// the stream completes leaves a dangling model intent (the write-ahead
    /// intent with no completion), exactly like a live [`model_call`](Self::model_call)
    /// the process died inside. That intent is re-issued safely on resume: the
    /// fresh completion correlates to the recorded intent. `on_event` firing is
    /// not part of the durable record, so a ticker that saw partial tokens before
    /// the drop has no effect on what replay produces.
    ///
    /// When [`with_record_prompts`](Self::with_record_prompts) is on, the exact
    /// request body is recorded on the fresh live intent, identically to
    /// [`model_call`](Self::model_call).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence, [`RuntimeError::Store`] when
    /// persistence fails, [`RuntimeError::Model`] when the live stream fails
    /// (opening it, an error event or transport fault mid-stream, or a
    /// tool-call fragment that does not parse) surfaced as the same error type
    /// [`model_call`](Self::model_call) returns, with the log left intact and the
    /// run recoverable, and [`RuntimeError::RequestEncode`] /
    /// [`RuntimeError::RecordedResponseDecode`] on the JSON edges.
    pub async fn model_call_streaming(
        &mut self,
        client: &Client,
        request: &MessageRequest,
        mut on_event: impl FnMut(&StreamEvent),
    ) -> Result<ModelTurn, RuntimeError> {
        let request_value = serde_json::to_value(request).map_err(RuntimeError::RequestEncode)?;
        let request_hash = hash_value(&request_value);
        // Hashing and body recording are identical to `model_call`: the hash is
        // computed from `request_value` above, and the body handed to the cursor
        // is that same value when recording is on, `None` when off. Streaming
        // changes nothing here, which is half of why the recorded intent matches.
        let request_body = if self.record_prompts {
            Some(request_value)
        } else {
            None
        };
        match self.cursor.model_call(&request_hash, request_body)? {
            Outcome::Replayed(ModelReply { response, usage }) => {
                // No live call, so `on_event` never fires: replay has no tokens.
                let response = serde_json::from_value(response)
                    .map_err(RuntimeError::RecordedResponseDecode)?;
                Ok(ModelTurn { response, usage })
            }
            Outcome::Live(permit) => {
                if let Some(intent) = permit.intent().cloned() {
                    persist(self.store.as_ref(), self.run_id, &self.clock, &intent).await?;
                }
                // Pump the stream once: every event feeds the ticker and the
                // accumulator in the same pass. The accumulator assembles the
                // exact `MessageResponse` `send_message` would have returned
                // (salvor-llm guarantees this), so the recorded completion below
                // is byte-identical to the non-streaming path.
                let mut stream = client.stream_message(request).await?;
                let mut accumulator = MessageAccumulator::new();
                while let Some(event) = stream.next_event().await {
                    let event = event?;
                    on_event(&event);
                    accumulator.apply(&event)?;
                }
                let response = accumulator.into_message()?;
                let usage = usage_of(&response);
                let completion = permit.record(response_value(&response), usage);
                persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
                Ok(ModelTurn { response, usage })
            }
        }
    }

    /// A recorded tool call: one intent/completion pair, whatever happens in
    /// between.
    ///
    /// Replayed: the recorded completion output is decoded (an output, a
    /// failure object, or a suspension sentinel; see [`crate::wire`]) and
    /// the tool is never executed. Live: the intent is persisted **before**
    /// the tool executes (write-ahead), the tool runs with retries per its
    /// effect's `RetryPolicy` (see [`MAX_TOOL_ATTEMPTS`]), and the
    /// completion is persisted. A recorded `Read`/`Idempotent` intent with
    /// no completion re-executes here under its recorded idempotency key; a
    /// dangling `Write` intent fails with
    /// `ReplayError::NeedsReconciliation` before anything runs.
    ///
    /// `idempotency_key` is the key for a *fresh* call; the built-in loop
    /// derives it from [`random`](Self::random) for `Idempotent` tools so it
    /// reproduces on replay. A key the tool declares for itself
    /// ([`DynTool::idempotency_key`]) takes precedence over the one passed
    /// here, because only the tool can say what effect a call *is*. For a
    /// re-executed recorded intent the recorded key wins, and whatever is
    /// presented must match it (the cursor checks).
    ///
    /// # Cross-run deduplication
    ///
    /// Within one run, nothing happens twice because a recorded completion is
    /// replayed rather than re-executed. Across independent runs there is no
    /// log to replay, so something else has to hold the line, and that
    /// something is the idempotency key.
    ///
    /// ## Which keys count
    ///
    /// Only a key the tool **declares** for itself, through
    /// [`DynTool::idempotency_key`], is an identity to deduplicate on. A key
    /// the runtime derives on a tool's behalf is not, and the difference is not
    /// a technicality.
    ///
    /// A hand-written tool makes that declaration in Rust. An MCP or wasm tool
    /// has no code here to make it in, so its operator does, by naming the
    /// input field that identifies a call in the agent file
    /// (`idempotency_keys`); the tool derives the key from that field on every
    /// call and answers through the same trait method. Nothing below this
    /// distinguishes the two, because there is no distinction to make: both are
    /// a statement about what the call does in the world, from someone in a
    /// position to know.
    ///
    /// A declared key is a statement about the world: `"pay_claim:wreck-9931"`
    /// means *this is the payout for claim 9931*, and two calls carrying it are
    /// the same payment no matter which run asked for them. A derived key says
    /// something much weaker. The built-in loop draws one from recorded
    /// randomness so a retry within a run reuses it; the graph engine derives
    /// one from a node's position so a fork re-executing a node reuses it.
    /// Both are *attempt* identifiers, scoped to one run or one lineage, and
    /// two unrelated runs can hold the same derived key over completely
    /// different arguments. Treating one as an effect identity would let a
    /// second run collect the first run's output for a call it never made,
    /// which is a worse failure than the duplicate execution this is meant to
    /// stop.
    ///
    /// So a derived key keeps doing exactly what it always did, at the provider
    /// and inside its own run, and is recorded exactly as before. Cross-run
    /// deduplication waits for a tool to say what its calls *are*.
    ///
    /// A declared key is also what a call records, in preference to a derived
    /// one, since only the tool can name its own effect.
    ///
    /// ## The mechanism
    ///
    /// **The decision is made here, live, before the intent is recorded and
    /// before the tool runs.** For a [`Effect::Write`] or
    /// [`Effect::Idempotent`] call carrying a declared key, this method claims
    /// the identity `(tool name, idempotency key)` in the store
    /// ([`EventStore::claim_call`](salvor_store::EventStore::claim_call)),
    /// which is the arbiter:
    ///
    /// - **Claimed.** This run is the one execution. The intent is recorded,
    ///   the tool runs, and the completion is appended *and* settles the
    ///   commitment as one atomic step, so no crash can leave a committed
    ///   completion the store still calls unfinished.
    /// - **Held, and settled.** An equal call is already committed. The origin
    ///   run's log is read back (through
    ///   [`read_log`](salvor_store::EventStore::read_log), so its hash chain is
    ///   verified before a single byte is copied), its recorded input is
    ///   checked against this call's, and its output becomes this call's
    ///   output. The intent is still recorded, because an intent that resolves
    ///   as a duplicate is an honest thing to have recorded, and the completion
    ///   carries a [`DedupOrigin`] naming what it copied. **The tool does not
    ///   run.**
    /// - **Held, and unfinished.** Refused with
    ///   [`RuntimeError::CallInFlight`], before anything is recorded. See that
    ///   variant for why refusing beats guessing.
    ///
    /// A call with no declared key is untouched by any of this: there is no
    /// identity to deduplicate on, so a keyless write behaves exactly as it
    /// always has, and so does a write carrying only a derived key. So does
    /// every [`Effect::Read`], which has no effect worth naming.
    ///
    /// **Replay never participates.** A recorded completion replays from the
    /// log, whether it was witnessed or copied, with no store lookup of any
    /// kind; the [`DedupOrigin`] on it is read by humans and audits, never by
    /// the cursor. That is what keeps a recorded log a self-contained
    /// description of a run.
    ///
    /// The one place resume consults the store is the gap a crash can leave
    /// between a deduplicated intent and its copied completion. That intent
    /// executed nothing (this run never held the identity, so it never held the
    /// right to execute), and the store can prove it, so the call is finished
    /// as the duplicate it was rather than parked for a human. Every other
    /// dangling write still parks: see [`recover_deduplicated_intent`](Self::recover_deduplicated_intent).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence or a dangling write intent;
    /// [`RuntimeError::Store`] when persistence fails;
    /// [`RuntimeError::CallInFlight`] when another run holds this call's
    /// identity and has not finished with it;
    /// [`RuntimeError::IdempotencyKeyCollision`] when one key names two
    /// different calls; [`RuntimeError::CommitmentUnreadable`] when the store
    /// points at a completion its own log does not hold. A failing *tool* is
    /// not an `Err`: it returns [`ToolCallResult::Failed`], because the
    /// failure is a recorded outcome the orchestration must handle
    /// deterministically.
    pub async fn tool_call(
        &mut self,
        tool: &dyn DynTool,
        input: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<ToolCallResult, RuntimeError> {
        let effect = tool.effect();
        let declared = tool.idempotency_key(input);
        // What is recorded on the wire: the tool's own declaration when it
        // makes one, otherwise the attempt key the caller derived.
        let recorded = declared
            .clone()
            .or_else(|| idempotency_key.map(ToOwned::to_owned));
        let key = recorded.as_deref();
        // What deduplication is arbitrated on: a declared key only. See the
        // method docs for why an attempt key is not an identity.
        let identity = declared.as_deref().filter(|_| deduplicates(effect));

        // Before the cursor is asked to take a step, because `tool_call` either
        // advances it or fails, with nothing in between where a store lookup
        // could go.
        if let Some(resolved) = self
            .recover_deduplicated_intent(tool, input, effect, identity)
            .await?
        {
            return Ok(resolved);
        }

        match self.cursor.tool_call(tool.name(), input, effect, key)? {
            Outcome::Replayed(output) => Ok(decode_tool_output(output)),
            Outcome::Live(permit) => {
                // THE DECISION POINT. Live, before the write-ahead intent is
                // persisted and before the tool is touched. Nothing below this
                // block consults the store about other runs, and replay never
                // reaches it at all.
                let claimant = identity.map(|key| CallClaimant {
                    tool: tool.name(),
                    idempotency_key: key,
                    run_id: self.run_id,
                    intent_seq: permit.seq(),
                });
                let mut copied = None;
                if let Some(claimant) = claimant {
                    match self.store.claim_call(claimant).await? {
                        // This run is the one execution.
                        CallClaim::Claimed => {}
                        CallClaim::Held(commitment) if commitment.completion_seq.is_some() => {
                            copied = Some(
                                committed_call(
                                    self.store.as_ref(),
                                    tool.name(),
                                    claimant.idempotency_key,
                                    commitment,
                                    input,
                                )
                                .await?,
                            );
                        }
                        // Held by a run that has not finished. Nothing is
                        // recorded, so this run can simply be run again once
                        // the holder is resolved.
                        CallClaim::Held(commitment) => {
                            return Err(RuntimeError::CallInFlight {
                                tool: tool.name().to_owned(),
                                idempotency_key: claimant.idempotency_key.to_owned(),
                                holder: commitment.run_id,
                                holder_seq: commitment.intent_seq.get(),
                            });
                        }
                    }
                }

                // Write-ahead, on both paths. An intent that resolves as a
                // duplicate is still an honest record of what this run asked
                // for.
                if let Some(intent) = permit.intent().cloned() {
                    persist(self.store.as_ref(), self.run_id, &self.clock, &intent).await?;
                }

                if let Some((output, origin)) = copied {
                    let completion = permit.record_deduplicated(output.clone(), origin);
                    persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
                    return Ok(decode_tool_output(output));
                }

                let key = permit.idempotency_key().map(ToOwned::to_owned);
                let tool_ctx = ToolCtx::new(key);
                let policy = RetryPolicy::for_effect(effect);
                let mut attempts: u32 = 0;
                let outcome = loop {
                    attempts += 1;
                    match tool.call_json(&tool_ctx, input.clone()).await {
                        Ok(outcome) => break Ok(outcome),
                        Err(error) => {
                            // Only a handler failure is retryable, and only
                            // when the effect's policy allows a re-attempt.
                            let may_retry = matches!(error, ToolError::Handler { .. })
                                && policy.allows_retry()
                                && attempts < MAX_TOOL_ATTEMPTS;
                            if may_retry {
                                continue;
                            }
                            break Err(error);
                        }
                    }
                };
                let (output, result) = match outcome {
                    Ok(ToolOutcome::Output(value)) => {
                        (value.clone(), ToolCallResult::Output(value))
                    }
                    Ok(ToolOutcome::Suspend(suspension)) => (
                        encode_suspension(&suspension),
                        ToolCallResult::Suspended(suspension),
                    ),
                    Ok(ToolOutcome::Sleep(sleep)) => {
                        // The instant is normalized on the way into the
                        // completion, so what the caller sleeps on is what the
                        // log holds and what every later drive decodes.
                        let output = encode_sleep(&sleep);
                        let recorded = decode_sleep(&output).unwrap_or(sleep);
                        (output, ToolCallResult::Sleeping(recorded))
                    }
                    Err(error) => {
                        let failure = ToolFailure::from_error(&error, attempts);
                        (encode_failure(&failure), ToolCallResult::Failed(failure))
                    }
                };
                let completion = permit.record(output);
                match claimant {
                    // The completion and the settlement land together, so the
                    // store never believes an identity is still in flight when
                    // its result is already recorded.
                    Some(claimant) => {
                        persist_settling(
                            self.store.as_ref(),
                            self.run_id,
                            &self.clock,
                            &completion,
                            claimant,
                        )
                        .await?;
                    }
                    None => {
                        persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
                    }
                }
                Ok(result)
            }
        }
    }

    /// Finishes a deduplicated call whose process died between recording the
    /// intent and recording the copied completion, or reports that this is not
    /// that situation.
    ///
    /// This is the only place a resume consults the store about another run,
    /// and it turns on a fact the store can settle: a call executes only under
    /// a claim, so an identity held by a **different** run is proof that this
    /// run never executed. The intent it left behind is then not a dangling
    /// write at all, it is an unfinished copy, and parking it would ask a human
    /// to reconcile an effect that provably never happened.
    ///
    /// Every other reading falls through to [`tool_call`](Self::tool_call)'s
    /// normal path and its normal consequences. In particular an identity held
    /// by *this* run is exactly the reconciliation hazard it has always been:
    /// this run did hold the right to execute, so nobody can say from the
    /// outside whether it did, and the run parks with
    /// [`ReplayError::NeedsReconciliation`](salvor_core::ReplayError::NeedsReconciliation).
    ///
    /// Returns `Ok(None)` when the situation does not apply, which is the
    /// overwhelmingly common case.
    async fn recover_deduplicated_intent(
        &mut self,
        tool: &dyn DynTool,
        input: &Value,
        effect: Effect,
        key: Option<&str>,
    ) -> Result<Option<ToolCallResult>, RuntimeError> {
        let Some(key) = key.filter(|_| deduplicates(effect)) else {
            return Ok(None);
        };
        let Some(PendingCall::Tool {
            tool: recorded_tool,
            input: recorded_input,
            effect: recorded_effect,
            idempotency_key: Some(recorded_key),
            ..
        }) = self.cursor.dangling_intent()
        else {
            return Ok(None);
        };
        // Only the call the orchestration is asking for right now. Anything
        // else is a divergence for the cursor to report, not ours to smooth
        // over.
        if recorded_tool != tool.name()
            || recorded_input != *input
            || recorded_effect != effect
            || recorded_key != key
        {
            return Ok(None);
        }

        let Some(commitment) = self.store.lookup_call(tool.name(), key).await? else {
            return Ok(None);
        };
        // The proof, and the whole reason this is safe: the identity belongs to
        // some other run, and that run finished. This run never held it, so it
        // never had the right to execute, so it did not.
        if commitment.run_id == self.run_id || commitment.completion_seq.is_none() {
            return Ok(None);
        }

        let (output, origin) =
            committed_call(self.store.as_ref(), tool.name(), key, commitment, input).await?;
        let permit = self
            .cursor
            .resume_unexecuted_tool_call(tool.name(), input, effect, key)?;
        let completion = permit.record_deduplicated(output.clone(), origin);
        persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
        Ok(Some(decode_tool_output(output)))
    }

    /// Parks the run on a human gate: records `Suspended { reason,
    /// input_schema }`, with no discriminator, which is what every suspension
    /// recorded before signals existed means. Follow with
    /// [`await_resume`](Self::await_resume).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn suspend(
        &mut self,
        reason: &str,
        input_schema: &Value,
    ) -> Result<(), RuntimeError> {
        self.suspend_with_kind(reason, input_schema, None).await
    }

    /// Parks the run on an external signal: records `Suspended` with
    /// [`SuspensionKind::Signal`](salvor_core::SuspensionKind::Signal), for a
    /// wait a webhook or callback answers rather than a person. Follow with
    /// [`await_resume`](Self::await_resume), exactly as a gate does.
    ///
    /// The run parks, validates, and resumes identically either way. The
    /// recorded discriminator exists so a surface can route: a signal wait is
    /// nobody's task, and listing it in an approval inbox invents work for an
    /// operator who cannot do it.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence (a replayed suspension whose
    /// discriminator differs included); [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn suspend_for_signal(
        &mut self,
        reason: &str,
        input_schema: &Value,
    ) -> Result<(), RuntimeError> {
        self.suspend_with_kind(reason, input_schema, Some(SuspensionKind::Signal))
            .await
    }

    /// Parks the run on a suspension whose discriminator is already a value:
    /// records `Suspended { reason, input_schema, kind }`. Follow with
    /// [`await_resume`](Self::await_resume).
    ///
    /// This exists for the drivers, which read the kind off a
    /// [`Suspension`](salvor_tools::Suspension) a tool returned and cannot
    /// choose between the two named methods without matching on it. Hand-written
    /// orchestration should say [`suspend`](Self::suspend) or
    /// [`suspend_for_signal`](Self::suspend_for_signal) instead, so the call
    /// site reads as what it is.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence (a replayed suspension whose
    /// discriminator differs included); [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn suspend_with_kind(
        &mut self,
        reason: &str,
        input_schema: &Value,
        kind: Option<SuspensionKind>,
    ) -> Result<(), RuntimeError> {
        let requested = match kind {
            None => self.cursor.suspend(reason, input_schema)?,
            Some(SuspensionKind::Signal) => self.cursor.suspend_for_signal(reason, input_schema)?,
        };
        match requested {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Obtains the input a parked run was resumed with.
    ///
    /// Replayed: the recorded `Resumed` input. Live: when a resume input was
    /// provided through [`set_resume_input`](Self::set_resume_input), it is
    /// recorded and returned; otherwise the run stays parked and
    /// [`Resumption::Parked`] tells the caller to stop driving.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn await_resume(&mut self) -> Result<Resumption, RuntimeError> {
        match self.cursor.await_resume()? {
            Outcome::Replayed(input) => Ok(Resumption::Resumed(input)),
            Outcome::Live(parked) => match self.resume_input.take() {
                Some(input) => {
                    let emitted = parked.resume(input.clone());
                    persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                    Ok(Resumption::Resumed(input))
                }
                None => Ok(Resumption::Parked),
            },
        }
    }

    /// Parks the run on a durable timer: records `SleepStarted { wake_at }`.
    /// Follow with [`await_wake`](Self::await_wake).
    ///
    /// `wake_at` must be derived from recorded data, because replay presents
    /// it again and the cursor matches it exactly: derive it from an observed
    /// [`now`](Self::now) (which [`sleep_for`](Self::sleep_for) does for you),
    /// never from a clock read outside the log. An instant recomputed from an
    /// ambient clock differs on every drive and diverges on the first one.
    ///
    /// # Never inside a claimed tool call
    ///
    /// A sleep must not be recorded between a claimed call's intent and its
    /// completion. The claim is held for the whole span, so every other run
    /// under that idempotency key gets `CallInFlight` for as long as the run
    /// sleeps, which for a durable timer is hours or weeks rather than the
    /// seconds a call takes. Worse, a process death mid-sleep strands a
    /// dangling `Write` intent, which derives to
    /// [`RunStatus::NeedsReconciliation`](salvor_core::RunStatus::NeedsReconciliation)
    /// and needs a human before the run moves again. Sleeping belongs between
    /// completed calls. Nothing here can enforce that (the caller owns the
    /// ordering, and this context sees one request at a time), so this
    /// paragraph is the guardrail.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence, including a `wake_at` that
    /// differs from the recorded one; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn sleep_until(&mut self, wake_at: OffsetDateTime) -> Result<(), RuntimeError> {
        match self.cursor.sleep_started(wake_at)? {
            Outcome::Replayed(()) => {}
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
            }
        }
        self.sleeping_until = Some(wake_at);
        Ok(())
    }

    /// Sleeps for `duration` from a recorded reading of the clock, returning
    /// the wake instant it recorded.
    ///
    /// Exactly `now() + duration`, recorded: the reading goes into the log as
    /// a `NowObserved` before the sleep is derived from it, so every later
    /// drive replays the identical reading and derives the identical instant.
    /// A duration alone means nothing to a replay, which has no clock to
    /// interpret it against; this is the composition that turns one into an
    /// instant without leaving determinism behind.
    ///
    /// Carries every constraint [`sleep_until`](Self::sleep_until) does,
    /// including the never-inside-a-claimed-call rule. Follow it with
    /// [`await_wake`](Self::await_wake).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::SleepOverflow`] when the wake instant would fall
    /// outside the representable range; [`RuntimeError::Replay`] on
    /// divergence; [`RuntimeError::Store`] when persistence fails.
    pub async fn sleep_for(&mut self, duration: Duration) -> Result<OffsetDateTime, RuntimeError> {
        let now = self.now().await?;
        let wake_at = now
            .checked_add(duration)
            .ok_or(RuntimeError::SleepOverflow { now, duration })?;
        self.sleep_until(wake_at).await?;
        Ok(wake_at)
    }

    /// Asks whether the sleep is over.
    ///
    /// Replayed: the log holds the `SleepCompleted`, so the sleep already
    /// ended and the run carries on. Live: the injected clock decides. At or
    /// past the recorded wake instant the completion is recorded and the run
    /// continues; before it, the run stays asleep and [`Waking::Asleep`] tells
    /// the caller to stop driving.
    ///
    /// The clock read belongs here and not in the cursor, which reads none;
    /// it is the same category of live-only decision as "was a resume input
    /// provided", and like that one it is never recorded as an observation,
    /// because what the log needs is the fact that the sleep ended, not the
    /// instant something noticed. Enforcing the deadline here also means no
    /// caller can wake a run early by driving it early: a driver that comes
    /// back too soon simply finds it still asleep.
    ///
    /// Call it after [`sleep_until`](Self::sleep_until) or
    /// [`sleep_for`](Self::sleep_for) in the same drive, so the deadline to
    /// compare against is in hand. Without a sleep before it, there is no
    /// deadline that could have arrived and the run stays asleep.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn await_wake(&mut self) -> Result<Waking, RuntimeError> {
        match self.cursor.sleep_completed()? {
            Outcome::Replayed(()) => {
                self.sleeping_until = None;
                Ok(Waking::Woken)
            }
            Outcome::Live(asleep) => {
                // The last representable instant stands in for a deadline
                // this drive never set: a wake nobody asked for has not
                // arrived, and no clock reading will make it so.
                let wake_at = self
                    .sleeping_until
                    .unwrap_or_else(|| PrimitiveDateTime::MAX.assume_utc());
                if (self.clock)() < wake_at {
                    return Ok(Waking::Asleep { wake_at });
                }
                let emitted = asleep.wake();
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await?;
                self.sleeping_until = None;
                Ok(Waking::Woken)
            }
        }
    }

    /// Records a budget crossing. The check that led here must be computed
    /// from replayed data (recorded usage, recorded `now` observations) so
    /// it re-fires identically on replay. Follow with
    /// [`await_resume`](Self::await_resume), exactly like a suspension.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn budget_exceeded(
        &mut self,
        budget: Budget,
        observed: f64,
    ) -> Result<(), RuntimeError> {
        match self.cursor.budget_exceeded(budget, observed)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Completes the run with `output`. Every request after this is a
    /// divergence.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence (including an output that does
    /// not match the recorded one); [`RuntimeError::Store`] when persistence
    /// fails.
    pub async fn complete_run(&mut self, output: &Value) -> Result<(), RuntimeError> {
        match self.cursor.complete_run(output)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }

    /// Fails the run with `error`. Every request after this is a divergence.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence; [`RuntimeError::Store`] when
    /// persistence fails.
    pub async fn fail_run(&mut self, error: &str) -> Result<(), RuntimeError> {
        match self.cursor.fail_run(error)? {
            Outcome::Replayed(()) => Ok(()),
            Outcome::Live(emitted) => {
                persist(self.store.as_ref(), self.run_id, &self.clock, &emitted).await
            }
        }
    }
}

/// Wraps an emitted event in an envelope (timestamp from the injected clock,
/// at this IO edge) and appends it durably. When this returns `Ok`, the
/// event is in the store.
async fn persist(
    store: &dyn EventStore,
    run_id: RunId,
    clock: &ClockFn,
    emitted: &Emitted,
) -> Result<(), RuntimeError> {
    let envelope = EventEnvelope::new(run_id, emitted.seq, (clock)(), emitted.event.clone());
    store.append(&envelope).await?;
    // The event is durable now, so this is the honest moment to report it.
    // Live progress streams from here as the run drives; replayed events take
    // the cursor's early return above and never reach this edge, so they never
    // re-emit. The detail is truncated (see `crate::progress`), so no full
    // payload rides the progress stream.
    crate::progress::emit_step(run_id, envelope.seq, &envelope.event);
    Ok(())
}

/// Whether an effect class participates in cross-run deduplication at all.
///
/// A [`Effect::Read`] performs nothing worth naming, so there is nothing for a
/// second run to avoid repeating. The other two classes both do something to
/// the world, and a key on them is a claim about which something.
fn deduplicates(effect: Effect) -> bool {
    matches!(effect, Effect::Write | Effect::Idempotent)
}

/// Reads back the call a commitment points at, and checks it really is the same
/// call.
///
/// The read goes through [`EventStore::read_log`], never around it, so the
/// origin run's hash chain is verified before any of its recorded bytes are
/// copied into this run's log. A commitment is a pointer and nothing else,
/// which is what makes that unavoidable rather than merely encouraged.
///
/// The input check is what keeps a key honest. If the origin's recorded input
/// differs from this call's, one key is naming two different calls, and both
/// available answers are wrong: copying would return an output computed from
/// somebody else's arguments, executing would repeat an effect the key says has
/// already happened. So neither happens.
async fn committed_call(
    store: &dyn EventStore,
    tool: &str,
    idempotency_key: &str,
    commitment: CallCommitment,
    input: &Value,
) -> Result<(Value, DedupOrigin), RuntimeError> {
    let log = store.read_log(commitment.run_id).await?;
    let correlation = commitment.intent_seq;
    let unreadable = || RuntimeError::CommitmentUnreadable {
        tool: tool.to_owned(),
        idempotency_key: idempotency_key.to_owned(),
        origin: commitment.run_id,
        origin_seq: correlation.get(),
    };

    let recorded_input = log
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallRequested {
                seq,
                tool: recorded_tool,
                input,
                ..
            } if *seq == correlation && recorded_tool == tool => Some(input),
            _ => None,
        })
        .ok_or_else(unreadable)?;
    if recorded_input != input {
        return Err(RuntimeError::IdempotencyKeyCollision {
            tool: tool.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            origin: commitment.run_id,
            origin_seq: correlation.get(),
        });
    }

    let output = log
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallCompleted { seq, output, .. } if *seq == correlation => Some(output),
            _ => None,
        })
        .ok_or_else(unreadable)?
        .clone();

    Ok((
        output,
        DedupOrigin {
            run_id: commitment.run_id,
            seq: correlation,
        },
    ))
}

/// Persists a completion and settles its call commitment in one indivisible
/// store operation. The settling counterpart of [`persist`].
async fn persist_settling(
    store: &dyn EventStore,
    run_id: RunId,
    clock: &ClockFn,
    emitted: &Emitted,
    claimant: CallClaimant<'_>,
) -> Result<(), RuntimeError> {
    let envelope = EventEnvelope::new(run_id, emitted.seq, (clock)(), emitted.event.clone());
    store.append_settling_call(&envelope, claimant).await?;
    crate::progress::emit_step(run_id, envelope.seq, &envelope.event);
    Ok(())
}

/// Decodes a recorded completion output into the same [`ToolCallResult`] the
/// live path produced, so replayed orchestration takes the identical branch.
fn decode_tool_output(output: Value) -> ToolCallResult {
    if let Some(suspension) = decode_suspension(&output) {
        return ToolCallResult::Suspended(suspension);
    }
    if let Some(sleep) = decode_sleep(&output) {
        return ToolCallResult::Sleeping(sleep);
    }
    if let Some(failure) = decode_failure(&output) {
        return ToolCallResult::Failed(failure);
    }
    ToolCallResult::Output(output)
}

/// The default random source: 64 bits folded from a freshly drawn version 4
/// UUID, which the `uuid` crate fills from operating-system randomness. Non
/// cryptographic by design; recorded bits only ever seed idempotency keys
/// and user-level derivations.
pub(crate) fn os_random() -> u64 {
    let bits = Uuid::new_v4().as_u128();
    (bits as u64) ^ ((bits >> 64) as u64)
}
