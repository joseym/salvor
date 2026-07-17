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
//! | [`await_resume`](RunCtx::await_resume) | `await_resume` | persist `Resumed` when input was provided |
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

use std::collections::BTreeMap;
use std::sync::Arc;

use salvor_core::{
    Budget, Emitted, EventEnvelope, ModelReply, Outcome, ReplayCursor, RunId, SequenceNumber,
    TokenUsage,
};
use salvor_llm::{Client, MessageAccumulator, MessageRequest, MessageResponse, StreamEvent};
use salvor_store::EventStore;
use salvor_tools::{DynTool, RetryPolicy, Suspension, ToolCtx, ToolError, ToolOutcome};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::RuntimeError;
use crate::hash::hash_value;
use crate::labels::validate_labels;
use crate::model::{response_value, usage_of};
use crate::wire::{
    ToolFailure, decode_failure, decode_suspension, encode_failure, encode_suspension,
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

/// The public durability substrate for one run. See the module docs.
pub struct RunCtx {
    cursor: ReplayCursor,
    store: Arc<dyn EventStore>,
    run_id: RunId,
    clock: ClockFn,
    random: RandomFn,
    resume_input: Option<Value>,
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
    /// reproduces on replay. For a re-executed recorded intent the recorded
    /// key wins, whatever is passed here must match it (the cursor checks).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] on divergence or a dangling write intent;
    /// [`RuntimeError::Store`] when persistence fails. A failing *tool* is
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
        match self
            .cursor
            .tool_call(tool.name(), input, effect, idempotency_key)?
        {
            Outcome::Replayed(output) => Ok(decode_tool_output(output)),
            Outcome::Live(permit) => {
                if let Some(intent) = permit.intent().cloned() {
                    persist(self.store.as_ref(), self.run_id, &self.clock, &intent).await?;
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
                    Err(error) => {
                        let failure = ToolFailure::from_error(&error, attempts);
                        (encode_failure(&failure), ToolCallResult::Failed(failure))
                    }
                };
                let completion = permit.record(output);
                persist(self.store.as_ref(), self.run_id, &self.clock, &completion).await?;
                Ok(result)
            }
        }
    }

    /// Parks the run: records `Suspended { reason, input_schema }`. Follow
    /// with [`await_resume`](Self::await_resume).
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
        match self.cursor.suspend(reason, input_schema)? {
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

/// Decodes a recorded completion output into the same [`ToolCallResult`] the
/// live path produced, so replayed orchestration takes the identical branch.
fn decode_tool_output(output: Value) -> ToolCallResult {
    if let Some(suspension) = decode_suspension(&output) {
        return ToolCallResult::Suspended(suspension);
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
