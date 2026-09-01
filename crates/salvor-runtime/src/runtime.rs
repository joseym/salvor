//! [`Runtime`]: the batteries-included entry points over the built-in loop.
//!
//! Four verbs, one per way a run can need driving:
//!
//! - [`start`](Runtime::start) mints a run id and drives a fresh run.
//! - [`recover`](Runtime::recover) re-drives an interrupted (crashed) run
//!   over its recorded log: recorded steps replay, execution continues live
//!   from the first unrecorded step. Driving an already-completed run this
//!   way replays it end to end and is the cheapest full divergence check.
//! - [`resume`](Runtime::resume) supplies input to a *parked* run (one whose
//!   log ends at a `Suspended` or `BudgetExceeded` event). The input is
//!   validated first: against the recorded suspension `input_schema` (see
//!   [`crate::validate`] for what validation means in v0.1), or against the
//!   budget-extension shape (see [`validate_extension_input`](crate::validate_extension_input)). Only then is it handed
//!   to the loop, which records it as the `Resumed` event at the parked
//!   position, through the cursor like every other event.
//! - [`resolve`](Runtime::resolve) is the one human-driven override: it
//!   records the completion of a dangling `Write` intent by hand, after a
//!   human has verified externally what the write actually did. It executes
//!   nothing and drives nothing; it appends exactly one `ToolCallCompleted`
//!   so the run leaves `NeedsReconciliation` and a later `recover` can
//!   continue it. Every other verb refuses a dangling write, by design; this
//!   verb is the sanctioned way past it.
//!
//! A `Runtime` owns the store handle plus the injected clock and random
//! source it builds each [`RunCtx`](crate::RunCtx) with. It holds no
//! per-run state at all: dropping it mid-run loses nothing, because every
//! event was persisted the moment it happened. That is the kill -9 story.

use std::collections::BTreeMap;
use std::sync::Arc;

use salvor_core::{
    Event, EventEnvelope, PendingCall, RunId, RunStatus, SettledBy, UnresolvedWrite, derive_state,
};
use salvor_store::{CallClaimant, EventStore};
use serde_json::Value;

// `ParkReason` is a plain value over the event vocabulary, so it lives in the
// pure `salvor-replay` crate that `salvor-core` re-exports rather than at this
// IO edge. Re-exported here so every `salvor_runtime::ParkReason` and
// `crate::runtime::ParkReason` path keeps resolving.
pub use salvor_core::ParkReason;

use crate::agent::Agent;
use crate::ctx::{ClockFn, RandomFn, RunCtx};
use crate::driver::{self, LoopOutcome};
use crate::error::RuntimeError;
use crate::validate::validate_against_schema;
use salvor_core::validate_extension_input;

