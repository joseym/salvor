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
//!   rejects, naming a run the store does not know, or asking to sleep past
//!   the end of representable time.

use salvor_core::{ReplayError, RunId};
use salvor_store::StoreError;
use thiserror::Error;
use time::{Duration, OffsetDateTime};

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

    /// A sleep asked for a wake instant no timestamp can hold: the duration
    /// added to the observed clock reading falls outside the representable
    /// range. Refused rather than clamped, because a silently shortened
    /// deadline is a run that wakes at a time nobody asked for.
    #[error("sleep of {duration:?} from {now:?} overflows the representable range of an instant")]
    SleepOverflow {
        /// The recorded clock reading the sleep was derived from.
        now: OffsetDateTime,
        /// The duration asked for.
        duration: Duration,
    },

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

    /// A structured-output drive was asked to run for an agent that already
    /// offers a real tool named `salvor_answer`.
    ///
    /// Under a declared output schema the loop offers a synthetic tool of that
    /// name and reads a call to it as the final answer. A real tool sharing the
    /// name would make the two indistinguishable in the response, so the drive
    /// refuses before its first model call and records nothing.
    #[error(
        "the agent offers a tool named `salvor_answer`, the name a declared output schema reserves for its answer call; rename the tool or drop the schema"
    )]
    AnswerToolNameTaken,

    /// A keyed call could not proceed because another run holds the same
    /// `(tool, idempotency key)` identity and has not finished with it.
    ///
    /// The holder is either running right now or died mid-call. Either way the
    /// effect may or may not have happened, and this run has no way to find out
    /// and no right to try: proceeding would be exactly the second execution
    /// the key exists to prevent. So the call refuses, and it refuses *before*
    /// recording anything, which leaves this run's log untouched and the run
    /// re-runnable once the holder is finished or reconciled.
    ///
    /// The resolution lives in the holding run, never here. Finish it, or
    /// reconcile its dangling write with `salvor resolve`, and then run this
    /// one again.
    #[error(
        "tool `{tool}` under idempotency key `{idempotency_key}` is held by run {holder:?} at seq {holder_seq}, which has not recorded a completion; nothing was executed and nothing was recorded. Finish or reconcile that run before running this one again"
    )]
    CallInFlight {
        /// The tool whose identity is held.
        tool: String,
        /// The idempotency key naming the effect.
        idempotency_key: String,
        /// The run holding the identity.
        holder: RunId,
        /// The position of the holder's intent for this call.
        holder_seq: u64,
    },

    /// Two different calls presented the same `(tool, idempotency key)`
    /// identity with different inputs.
    ///
    /// The key is a promise that two calls are the same call. Different inputs
    /// under one key break that promise, and there is no safe reading of it:
    /// deduplicating would hand this call an output computed from somebody
    /// else's arguments, and executing would perform an effect the key says has
    /// already been performed. So neither happens and the key's author is told.
    ///
    /// The fix is in the key, not here. A key must be specific enough to name
    /// one effect: `"pay_claim:wreck-9931"`, not `"pay_claim"`.
    #[error(
        "tool `{tool}` was called under idempotency key `{idempotency_key}` with an input that differs from the call run {origin:?} already committed at seq {origin_seq}; the key names two different calls, so neither deduplicating nor executing is safe"
    )]
    IdempotencyKeyCollision {
        /// The tool whose key collided.
        tool: String,
        /// The key that named two different calls.
        idempotency_key: String,
        /// The run holding the committed call.
        origin: RunId,
        /// The position of the committed call's intent.
        origin_seq: u64,
    },

    /// A commitment pointed at a completion that its run's log does not hold.
    ///
    /// The store said an identity was settled at a position, and reading that
    /// run's log (chain verification included) did not produce the completion
    /// there. That is a damaged store, not a race: a settlement and its
    /// completion are written as one unit, so one cannot exist without the
    /// other. Reported rather than worked around, because the alternative would
    /// be executing an effect the store believes already happened.
    #[error(
        "run {origin:?} was committed to tool `{tool}` under idempotency key `{idempotency_key}` at seq {origin_seq}, but its log holds no such completion; the store disagrees with itself and nothing was executed"
    )]
    CommitmentUnreadable {
        /// The tool named by the commitment.
        tool: String,
        /// The key named by the commitment.
        idempotency_key: String,
        /// The run the commitment pointed at.
        origin: RunId,
        /// The position the commitment pointed at.
        origin_seq: u64,
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
