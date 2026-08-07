//! The one place that maps a run's derived state to the verb that continues
//! it. Both the CLI's `resume` command and the server's resume endpoint call
//! [`classify`], so the two surfaces cannot drift on what a given state means.
//!
//! The mapping is exactly this continuation rule:
//!
//! - a **parked** run (suspended, or budget-exceeded) resumes with input;
//! - a **crashed** run (running, or interrupted mid model or tool step)
//!   recovers with no input;
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
        // A sleeping run continues by being driven with no input, which is what
        // `Recover` already means mechanically, so it takes that disposition
        // rather than a new one. No log reaches here yet: nothing records the
        // sleep events, and the wake path that will drive a sleeping run once
        // its deadline passes is not built. This arm exists to keep the
        // classification total, and the surface that owns waking should give
        // sleeping its own considered treatment.
        RunStatus::Sleeping { .. } => Disposition::Recover,
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
