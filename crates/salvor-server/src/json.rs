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
//!   human. Once the server's clock is past `wake_at`, the object also
//!   carries `"overdue": true` and `"overdue_seconds": n` (whole seconds
//!   since `wake_at`), naming a nap nobody has re-driven yet rather than
//!   leaving a caller to work it out against `wake_at` itself; before the
//!   deadline neither key appears.
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
///
/// `now` is the server's clock, read once by the caller and passed in rather
/// than read here, so this stays a pure fold: a `sleeping` status compares it
/// against the recorded `wake_at` to decide whether `overdue` and
/// `overdue_seconds` belong in the object. Every other arm ignores it.
#[must_use]
pub fn status(status: &RunStatus, now: OffsetDateTime) -> Value {
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
        RunStatus::Sleeping { wake_at } => {
            let mut obj = json!({
                "state": "sleeping",
                "wake_at": rfc3339(*wake_at),
            });
            // Omitted rather than sent as false/zero when the deadline is
            // still ahead: the same absent-is-absent rule `kind` follows on
            // `suspended` above, so a client that has never heard of these
            // keys reads a not-yet-due nap exactly as it always has.
            if now > *wake_at {
                let map = obj.as_object_mut().expect("status object");
                map.insert("overdue".to_owned(), json!(true));
                map.insert(
                    "overdue_seconds".to_owned(),
                    json!((now - *wake_at).whole_seconds()),
                );
            }
            obj
        }
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
/// intent). Nothing here executes; it is a pure fold of the recorded log,
/// except for the clock `now` carries in for [`status`]'s overdue check.
#[must_use]
pub fn run_state(state: &RunState, now: OffsetDateTime) -> Value {
    json!({
        "status": status(&state.status, now),
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
    use time::macros::datetime;

    use super::status;
    use salvor_core::RunStatus;

    /// A fixed instant for tests that do not care what "now" is, only that
    /// `status` needs one to fold a `sleeping` status; every other arm ignores
    /// it entirely.
    const ANY_NOW: time::OffsetDateTime = datetime!(2026-01-01 00:00:00 UTC);

    /// A suspension says what it waits on only when there is something to
    /// say. A signal wait carries `"kind": "signal"` so a client can keep it
    /// out of an approval inbox; a human gate carries no `kind` key at all,
    /// which is the shape every client already reads and the shape every log
    /// written before the discriminator existed derives to.
    #[test]
    fn a_suspended_status_names_a_signal_and_stays_silent_about_a_gate() {
        let schema = json!({"type": "object", "required": ["approved"]});

        let signal = status(
            &RunStatus::Suspended {
                reason: "awaiting the payment webhook".to_owned(),
                input_schema: schema.clone(),
                kind: Some(SuspensionKind::Signal),
            },
            ANY_NOW,
        );
        assert_eq!(
            signal,
            json!({
                "state": "suspended",
                "reason": "awaiting the payment webhook",
                "input_schema": schema,
                "kind": "signal",
            })
        );

        let gate = status(
            &RunStatus::Suspended {
                reason: "awaiting operator approval".to_owned(),
                input_schema: schema.clone(),
                kind: None,
            },
            ANY_NOW,
        );
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

    /// Before its deadline, a sleeping run's status carries only `wake_at`:
    /// no `overdue` key, not even `false`, the same absent-is-absent rule the
    /// suspended `kind` follows above.
    #[test]
    fn a_sleeping_run_not_yet_due_carries_no_overdue_keys() {
        let wake_at = datetime!(2026-01-01 12:00:00 UTC);
        let now = wake_at - time::Duration::minutes(5);

        let value = status(&RunStatus::Sleeping { wake_at }, now);
        assert_eq!(
            value,
            json!({
                "state": "sleeping",
                "wake_at": "2026-01-01T12:00:00Z",
            })
        );
        assert!(
            value.get("overdue").is_none(),
            "not due yet, so no overdue key at all: {value}"
        );
        assert!(value.get("overdue_seconds").is_none());
    }

    /// Once the server's clock is past `wake_at`, the status names it: a caller
    /// reads `overdue` and how long, rather than computing it against
    /// `wake_at` itself.
    #[test]
    fn a_sleeping_run_past_its_deadline_reports_overdue() {
        let wake_at = datetime!(2026-01-01 12:00:00 UTC);
        let now = wake_at + time::Duration::seconds(90);

        let value = status(&RunStatus::Sleeping { wake_at }, now);
        assert_eq!(
            value,
            json!({
                "state": "sleeping",
                "wake_at": "2026-01-01T12:00:00Z",
                "overdue": true,
                "overdue_seconds": 90,
            })
        );
    }

    /// The instant of the deadline itself is not yet "passed": a caller whose
    /// clock reads exactly `wake_at` sees the same not-due shape a moment
    /// earlier did.
    #[test]
    fn the_deadline_instant_itself_is_not_overdue() {
        let wake_at = datetime!(2026-01-01 12:00:00 UTC);

        let value = status(&RunStatus::Sleeping { wake_at }, wake_at);
        assert!(value.get("overdue").is_none());
    }
}
