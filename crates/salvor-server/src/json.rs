//! Turning derived run state into the JSON shapes the control-plane returns.
//!
//! The core [`RunState`] and its parts are Rust enums with data; the wire
//! wants stable, self-describing objects a thin SDK can read without knowing
//! the Rust types. This module is that translation, and it is the one place
//! that fixes those shapes, so every endpoint returns a run's status the same
//! way.
//!
//! # The status object
//!
//! A status is always `{ "state": "<name>", ... }`, where `state` is a stable
//! snake_case token and any extra keys carry that state's data:
//!
//! - `{ "state": "running" }`, `{ "state": "awaiting_model" }`,
//!   `{ "state": "awaiting_tool" }`, `{ "state": "not_started" }`,
//!   `{ "state": "needs_reconciliation" }` carry nothing more.
//! - `{ "state": "suspended", "reason": "...", "input_schema": { ... } }`,
//!   with `"kind": "signal"` added when the run is waiting on an external
//!   system rather than a person. The key is omitted for a human gate, which
//!   is what a suspension recorded before the discriminator existed meant, so
//!   a client that has never heard of it reads every old and new gate exactly
//!   as it did before.
//! - `{ "state": "sleeping", "wake_at": "<RFC 3339>" }`: the run is parked on
//!   a durable timer until that instant, which is a different thing from
//!   `suspended` and never reported as one, because nothing is waiting on a
//!   human.
//! - `{ "state": "budget_exceeded", "budget": { "kind": "...", "limit": n },
//!    "observed": n }`
//! - `{ "state": "completed", "output": <json> }`
//! - `{ "state": "failed", "error": "..." }`
//! - `{ "state": "abandoned" }`, optionally with `"reason": "..."` and, when a
//!   needs-reconciliation run was abandoned,
//!   `"unresolved_write": { "seq": n, "tool": "..." }`: the recorded evidence
//!   that the abandonment never claimed the dangling write settled.
//!
//! # The pending object
//!
//! A dangling call intent is `null` when there is none, or one of:
//!
//! - `{ "kind": "model", "seq": n, "request_hash": "..." }`
//! - `{ "kind": "tool", "seq": n, "tool": "...", "input": <json>,
//!    "effect": "read|idempotent|write", "idempotency_key": "..."|null }`

use salvor_core::{PendingCall, RunState, RunStatus};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The status object for a derived status. See the module docs for the shapes.
#[must_use]
pub fn status(status: &RunStatus) -> Value {
    match status {
        RunStatus::NotStarted => json!({ "state": "not_started" }),
        RunStatus::Running => json!({ "state": "running" }),
        RunStatus::AwaitingModel => json!({ "state": "awaiting_model" }),
        RunStatus::AwaitingTool => json!({ "state": "awaiting_tool" }),
        RunStatus::Suspended {
            reason,
            input_schema,
            kind,
        } => {
            let mut obj = json!({
                "state": "suspended",
                "reason": reason,
                "input_schema": input_schema,
            });
            // Omitted rather than sent as null, the same absent-is-absent rule
            // `abandoned` follows below: a gate says nothing about what it
            // waits on, because a person is the assumption.
            if let Some(kind) = kind {
                obj.as_object_mut()
                    .expect("status object")
                    .insert("kind".to_owned(), json!(kind));
            }
            obj
        }
        RunStatus::Sleeping { wake_at } => json!({
            "state": "sleeping",
            "wake_at": rfc3339(*wake_at),
        }),
        RunStatus::BudgetExceeded { budget, observed } => json!({
            "state": "budget_exceeded",
            "budget": budget,
            "observed": observed,
        }),
        RunStatus::NeedsReconciliation => json!({ "state": "needs_reconciliation" }),
        RunStatus::Completed { output } => json!({ "state": "completed", "output": output }),
        RunStatus::Failed { error } => json!({ "state": "failed", "error": error }),
        RunStatus::Abandoned {
            reason,
            unresolved_write,
        } => {
            let mut obj = json!({ "state": "abandoned" });
            let map = obj.as_object_mut().expect("status object");
            // Omit rather than assert: a reasonless abandonment carries no
            // `reason` key, and only a needs-reconciliation abandonment carries
            // `unresolved_write`: the same zero-vs-absent honesty the rest of
            // the API holds to.
            if let Some(reason) = reason {
                map.insert("reason".to_owned(), json!(reason));
            }
            if let Some(write) = unresolved_write {
                map.insert(
                    "unresolved_write".to_owned(),
                    json!({ "seq": write.seq.get(), "tool": write.tool }),
                );
            }
            obj
        }
    }
}

