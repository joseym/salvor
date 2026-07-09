//! [`Runtime`]: the batteries-included entry points over the built-in loop.
//!
//! Three verbs, one per way a run can need driving:
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
//!
//! A `Runtime` owns the store handle plus the injected clock and random
//! source it builds each [`RunCtx`](crate::RunCtx) with. It holds no
//! per-run state at all: dropping it mid-run loses nothing, because every
//! event was persisted the moment it happened. That is the kill -9 story.

use std::sync::Arc;

use salvor_core::{Budget, EventEnvelope, RunId, RunStatus, derive_state};
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
        }
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
        RunCtx::with_hooks(
            self.store.clone(),
            run_id,
            log,
            self.clock.clone(),
            self.random.clone(),
        )
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