/// How a drive of a run ended.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// The run completed with this output.
    Completed {
        /// The run that completed.
        run_id: RunId,
        /// The recorded final output.
        output: Value,
    },
    /// The run is parked durably; it survives restarts and deploys, and
    /// [`Runtime::resume`] continues it once input arrives.
    Parked {
        /// The parked run.
        run_id: RunId,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// The batteries-included runtime. See the module docs for the three verbs.
pub struct Runtime {
    store: Arc<dyn EventStore>,
    clock: ClockFn,
    random: RandomFn,
    /// Whether the [`RunCtx`](crate::RunCtx) this runtime builds records the
    /// full model request body. Off unless
    /// [`with_record_prompts`](Runtime::with_record_prompts) turns it on.
    record_prompts: bool,
    /// Correlation tags every [`RunCtx`](crate::RunCtx) this runtime builds
    /// stamps on a genuinely fresh run. Unset unless
    /// [`with_labels`](Runtime::with_labels) sets them.
    labels: Option<BTreeMap<String, String>>,
    /// The name every event this runtime records on someone's behalf carries.
    /// Unset unless [`with_caller`](Runtime::with_caller) sets it.
    caller: Option<String>,
}

impl Runtime {
    /// A runtime over `store` with the default clock and OS randomness.
    #[must_use]
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self::with_hooks(
            store,
            Arc::new(time::OffsetDateTime::now_utc),
            Arc::new(crate::ctx::os_random),
        )
    }

    /// A runtime with an injected clock and random source, handed to every
    /// [`RunCtx`](crate::RunCtx) it builds. Deterministic tests inject fixed
    /// functions so full event logs compare equal across runs.
    #[must_use]
    pub fn with_hooks(store: Arc<dyn EventStore>, clock: ClockFn, random: RandomFn) -> Self {
        Self {
            store,
            clock,
            random,
            record_prompts: false,
            labels: None,
            caller: None,
        }
    }

    /// Turns on recording of the full model request body for every run this
    /// runtime drives, passed through to each [`RunCtx`](crate::RunCtx) it
    /// builds. Off by default. Chained builder style so
    /// [`new`](Self::new)/[`with_hooks`](Self::with_hooks) keep their
    /// signatures and every existing caller stays at off.
    ///
    /// This is PII-sensitive (the body may hold user data or secrets), which is
    /// why it is off unless an operator opts in. See
    /// [`RunCtx::with_record_prompts`](crate::RunCtx::with_record_prompts) for
    /// what recording means and the guarantee that it does not affect replay.
    #[must_use]
    pub fn with_record_prompts(mut self, record_prompts: bool) -> Self {
        self.record_prompts = record_prompts;
        self
    }

    /// Sets the correlation tags every run this runtime drives stamps on its
    /// `RunStarted`, passed through to each [`RunCtx`](crate::RunCtx) it
    /// builds. Unset by default. Chained builder style, mirroring
    /// [`with_record_prompts`](Self::with_record_prompts); see
    /// [`RunCtx::with_labels`](crate::RunCtx::with_labels) for the bounds
    /// that apply and when they are checked.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// Sets the name of whoever asked for the work this runtime does, stamped
    /// on every event it records on their behalf: `caller` on a fresh
    /// `RunStarted` or `GraphRunStarted`, on a live `Resumed`, on the
    /// `RunRedriven` [`record_redrive`](Self::record_redrive) appends, and on
    /// the terminal [`abandon`](Self::abandon) appends, and `settled_caller` on
    /// the completion [`resolve`](Self::resolve) records.
    ///
    /// The surface that took the request supplies the name: a server with a
    /// bearer configured passes the token's name, the CLI passes the operating
    /// system user. Unset by default and unset is honest, so a surface with
    /// nobody to name records nothing rather than inventing a name. Chained
    /// builder style, mirroring [`with_labels`](Self::with_labels); the
    /// per-run half is [`RunCtx::with_caller`](crate::RunCtx::with_caller),
    /// which this passes it through to.
    #[must_use]
    pub fn with_caller(mut self, caller: impl Into<String>) -> Self {
        self.caller = Some(caller.into());
        self
    }

    /// Starts a fresh run of `agent` with `input`, under a newly minted
    /// run id.
    ///
    /// # Errors
    ///
    /// Everything [`start_with_id`](Self::start_with_id) returns.
    pub async fn start(&self, agent: &Agent, input: Value) -> Result<RunOutcome, RuntimeError> {
        self.start_with_id(agent, RunId::new(), input).await
    }

    /// Starts a fresh run under a caller-chosen run id (tests use this to
    /// make logs comparable across control and killed runs).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::RunAlreadyStarted`] when the id already has history;
    /// otherwise whatever the loop surfaces ([`RuntimeError::Store`],
    /// [`RuntimeError::Model`], [`RuntimeError::Replay`], ...).
    pub async fn start_with_id(
        &self,
        agent: &Agent,
        run_id: RunId,
        input: Value,
    ) -> Result<RunOutcome, RuntimeError> {
        let log = self.store.read_log(run_id).await?;
        if !log.is_empty() {
            return Err(RuntimeError::RunAlreadyStarted { run_id });
        }
        let mut ctx = self.ctx(run_id, log)?;
        finish(run_id, driver::drive(&mut ctx, agent, &input).await?)
    }

    /// Re-drives an interrupted run: replays the recorded log, then
    /// continues live from the first unrecorded step. This is the
    /// post-crash verb; it supplies no new input.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownRun`] when the id has no history;
    /// `RuntimeError::Replay(ReplayError::NeedsReconciliation)` when the log
    /// ends in a write intent with no completion (a human must resolve it);
    /// [`RuntimeError::Replay`] on any divergence.
    pub async fn recover(&self, agent: &Agent, run_id: RunId) -> Result<RunOutcome, RuntimeError> {
        let log = self.read_existing(run_id).await?;
        let mut ctx = self.ctx(run_id, log)?;
        finish(run_id, driver::drive(&mut ctx, agent, &Value::Null).await?)
    }

    /// Records that a crashed or sleeping run is about to be driven again,
    /// naming whoever asked. Appends exactly one [`Event::RunRedriven`] and
    /// drives nothing.
    ///
    /// Every redrive path calls this immediately before it drives: the server's
    /// resume endpoint on a crashed or due run, the wake sweeper, `salvor
    /// resume`, and `salvor wake`. A redrive is otherwise invisible. Recovering
    /// and waking both continue with nothing supplied, so neither writes a
    /// `Resumed`, and every event the drive goes on to record belongs to the
    /// run's own work rather than to whoever reached for it. Appending before
    /// the drive is what makes a drive that records nothing (an early wake, a
    /// lost position race) still say who tried.
    ///
    /// The name is [`with_caller`](Self::with_caller)'s, so a pass-through
    /// server and a CLI with no account to read record the act with no name
    /// rather than an invented one.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownRun`] when the id has no history;
    /// [`RuntimeError::AlreadyTerminal`] when the run already reached a
    /// terminal event, so there is nothing left to drive;
    /// [`RuntimeError::Store`] when the append fails, which includes a
    /// `Conflict` when another driver reached the same position first.
    pub async fn record_redrive(&self, run_id: RunId) -> Result<RunId, RuntimeError> {
        let log = self.read_existing(run_id).await?;
        let state = derive_state(&log);
        // A terminal log is closed: an event after `RunCompleted`, `RunFailed`,
        // or `RunAbandoned` is malformed, and replay refuses such a log outright.
        // No caller reaches here with one (each classifies the run first), so
        // this guards a bug rather than an ordinary path.
        if matches!(
            state.status,
            RunStatus::Completed { .. } | RunStatus::Failed { .. } | RunStatus::Abandoned { .. }
        ) {
            return Err(RuntimeError::AlreadyTerminal {
                run_id,
                status: status_name(&state.status).to_owned(),
            });
        }
        let event = Event::RunRedriven {
            caller: self.caller.clone(),
        };
        let envelope = EventEnvelope::new(run_id, state.next_seq, (self.clock)(), event);
        self.store.append(&envelope).await?;
        // The shipped log carries the redrive too, and names the caller as a
        // field rather than inside a message, so an operator can filter on it.
        // This line stands in for the `emit_step` every other append makes: a
        // redrive is not a step of the run's work, and one record per redrive
        // is the whole of what a reader needs.
        tracing::info!(
            run_id = %run_id.as_uuid(),
            seq = envelope.seq.get(),
            caller = self.caller.as_deref().unwrap_or("<unnamed>"),
            "driving this run again"
        );
        Ok(run_id)
    }

    /// Resumes a parked run with `input`.
    ///
    /// The run must be parked: its derived status must be `Suspended` or
    /// `BudgetExceeded`. The input is validated before anything is recorded:
    /// a suspension validates against its recorded `input_schema`, a budget
    /// crossing against the extension shape. On success, the loop re-drives
    /// the run; the input is recorded as `Resumed` at the parked position
    /// and becomes the pending tool's result (or the budget extension).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownRun`], [`RuntimeError::NotParked`], or
    /// [`RuntimeError::ResumeInputRejected`]; then whatever the loop
    /// surfaces.
    pub async fn resume(
        &self,
        agent: &Agent,
        run_id: RunId,
        input: Value,
    ) -> Result<RunOutcome, RuntimeError> {
        let log = self.read_existing(run_id).await?;
        let state = derive_state(&log);
        match &state.status {
            RunStatus::Suspended { input_schema, .. } => {
                validate_against_schema(&input, input_schema)
                    .map_err(RuntimeError::ResumeInputRejected)?;
            }
            RunStatus::BudgetExceeded { .. } => {
                validate_extension_input(&input).map_err(RuntimeError::ResumeInputRejected)?;
            }
            other => {
                return Err(RuntimeError::NotParked {
                    run_id,
                    status: status_name(other).to_owned(),
                });
            }
        }
        let mut ctx = self.ctx(run_id, log)?;
        ctx.set_resume_input(input);
        finish(run_id, driver::drive(&mut ctx, agent, &Value::Null).await?)
    }

    /// Records, by hand, the completion of a dangling `Write` intent, after a
    /// human has verified externally what the write did.
    ///
    /// This is the concrete form of human resolution. A crash
    /// between a write's recorded intent and its completion derives to
    /// [`RunStatus::NeedsReconciliation`], which every automatic verb refuses:
    /// the write may or may not have reached its target, and the runtime will
    /// not guess. Once a human has checked, `resolve` appends the completion
    /// they observed (or the completion of the write they performed by hand),
    /// so replay treats the call as done and never re-executes it. The run is
    /// then recoverable through [`recover`](Self::recover) like any other.
    ///
    /// It takes the same care as [`resume`](Self::resume): the state is
    /// validated *before* anything is written, and exactly one event is
    /// appended. `output` is recorded verbatim as the tool's output; nothing
    /// executes and nothing else is driven.
    ///
    /// When the resolved call held a cross-run commitment for its
    /// `(tool, idempotency key)`, this completion settles it, in the same
    /// atomic step. A human resolving a payment is the moment that payment
    /// stops being in flight, and the store has to learn it from somewhere;
    /// leaving the commitment open would refuse every later run under that key
    /// forever, with nothing anywhere to say why.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownRun`] when the id has no history;
    /// [`RuntimeError::NotReconcilable`] when the run's log does not end at a
    /// dangling write intent (so there is no completion to record);
    /// [`RuntimeError::Store`] when the append fails.
    pub async fn resolve(&self, run_id: RunId, output: Value) -> Result<RunId, RuntimeError> {
        let log = self.read_existing(run_id).await?;
        let state = derive_state(&log);
        // Only a dangling write derives to NeedsReconciliation, and it always
        // carries the pending tool intent whose completion is missing.
        let (intent_seq, tool, idempotency_key) = match (&state.status, &state.pending_call) {
            (
                RunStatus::NeedsReconciliation,
                Some(PendingCall::Tool {
                    seq,
                    tool,
                    idempotency_key,
                    ..
                }),
            ) => (*seq, tool.clone(), idempotency_key.clone()),
            (other, _) => {
                return Err(RuntimeError::NotReconcilable {
                    run_id,
                    status: status_name(other).to_owned(),
                });
            }
        };
        // The completion correlates to the intent's sequence number and takes
        // the next contiguous log position, exactly as the cursor would have
        // recorded it had the process not died in between.
        let completion = Event::ToolCallCompleted {
            seq: intent_seq,
            output,
            // A human reconciled this write by hand. Nothing was copied from
            // another run, so there is no origin to name.
            deduplicated_from: None,
            // The one place a completion is not the run recording what it saw.
            // Every caller of this method is a person acting over the run's
            // head (`salvor resolve`, and both resolve endpoints), and the
            // output below is their report of an effect nothing in this
            // process witnessed, so the completion says who recorded it.
            settled_by: Some(SettledBy::Operator),
            // And, when the surface that took the request had a name to give,
            // which one of them. `settled_by` says a person did this;
            // `settled_caller` says which person. See
            // [`with_caller`](Self::with_caller).
            settled_caller: self.caller.clone(),
        };
        let envelope = EventEnvelope::new(run_id, state.next_seq, (self.clock)(), completion);

        // If this call held a cross-run commitment, the human's completion is
        // the one that settles it. Skipping this would leave the key held by a
        // run that is finished, and every future call under it refused forever:
        // the human resolved the write and would have no way to tell that the
        // key was still stuck.
        let held = match &idempotency_key {
            Some(key) => self.store.lookup_call(&tool, key).await?,
            None => None,
        };
        let unsettled_here = held.is_some_and(|commitment| {
            commitment.run_id == run_id
                && commitment.intent_seq == intent_seq
                && commitment.completion_seq.is_none()
        });
        if unsettled_here {
            let key = idempotency_key.as_deref().expect("checked above");
            self.store
                .append_settling_call(
                    &envelope,
                    CallClaimant {
                        tool: &tool,
                        idempotency_key: key,
                        run_id,
                        intent_seq,
                    },
                )
                .await?;
        } else {
            self.store.append(&envelope).await?;
        }
        crate::progress::emit_step(run_id, envelope.seq, &envelope.event);
        Ok(run_id)
    }

    /// Abandons a run: appends a terminal [`Event::RunAbandoned`] by hand,
    /// retiring a run deliberately without finishing or failing it.
    ///
    /// An operator action, not a driver action. It executes nothing, drives
    /// nothing, and needs no lease: abandonment is the sanctioned "we do not
    /// care about this run anymore" path, appended straight to the log the way
    /// [`resolve`](Self::resolve) appends its one completion. It is allowed for
    /// any non-terminal run, whatever state it parked or crashed in.
    ///
    /// When the run is parked at a dangling write (status
    /// [`RunStatus::NeedsReconciliation`]), the outstanding intent's position
    /// and tool ride on the event as
    /// [`unresolved_write`](Event::RunAbandoned::unresolved_write). The
    /// abandonment never claims the write question was answered: it records
    /// exactly which write was left unsettled, so the honesty the reconciliation
    /// refusal carried is preserved in the terminal record rather than erased.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownRun`] when the id has no history;
    /// [`RuntimeError::AlreadyTerminal`] when the run already reached a terminal
    /// event (completed, failed, or previously abandoned), so there is nothing
    /// left to retire; [`RuntimeError::Store`] when the append fails.
    pub async fn abandon(
        &self,
        run_id: RunId,
        reason: Option<String>,
    ) -> Result<RunId, RuntimeError> {
        let log = self.read_existing(run_id).await?;
        let state = derive_state(&log);
        // Refuse a run that already reached a terminal event: there is nothing
        // left to abandon. Every non-terminal state is fair game.
        if matches!(
            state.status,
            RunStatus::Completed { .. } | RunStatus::Failed { .. } | RunStatus::Abandoned { .. }
        ) {
            return Err(RuntimeError::AlreadyTerminal {
                run_id,
                status: status_name(&state.status).to_owned(),
            });
        }
        // A run parked at a dangling write carries the outstanding intent
        // forward as recorded honesty: name the write whose effect stays
        // unknown rather than pretend it settled. Every other state abandons
        // with no unresolved-write evidence.
        let unresolved_write = match (&state.status, &state.pending_call) {
            (RunStatus::NeedsReconciliation, Some(PendingCall::Tool { seq, tool, .. })) => {
                Some(UnresolvedWrite {
                    seq: *seq,
                    tool: tool.clone(),
                })
            }
            _ => None,
        };
        let event = Event::RunAbandoned {
            reason,
            unresolved_write,
            // Abandonment is the one terminal a person appends by hand, so the
            // record says which person, when the surface that took the request
            // had a name to give. See [`with_caller`](Self::with_caller).
            caller: self.caller.clone(),
        };
        let envelope = EventEnvelope::new(run_id, state.next_seq, (self.clock)(), event);
        self.store.append(&envelope).await?;
        crate::progress::emit_step(run_id, envelope.seq, &envelope.event);
        Ok(run_id)
    }

    /// Reads a run's log, insisting it exists.
    async fn read_existing(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, RuntimeError> {
        let log = self.store.read_log(run_id).await?;
        if log.is_empty() {
            return Err(RuntimeError::UnknownRun { run_id });
        }
        Ok(log)
    }

    /// Builds the per-run context with this runtime's hooks.
    fn ctx(&self, run_id: RunId, log: Vec<EventEnvelope>) -> Result<RunCtx, RuntimeError> {
        let mut ctx = RunCtx::with_hooks(
            self.store.clone(),
            run_id,
            log,
            self.clock.clone(),
            self.random.clone(),
        )?
        .with_record_prompts(self.record_prompts);
        if let Some(labels) = &self.labels {
            ctx = ctx.with_labels(labels.clone());
        }
        if let Some(caller) = &self.caller {
            ctx = ctx.with_caller(caller.clone());
        }
        Ok(ctx)
    }
}

/// Attaches the run id to a loop outcome.
#[allow(clippy::unnecessary_wraps)]
fn finish(run_id: RunId, outcome: LoopOutcome) -> Result<RunOutcome, RuntimeError> {
    Ok(match outcome {
        LoopOutcome::Completed(output) => RunOutcome::Completed { run_id, output },
        LoopOutcome::Parked(reason) => RunOutcome::Parked { run_id, reason },
    })
}

/// A short status name for the not-parked error message.
fn status_name(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::NotStarted => "not started",
        RunStatus::Running => "running",
        RunStatus::AwaitingModel => "awaiting model (interrupted; use recover)",
        RunStatus::AwaitingTool => "awaiting tool (interrupted; use recover)",
        RunStatus::Suspended { .. } => "suspended",
        RunStatus::Sleeping { .. } => "sleeping",
        RunStatus::BudgetExceeded { .. } => "budget exceeded",
        RunStatus::NeedsReconciliation => "needs reconciliation",
        RunStatus::Completed { .. } => "completed",
        RunStatus::Failed { .. } => "failed",
        RunStatus::Abandoned { .. } => "abandoned",
    }
}
