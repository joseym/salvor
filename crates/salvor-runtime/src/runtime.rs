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
//!   budget-extension shape (see [`crate::budgets`]). Only then is it handed
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

use salvor_core::{Budget, Event, EventEnvelope, PendingCall, RunId, RunStatus, derive_state};
use salvor_store::EventStore;
use serde_json::Value;

use crate::agent::Agent;
use crate::budgets::validate_extension_input;
use crate::ctx::{ClockFn, RandomFn, RunCtx};
use crate::driver::{self, LoopOutcome};
use crate::error::RuntimeError;
use crate::validate::validate_against_schema;

/// Why a run parked instead of completing.
#[derive(Debug, Clone)]
pub enum ParkReason {
    /// A tool suspended the run, awaiting input matching the schema.
    Suspended {
        /// The recorded suspension reason.
        reason: String,
        /// The JSON Schema the resume input must satisfy.
        input_schema: Value,
    },
    /// A declared budget was crossed. Resume may carry an extension.
    BudgetExceeded {
        /// The crossed budget, with its effective limit.
        budget: Budget,
        /// The observed value that crossed it.
        observed: f64,
    },
}

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
        let intent_seq = match (&state.status, &state.pending_call) {
            (RunStatus::NeedsReconciliation, Some(PendingCall::Tool { seq, .. })) => *seq,
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
        };
        let envelope = EventEnvelope::new(run_id, state.next_seq, (self.clock)(), completion);
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
        RunStatus::BudgetExceeded { .. } => "budget exceeded",
        RunStatus::NeedsReconciliation => "needs reconciliation",
        RunStatus::Completed { .. } => "completed",
        RunStatus::Failed { .. } => "failed",
    }
}