/// The pending-call object, or `null` when there is no dangling intent.
#[must_use]
pub fn pending(pending: Option<&PendingCall>) -> Value {
    match pending {
        None => Value::Null,
        Some(PendingCall::Model { seq, request_hash }) => json!({
            "kind": "model",
            "seq": seq.get(),
            "request_hash": request_hash,
        }),
        Some(PendingCall::Tool {
            seq,
            tool,
            input,
            effect,
            idempotency_key,
        }) => json!({
            "kind": "tool",
            "seq": seq.get(),
            "tool": tool,
            "input": input,
            "effect": effect,
            "idempotency_key": idempotency_key,
        }),
    }
}

/// Formats a recorded instant as RFC 3339, the wire form every timestamp this
/// API returns takes.
///
/// Infallible in practice, and deliberately: normalizing to UTC rules out an
/// offset with seconds, and without the `time` crate's `large-dates` feature
/// an `OffsetDateTime` cannot hold a year outside 0000..=9999. Those are the
/// only two ways RFC 3339 formatting fails, and a recorded instant on this
/// server can hit neither, so `unwrap_or_default` would only ever hide a bug
/// behind a silently empty `"wake_at": ""` on the wire rather than surface
/// it; matches `salvor_runtime::wire`'s private `rfc3339`.
pub(crate) fn rfc3339(timestamp: OffsetDateTime) -> String {
    timestamp
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .expect("an instant a run can hold formats as RFC 3339 in UTC")
}

/// The full derived-state object: the dry-run replay projection a client gets
/// from the run and replay endpoints (status, usage, next position, pending
/// intent). Nothing here executes; it is a pure fold of the recorded log.
#[must_use]
pub fn run_state(state: &RunState) -> Value {
    json!({
        "status": status(&state.status),
        "usage": {
            "input_tokens": state.usage.input_tokens,
            "output_tokens": state.usage.output_tokens,
        },
        "next_seq": state.next_seq.get(),
        "pending": pending(state.pending_call.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use salvor_core::SuspensionKind;
    use serde_json::json;

    use super::status;
    use salvor_core::RunStatus;

    /// A suspension says what it waits on only when there is something to
    /// say. A signal wait carries `"kind": "signal"` so a client can keep it
    /// out of an approval inbox; a human gate carries no `kind` key at all,
    /// which is the shape every client already reads and the shape every log
    /// written before the discriminator existed derives to.
    #[test]
    fn a_suspended_status_names_a_signal_and_stays_silent_about_a_gate() {
        let schema = json!({"type": "object", "required": ["approved"]});

        let signal = status(&RunStatus::Suspended {
            reason: "awaiting the payment webhook".to_owned(),
            input_schema: schema.clone(),
            kind: Some(SuspensionKind::Signal),
        });
        assert_eq!(
            signal,
            json!({
                "state": "suspended",
                "reason": "awaiting the payment webhook",
                "input_schema": schema,
                "kind": "signal",
            })
        );

        let gate = status(&RunStatus::Suspended {
            reason: "awaiting operator approval".to_owned(),
            input_schema: schema.clone(),
            kind: None,
        });
        assert_eq!(
            gate,
            json!({
                "state": "suspended",
                "reason": "awaiting operator approval",
                "input_schema": schema,
            })
        );
        assert!(
            gate.get("kind").is_none(),
            "a gate carries no discriminator, not even a null one: {gate}"
        );
    }
}
