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
//! to carry. The reconciliation refusal is the original case (the recorded write
//! intent travels in `details.intent`, mirroring the CLI's report); the fork
//! endpoint's `write_replay_hazard` carries the same kind of evidence in
//! `details.writes` (the exact writes a fork would re-fire), which is the
//! refuse-then-record differentiator on the wire.
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
    /// A client-driven append arrived with no drive token. HTTP 401. The drive
    /// token is the per-run single-writer lease; every append must present it.
    MissingDriveToken(String),
    /// A client-driven append presented a drive token that is not the run's
    /// current lease. HTTP 403. Only the run's current writer may drive it.
    InvalidDriveToken(String),
    /// A client-driven append carried an event kind this endpoint does not
    /// accept (a model or tool event, which the model-step and tool-step
    /// endpoints own). HTTP 422.
    UnsupportedEventKind(String),
    /// A client-driven append is not the legal next event for the run's log:
    /// the re-folding append-guard rejected it, or byte-different bytes arrived
    /// at an already-recorded position. HTTP 409.
    Divergence(String),
    /// A request body exceeded the size or count cap. HTTP 413.
    PayloadTooLarge(String),
    /// A server-performed model step was requested but no model executor is
    /// wired on this server (the host injected none). HTTP 503. Recording no
    /// completion, so the run stays drivable once an executor is present.
    ModelExecutorUnavailable(String),
    /// The provider call for a model step failed. HTTP 502. No completion is
    /// recorded, so the write-ahead intent is left dangling (the legal crash
    /// story) and the run stays drivable: a retry re-issues the call safely.
    ModelExecution(String),
    /// A tool-step named a tool the server's registry does not hold. HTTP 404.
    /// Nothing is written for a tool the server cannot dispatch, so the step is
    /// retriable once the tool is registered.
    UnknownTool(String),
    /// A server-performed tool step was requested but no tool registry is wired
    /// on this server (the host injected none). HTTP 503. The mirror of
    /// [`ModelExecutorUnavailable`](Self::ModelExecutorUnavailable): no intent
    /// is written, so the run stays drivable once a registry is present.
    ToolRegistryUnavailable(String),
    /// The dispatch of a tool-step's tool failed. HTTP 502. No completion is
    /// recorded, so the write-ahead intent is left dangling (the legal crash
    /// story) and the run stays drivable-or-reconcilable per the tool's effect.
    ToolExecution(String),
    /// A client tried to record its own completion for a client-performed tool
    /// call this server will not take its word for: the pending intent was
    /// performed by the server, or the declaration says
    /// `trust_completion = false`, or the declaration carries no `output_schema`
    /// to check the report against. HTTP 403.
    ///
    /// Nothing is recorded, so the log still ends at the recorded intent. For a
    /// `Write` that is already `needs_reconciliation` to the pure fold in
    /// `salvor-replay`, and `POST /v1/client-runs/{id}/resolve` already exists
    /// to settle it by hand: the refusal reuses machinery rather than inventing
    /// a state. The message names that endpoint, because it is what the caller
    /// does next.
    ClientCompletionRefused(String),
    /// A submitted graph document failed strict validation. HTTP 400. Carries
    /// the complete, node/edge-precise error list as evidence, because a graph
    /// is a control document validated all at once (collect-all, no
    /// short-circuit), so an author sees every mistake in one response.
    InvalidGraph {
        /// The human sentence.
        message: String,
        /// The full list of structured validation errors, each naming the node
        /// or edge at fault.
        errors: Value,
    },
    /// No graph is stored under the given hash. HTTP 404.
    UnknownGraph(String),
    /// A graph-only endpoint (the per-run graph projection) was asked for a run
    /// whose log is not a graph run (an ordinary agent run has no
    /// `GraphRunStarted` head). HTTP 409.
    NotAGraphRun(String),
    /// A fork was requested from a node the origin never entered (it is not in
    /// the graph, or the walk routed past it). A fork point must be a node
    /// boundary the run reached. HTTP 409.
    InvalidForkNode(String),
    /// A fork was requested of an origin parked at a dangling write (status
    /// `NeedsReconciliation`): the origin must be resolved first, since forking
    /// past an unsettled write would carry that ambiguity into the child. HTTP
    /// 409. Carries the origin's recorded write intent as evidence, mirroring
    /// [`NeedsReconciliation`](Self::NeedsReconciliation).
    OriginNeedsReconciliation {
        /// The human sentence.
        message: String,
        /// The origin's recorded dangling write intent (the same shape a resume
        /// reconciliation refusal carries).
        intent: Value,
    },
    /// A fork would re-walk a segment containing recorded `Effect::Write` intents
    /// the operator has not acknowledged. HTTP 409. Carries the exact writes that
    /// would re-fire as evidence, mirroring
    /// [`NeedsReconciliation`](Self::NeedsReconciliation)'s use of `details`: the
    /// refuse-then-record differentiator in one response the operator can read
    /// and then acknowledge.
    WriteReplayHazard {
        /// The human sentence.
        message: String,
        /// The unacknowledged writes the fork's re-walked segment would
        /// re-execute, each `{ seq, tool, input, idempotency_key, recorded_at }`.
        writes: Value,
    },
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
            ApiError::InvalidGraph { .. } => (StatusCode::BAD_REQUEST, "invalid_graph"),
            ApiError::UnknownGraph(_) => (StatusCode::NOT_FOUND, "unknown_graph"),
            ApiError::NotAGraphRun(_) => (StatusCode::CONFLICT, "not_a_graph_run"),
            ApiError::InvalidForkNode(_) => (StatusCode::CONFLICT, "invalid_fork_node"),
            ApiError::OriginNeedsReconciliation { .. } => {
                (StatusCode::CONFLICT, "origin_needs_reconciliation")
            }
            ApiError::WriteReplayHazard { .. } => (StatusCode::CONFLICT, "write_replay_hazard"),
            ApiError::NeedsReconciliation { .. } => (StatusCode::CONFLICT, "needs_reconciliation"),
            ApiError::MissingDriveToken(_) => (StatusCode::UNAUTHORIZED, "missing_drive_token"),
            ApiError::InvalidDriveToken(_) => (StatusCode::FORBIDDEN, "invalid_drive_token"),
            ApiError::UnsupportedEventKind(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "unsupported_event_kind")
            }
            ApiError::Divergence(_) => (StatusCode::CONFLICT, "divergence"),
            ApiError::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            ApiError::ModelExecutorUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "model_executor_unavailable",
            ),
            ApiError::ModelExecution(_) => (StatusCode::BAD_GATEWAY, "model_execution"),
            ApiError::UnknownTool(_) => (StatusCode::NOT_FOUND, "unknown_tool"),
            ApiError::ToolRegistryUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "tool_registry_unavailable")
            }
            ApiError::ToolExecution(_) => (StatusCode::BAD_GATEWAY, "tool_execution"),
            ApiError::ClientCompletionRefused(_) => {
                (StatusCode::FORBIDDEN, "client_completion_refused")
            }
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
            | ApiError::MissingDriveToken(m)
            | ApiError::InvalidDriveToken(m)
            | ApiError::UnsupportedEventKind(m)
            | ApiError::Divergence(m)
            | ApiError::PayloadTooLarge(m)
            | ApiError::ModelExecutorUnavailable(m)
            | ApiError::ModelExecution(m)
            | ApiError::UnknownTool(m)
            | ApiError::ToolRegistryUnavailable(m)
            | ApiError::ToolExecution(m)
            | ApiError::ClientCompletionRefused(m)
            | ApiError::UnknownGraph(m)
            | ApiError::NotAGraphRun(m)
            | ApiError::InvalidForkNode(m)
            | ApiError::InvalidGraph { message: m, .. }
            | ApiError::OriginNeedsReconciliation { message: m, .. }
            | ApiError::WriteReplayHazard { message: m, .. }
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
        match self {
            ApiError::NeedsReconciliation { intent, .. }
            | ApiError::OriginNeedsReconciliation { intent, .. } => {
                error["details"] = json!({ "intent": intent });
            }
            ApiError::WriteReplayHazard { writes, .. } => {
                error["details"] = json!({ "writes": writes });
            }
            ApiError::InvalidGraph { errors, .. } => {
                error["details"] = json!({ "errors": errors });
            }
            _ => {}
        }
        (status, Json(json!({ "error": error }))).into_response()
    }
}
