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
//! - `{ "state": "suspended", "reason": "...", "input_schema": { ... } }`
//! - `{ "state": "budget_exceeded", "budget": { "kind": "...", "limit": n },
//!    "observed": n }`
//! - `{ "state": "completed", "output": <json> }`
//! - `{ "state": "failed", "error": "..." }`
//! - `{ "state": "abandoned" }`, optionally with `"reason": "..."` and, when a
//!   needs-reconciliation run was abandoned,
//!   `"unresolved_write": { "seq": n, "tool": "..." }` — the recorded evidence
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
        } => json!({
            "state": "suspended",
            "reason": reason,
            "input_schema": input_schema,
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
            // `unresolved_write` — the same zero-vs-absent honesty the rest of
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
