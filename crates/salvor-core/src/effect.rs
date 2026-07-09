//! Side-effect classification for tool calls.

use serde::{Deserialize, Serialize};

/// How a tool call may be retried and how it behaves on replay.
///
/// Effect classification does not change one thing: on replay, **any completed
/// recorded call is read from the log and never re-executed, regardless of its
/// effect class**. A completion is immutable history.
///
/// Effect classification governs calls that were **not** completed, the gap
/// between an intent recorded in the log and a completion that never arrived:
///
/// - [`Effect::Read`] calls re-execute freely.
/// - [`Effect::Idempotent`] calls retry with the same idempotency key.
/// - [`Effect::Write`] calls are never automatically retried and surface for
///   human reconciliation.
///
/// Serializes lowercase (`"read"`, `"idempotent"`, `"write"`), because the
/// wire form is part of the durable event contract.
///
/// This type lives in `salvor-core` because events reference it;
/// `salvor-tools` consumes it when it defines tool contracts.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// A call with no side effects. If it was not completed, it re-executes
    /// freely, because running it again observes state without changing it.
    Read,
    /// A call safe to repeat under the same idempotency key. If it was not
    /// completed, it retries with that same key, so the provider collapses
    /// duplicate attempts into one effect.
    Idempotent,
    /// A call that changes external state and is not safe to repeat blindly.
    /// If it was not completed, it is never automatically retried: a crash
    /// between the recorded intent and a completion surfaces for human
    /// reconciliation rather than a guess about whether the write landed.
    Write,
}
