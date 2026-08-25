//! The one place that maps a run's derived state to the verb that continues
//! it. Both the CLI's `resume` command and the server's resume endpoint call
//! [`classify`], so the two surfaces cannot drift on what a given state means.
//!
//! The mapping is exactly this continuation rule:
//!
//! - a **parked** run (suspended, or budget-exceeded) resumes with input;
//! - a **crashed** run (running, or interrupted mid model or tool step)
//!   recovers with no input;
//! - a **sleeping** run carries its deadline, and the caller decides against
//!   its own clock: due, it re-drives like a crashed one; early, it is
//!   refused and the instant is the evidence;
//! - a run that **needs reconciliation** is refused, and its recorded write
//!   intent is the evidence a human resolves it with;
//! - a **finished** run (completed, failed, or operator-abandoned) is reported
//!   and left alone;
//! - an **empty** log is not a run at all.
//!
//! This module holds only the decision, not the effect: it does no IO, drives
//! nothing, and prints nothing. The caller acts on the [`Disposition`] in the
//! way its surface calls for (an exit code and a report for the CLI, an HTTP
//! status and a JSON body for the server).

use salvor_core::{PendingCall, RunState, RunStatus, UnresolvedWrite};
use time::OffsetDateTime;

/// Whether a resume should validate and expect an input, or run with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    /// The run suspended on a tool; the input is validated against the
    /// recorded suspension schema.
    Suspension,
    /// The run crossed a budget; the input is validated against the
    /// budget-extension shape.
    Budget,
}

/// What to do with a run, decided from its derived state alone.
#[derive(Debug, Clone, PartialEq)]
pub enum Disposition {
    /// The run is parked and resumes with a validated input.
    Resume(ResumeKind),
    /// The run crashed mid-step and recovers with no input.
    Recover,
    /// The run is parked on a durable timer. Carries the recorded wake instant,
    /// which is both the evidence a refusal names and the value the caller
    /// tests its clock against.
    ///
    /// The clock is not read here, and the disposition is not "recover" or
    /// "refuse" on its own, because this module holds no clock and must not:
    /// the same state means "drive it" to a waker whose sweep found the run
    /// due and "refuse" to a person resuming it an hour early, and only the
    /// caller knows which it is. Both surfaces compare `wake_at` against the
    /// clock they already drive with, so neither can decide differently from
    /// the [`RunCtx::await_wake`](salvor_runtime::RunCtx::await_wake) that
    /// enforces the deadline inside the run.
    Sleeping {
        /// The instant the run's recorded `SleepStarted` said it may continue
        /// at.
        wake_at: OffsetDateTime,
    },
    /// The run needs human reconciliation. Carries the dangling write intent
    /// so the caller can show it as evidence.
    Reconcile(PendingCall),
    /// The run already finished with this output.
    Completed(serde_json::Value),
    /// The run already failed with this error.
    Failed(String),
    /// The run was abandoned by an operator. A terminal resting state, reported
    /// and left alone exactly as completed or failed is, distinct from failure.
    Abandoned {
        /// The operator's optional note.
        reason: Option<String>,
        /// The write intent left unsettled when a needs-reconciliation run was
        /// abandoned, when there was one.
        unresolved_write: Option<UnresolvedWrite>,
    },
    /// The log is empty; there is no run to continue.
    NotStarted,
}

/// Maps a derived [`RunState`] to its [`Disposition`].
#[must_use]
pub fn classify(state: &RunState) -> Disposition {
    match &state.status {
        RunStatus::Suspended { .. } => Disposition::Resume(ResumeKind::Suspension),
        RunStatus::BudgetExceeded { .. } => Disposition::Resume(ResumeKind::Budget),
        RunStatus::Running | RunStatus::AwaitingModel | RunStatus::AwaitingTool => {
            Disposition::Recover
        }
        // A sleeping run continues by being driven with no input, which is
        // mechanically a recovery, so waking still needs no verb: both wakers
        // (`salvor wake`, the server's sweeper) re-drive a due run through the
        // ordinary path. What this arm will not do is answer for a caller that
        // is early. Driving early was always harmless (`RunCtx::await_wake`
        // reads the clock, records nothing, and leaves the run asleep) but it
        // was also silent, and a person who typed `salvor resume` deserves to
        // be told the run is on a timer and how long is left rather than to
        // watch a no-op. So the deadline travels out of here and the caller
        // decides.
        RunStatus::Sleeping { wake_at } => Disposition::Sleeping { wake_at: *wake_at },
        RunStatus::NeedsReconciliation => {
            // A needs-reconciliation state always carries the pending write
            // intent whose completion is missing; if it somehow did not, there
            // is still nothing to drive, so recovery would refuse it too.
            match &state.pending_call {
                Some(pending @ PendingCall::Tool { .. }) => Disposition::Reconcile(pending.clone()),
                _ => Disposition::Recover,
            }
        }
        RunStatus::Completed { output } => Disposition::Completed(output.clone()),
        RunStatus::Failed { error } => Disposition::Failed(error.clone()),
        RunStatus::Abandoned {
            reason,
            unresolved_write,
        } => Disposition::Abandoned {
            reason: reason.clone(),
            unresolved_write: unresolved_write.clone(),
        },
        RunStatus::NotStarted => Disposition::NotStarted,
    }
}
