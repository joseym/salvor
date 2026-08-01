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
///
/// `Replay`, `Store`, and `Model` each spell out their inner error's `Display`
/// directly in their own message (`"replay: {0}"` and so on) rather than
/// leaning on `thiserror`'s `#[source]`/`#[from]` chaining for that text. A
/// field that is both interpolated into the message AND wired as the
/// `Error::source()` gets printed twice by anything that walks the source
/// chain on top of `Display` (`anyhow`'s `{:#}`, `{:?}`, and the like): once
/// embedded in this variant's own message, once again as the chain's next
/// link. Plain `From` impls below give `?` the same conversion `#[from]`
/// would without also handing these three a chained source, so the detail
/// appears exactly once no matter how the caller prints the error.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The replay layer refused to continue: divergence, a malformed log, or
    /// a dangling write intent that needs human reconciliation.
    #[error("replay: {0}")]
    Replay(ReplayError),

    /// The event store failed to persist or read an event.
    #[error("store: {0}")]
    Store(StoreError),

    /// A live model call failed after the client's own retries. The run's
    /// log is intact (the intent, if any, is recorded), so the run can be
    /// recovered later; the model intent will be re-issued safely.
    #[error("model call: {0}")]
    Model(salvor_llm::Error),

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

    /// The labels a run is about to be created with violate the sanity
    /// bounds (too many, or a key/value over its length cap). See
    /// [`crate::validate_labels`]. Surfaces only on a genuinely fresh
    /// `begin`; a replayed run never re-checks the labels it already
    /// recorded.
    #[error("invalid labels: {0}")]
    InvalidLabels(String),

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

    /// `abandon` was called on a run that already reached a terminal event
    /// (completed, failed, or previously abandoned). A terminal run is already
    /// at rest; there is nothing left to retire, so the operator action is
    /// refused rather than appending a second terminal.
    #[error(
        "run {run_id:?} is already terminal (status: {status}); there is nothing left to abandon"
    )]
    AlreadyTerminal {
        /// The run that had already finished.
        run_id: RunId,
        /// A short description of the terminal status the run was in.
        status: String,
    },
}

// Plain `From` impls, not `#[from]`: see the doc comment on `RuntimeError`
// for why these three stay unchained.
impl From<ReplayError> for RuntimeError {
    fn from(error: ReplayError) -> Self {
        RuntimeError::Replay(error)
    }
}

impl From<StoreError> for RuntimeError {
    fn from(error: StoreError) -> Self {
        RuntimeError::Store(error)
    }
}

impl From<salvor_llm::Error> for RuntimeError {
    fn from(error: salvor_llm::Error) -> Self {
        RuntimeError::Model(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Joins an error's `Display` with every `source()` below it, exactly the
    /// walk `anyhow`'s `{:#}`/`{:?}` and `salvor_runtime::wire::error_chain`
    /// both do. A variant whose message already embeds its own `#[source]`
    /// field's text, while ALSO exposing that field as the chained source,
    /// would print the same text twice through a walk like this one; that is
    /// the bug a tester hit for `RuntimeError::Model`.
    fn chain(error: &dyn std::error::Error) -> String {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(inner) = source {
            message.push_str(": ");
            message.push_str(&inner.to_string());
            source = inner.source();
        }
        message
    }

    /// Pins the fix: a model call's `500` (the demo model's own
    /// no-conversation-matched error, reproduced by hand here) reads exactly
    /// once whether a caller walks the source chain (`chain`, matching
    /// `anyhow`'s alternate `Display`) or just calls `to_string()` on the bare
    /// `RuntimeError` (matching `ApiError::message()`'s plain `Display` on the
    /// HTTP path). Before the fix, `chain` doubled it: `Model`'s own message
    /// interpolated the inner error's text AND `#[from]` chained that same
    /// field as `source()`.
    #[test]
    fn model_error_prints_once_through_the_source_chain_and_plain_display() {
        let inner = salvor_llm::Error::Api(salvor_llm::ApiError {
            status: 500,
            kind: "demo_script_no_conversation".to_owned(),
            message: "no conversation name matched the system prompt".to_owned(),
            request_id: None,
            retry_after: None,
        });
        let error: RuntimeError = inner.into();

        let needle = "no conversation name matched the system prompt";
        let chained = chain(&error);
        assert_eq!(chained.matches(needle).count(), 1, "{chained}");
        assert_eq!(error.to_string().matches(needle).count(), 1, "{error}");

        // No source to walk past `RuntimeError` itself: that absence is what
        // keeps `chain` from doubling the text back up.
        assert!(std::error::Error::source(&error).is_none());
    }
}
