//! [`RuntimeError`]: the one error type every `salvor-runtime` operation
//! returns.
//!
//! The variants fall into three groups:
//!
//! - **Forwarded layers.** [`Replay`](RuntimeError::Replay),
//!   [`Store`](RuntimeError::Store), and [`Model`](RuntimeError::Model) wrap
//!   the typed errors of the crates underneath, unflattened, so a caller can
//!   still match the inner variant. The one that matters most is
//!   `Replay(ReplayError::NeedsReconciliation)`: resuming a run whose log
//!   ends in a write intent with no completion surfaces here, and the runtime
//!   refuses to continue until a human resolves it.
//! - **Serialization edges.** [`RequestEncode`](RuntimeError::RequestEncode)
//!   and [`RecordedResponseDecode`](RuntimeError::RecordedResponseDecode)
//!   mark the two places JSON conversion can fail around a model call.
//! - **Runtime protocol.** Starting a run that already has history, resuming
//!   a run that is not parked, resuming with input the recorded schema
//!   rejects, or naming a run the store does not know.

use salvor_core::{ReplayError, RunId};
use salvor_store::StoreError;
use thiserror::Error;

/// What can go wrong while driving a run.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The replay layer refused to continue: divergence, a malformed log, or
    /// a dangling write intent that needs human reconciliation.
    #[error("replay: {0}")]
    Replay(#[from] ReplayError),

    /// The event store failed to persist or read an event.
    #[error("store: {0}")]
    Store(#[from] StoreError),

    /// A live model call failed after the client's own retries. The run's
    /// log is intact (the intent, if any, is recorded), so the run can be
    /// recovered later; the model intent will be re-issued safely.
    #[error("model call: {0}")]
    Model(#[from] salvor_llm::Error),

    /// A model request could not be serialized to JSON for hashing.
    #[error("model request did not serialize: {0}")]
    RequestEncode(serde_json::Error),

    /// A recorded model response could not be decoded back into a typed
    /// response. This means the log holds something this build cannot read,
    /// which is a storage or versioning fault, not orchestration divergence.
    #[error("recorded model response did not decode: {0}")]
    RecordedResponseDecode(serde_json::Error),

    /// `start` was called for a run id that already has recorded history.
    #[error("run {run_id:?} already has recorded history; use recover or resume")]
    RunAlreadyStarted {
        /// The run that already exists.
        run_id: RunId,
    },

    /// The named run has no recorded history at all.
    #[error("run {run_id:?} has no recorded history")]
    UnknownRun {
        /// The run that was not found.
        run_id: RunId,
    },

    /// `resume` was called on a run whose log does not end at a suspension
    /// or budget crossing.
    #[error("run {run_id:?} is not parked (status: {status}); resume needs a parked run")]
    NotParked {
        /// The run that was not parked.
        run_id: RunId,
        /// A short description of the status the run was actually in.
        status: String,
    },

    /// The resume input did not satisfy the recorded suspension schema (or,
    /// for a budget crossing, the budget-extension shape).
    #[error("resume input rejected: {0}")]
    ResumeInputRejected(String),

    /// `resolve` was called on a run that is not awaiting reconciliation. The
    /// hand-recorded completion is only ever appended to a run whose log ends
    /// at a dangling write intent; every other state is a caller mistake.
    #[error(
        "run {run_id:?} does not need reconciliation (status: {status}); resolve records the completion of a dangling write intent, and this run has none"
    )]
    NotReconcilable {
        /// The run that was not awaiting reconciliation.
        run_id: RunId,
        /// A short description of the status the run was actually in.
        status: String,
    },
}
