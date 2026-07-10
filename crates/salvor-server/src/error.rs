//! [`ApiError`]: the one error type every handler returns, and the JSON
//! envelope it serializes to.
//!
//! Every failure the control plane reports has the same shape on the wire, so
//! a thin SDK can decode one thing:
//!
//! ```json
//! { "error": { "code": "unknown_run", "message": "...", "details": { ... } } }
//! ```
//!
//! `code` is a stable machine token (an SDK matches on it); `message` is a
//! human sentence; `details` is present only when there is structured evidence
//! to carry, and the reconciliation refusal is the case that uses it (the
//! recorded write intent travels there, mirroring the CLI's report).
//!
//! Each variant fixes its own HTTP status, so the status and the body's `code`
//! never drift: a 404 always carries `unknown_run` or `unknown_agent`, a 409
//! always carries a conflict or a reconciliation refusal, and so on.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// A control-plane error, with the HTTP status and machine code baked in.
#[derive(Debug)]
pub enum ApiError {
    /// A request body was malformed, or a resume input failed validation
    /// against the recorded schema. HTTP 400.
    BadRequest(String),
    /// The bearer token was missing or wrong. HTTP 401.
    Unauthorized,
    /// No run exists under the given id. HTTP 404.
    UnknownRun(String),
    /// No agent is registered under the given id. HTTP 404.
    UnknownAgent(String),
    /// A run already exists at the requested id. HTTP 409.
    RunExists(String),
    /// A verb was applied to a run in the wrong state (resuming a finished
    /// run, resolving a run that has no dangling write). HTTP 409.
    WrongState(String),
    /// A run needs human reconciliation and cannot be driven automatically.
    /// Carries the recorded write intent as evidence. HTTP 409.
    NeedsReconciliation {
        /// The human sentence.
        message: String,
        /// The recorded intent (tool, input, effect, idempotency key, seq,
        /// recorded time), so the caller sees exactly what to reconcile.
        intent: Value,
    },
    /// An unexpected internal failure (a store read, an agent build). HTTP
    /// 500. The message is safe to surface: it names the layer, not a secret.
    Internal(String),
}

impl ApiError {
    /// The HTTP status and stable machine `code` for this error.
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::UnknownRun(_) => (StatusCode::NOT_FOUND, "unknown_run"),
            ApiError::UnknownAgent(_) => (StatusCode::NOT_FOUND, "unknown_agent"),
            ApiError::RunExists(_) => (StatusCode::CONFLICT, "run_exists"),
            ApiError::WrongState(_) => (StatusCode::CONFLICT, "wrong_state"),
            ApiError::NeedsReconciliation { .. } => (StatusCode::CONFLICT, "needs_reconciliation"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }

    /// The human sentence for this error.
    fn message(&self) -> String {
        match self {
            ApiError::BadRequest(m)
            | ApiError::UnknownRun(m)
            | ApiError::UnknownAgent(m)
            | ApiError::RunExists(m)
            | ApiError::WrongState(m)
            | ApiError::Internal(m)
            | ApiError::NeedsReconciliation { message: m, .. } => m.clone(),
            ApiError::Unauthorized => "missing or invalid bearer token".to_owned(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = self.message();
        let mut error = json!({ "code": code, "message": message });
        if let ApiError::NeedsReconciliation { intent, .. } = self {
            error["details"] = json!({ "intent": intent });
        }
        (status, Json(json!({ "error": error }))).into_response()
    }
}
