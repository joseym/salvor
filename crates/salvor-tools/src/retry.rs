//! [`RetryPolicy`]: the per-effect rule for retrying a *live* tool execution
//! that failed, encoded as pure data.

use salvor_core::Effect;

/// Whether and how a failed live tool execution may be retried, as a function
/// of the tool's [`Effect`].
///
/// This encodes the retry column of the effect table. It is pure data
/// and logic: it decides nothing and enforces nothing on its own.
///
/// # Scope: live execution only
///
/// This governs a tool call that is executing **now** and failed. It says
/// nothing about replay: a *completed* recorded call is read from the log and
/// never re-executed, whatever its effect. That rule lives in
/// [`salvor_core::Effect`] and the replay cursor, not here.
///
/// # Consumer: the agent loop
///
/// This type is consumed by the runtime loop, which owns retry enforcement.
/// When a live [`DynTool::call_json`](crate::DynTool::call_json) returns a
/// [`ToolError::Handler`](crate::ToolError::Handler), the loop asks
/// [`RetryPolicy::for_effect`] what it is allowed to do:
///
/// - [`Retry`](Self::Retry): re-execute the call.
/// - [`RetryWithSameKey`](Self::RetryWithSameKey): re-execute, reusing the
///   original idempotency key (carried on [`ToolCtx`](crate::ToolCtx)) so the
///   provider collapses duplicate attempts into one effect.
/// - [`Never`](Self::Never): do not auto-retry; the write's at-most-once intent
///   was recorded before execution, and a crash between intent and completion
///   surfaces as `NeedsReconciliation` for a human.
///
/// This layer only classifies. Recording intent, counting attempts, and
/// surfacing reconciliation are the loop's job.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RetryPolicy {
    /// Retry freely. A [`Read`](Effect::Read) has no side effect, so a repeat
    /// attempt only re-observes state.
    Retry,
    /// Retry, but only reusing the same idempotency key. For an
    /// [`Idempotent`](Effect::Idempotent) call, the shared key is what makes a
    /// retry safe.
    RetryWithSameKey,
    /// Never auto-retry. A [`Write`](Effect::Write) that may have landed is not
    /// re-attempted on a guess; it is left for human reconciliation.
    Never,
}

impl RetryPolicy {
    /// The retry policy for a tool of the given effect class.
    ///
    /// This is the whole table: [`Read`](Effect::Read) maps to
    /// [`Retry`](Self::Retry), [`Idempotent`](Effect::Idempotent) to
    /// [`RetryWithSameKey`](Self::RetryWithSameKey), and
    /// [`Write`](Effect::Write) to [`Never`](Self::Never).
    pub fn for_effect(effect: Effect) -> Self {
        match effect {
            Effect::Read => RetryPolicy::Retry,
            Effect::Idempotent => RetryPolicy::RetryWithSameKey,
            Effect::Write => RetryPolicy::Never,
        }
    }

    /// Whether a failed live execution of this policy may be re-attempted at
    /// all. A convenience over matching; `false` only for [`Never`](Self::Never).
    pub fn allows_retry(self) -> bool {
        !matches!(self, RetryPolicy::Never)
    }
}
