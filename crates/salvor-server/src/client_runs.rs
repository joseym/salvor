//! The client-driven run surface: open or resume a run, read its log, and the
//! generic guarded append for control and deterministic-context events.
//!
//! # Who owns the loop
//!
//! The server-driven endpoints in [`crate::runs`] own the loop: the server
//! drives a run in a background task and the client submits data and reads
//! events. This surface inverts that. The client (a browser folding the run's
//! log in a wasm `ReplayCursor`, or an SDK) owns the loop and streams the
//! events it produces; the server owns the durable log and, on every append,
//! re-folds the log with the pure `salvor-replay` append-guard to confirm the
//! incoming event is the one legal next event. The trust boundary is narrow and
//! honest: the guard proves the run history is well formed (shape, correlation,
//! ordering, terminal rules), which is all a log validator can prove.
//!
//! # Scope
//!
//! This surface carries only the control and deterministic-context events the
//! client's cursor emits itself and that hold no secret and no side effect:
//! `RunStarted`, `NowObserved`, `RandomObserved`, `Suspended`, `Resumed`,
//! `BudgetExceeded`, `RunCompleted`, `RunFailed`. The side-effecting steps (the
//! model call and the tool call, which the server must perform because it holds
//! the key or the binary) are not supported here, so a model or tool event is
//! refused with a clear error.
//!
//! # The single-writer lease
//!
//! Opening a run mints a per-run `drive_token`, required on every append. It is
//! the per-run gate that layers on top of the process-wide bearer: one
//! authenticated caller still cannot drive another caller's run, and a second
//! live driver without the current lease is refused. Re-opening a run mints a
//! fresh lease, so a resuming tab always holds the current one.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use salvor_core::{EventEnvelope, LogValidator, RunId, SequenceNumber};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// The header carrying the per-run drive token on a guarded append.
const DRIVE_TOKEN_HEADER: &str = "x-drive-token";

/// The largest event-append body this surface accepts, before parsing.
const MAX_EVENTS_BODY: usize = 8 * 1024 * 1024;

/// The most envelopes one append batch may carry, so a single request cannot
/// grow a log without bound.
const MAX_EVENTS_PER_BATCH: usize = 1024;

/// The body of `POST /v1/client-runs`.
#[derive(Debug, Deserialize)]
struct OpenRequest {
    /// The agent this run drives under (`agent_def_hash`). Informational:
    /// the client records it inside the `RunStarted` event it appends.
    #[serde(default)]
    agent: Option<String>,
    /// The run input. Informational, for the same reason as `agent`.
    #[serde(default)]
    input: Value,
    /// An optional caller-chosen run id (a UUID). Minted when omitted.
    #[serde(default)]
    run_id: Option<String>,
    /// Whether to record model request bodies on the intent (per-run, off by
    /// default). Governs the server-performed model step, not yet implemented
    /// on this surface.
    #[serde(default)]
    record_prompts: bool,
}

/// The body of `POST /v1/client-runs/{id}/events`.
#[derive(Debug, Deserialize)]
struct AppendRequest {
    /// The envelopes to append, in order. Each is the pinned event-envelope
    /// wire JSON already used by the event stream and `salvor history --json`.
    events: Vec<EventEnvelope>,
}

/// The `?from_seq=` query on the log read.
#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    /// Return only envelopes at or after this sequence number.
    #[serde(default)]
    from_seq: Option<u64>,
}

/// `POST /v1/client-runs`: open a fresh client-driven run, or re-open (resume)
/// one this process already holds.
///
/// A fresh run comes back with an empty log and a new drive token; the client
/// appends its own `RunStarted` as the first event through the append endpoint.
/// Re-opening a known client run returns its full recorded log and a fresh
/// lease, for a refreshed tab to rebuild its cursor. A chosen id that already
/// has history but is not a client-driven run this process opened is refused,
/// so the client-driven and server-driven modes cannot collide.
pub async fn open(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let request: OpenRequest = parse_body(&body)?;
    // `agent` and `input` are accepted but not enforced against the appended
    // RunStarted; they matter once the server performs model calls.
    let _ = (&request.agent, &request.input);

    let run_id = match &request.run_id {
        Some(text) => parse_run_id(text)?,
        None => RunId::new(),
    };

    // A re-open of a run this process opened: return its log and a fresh lease.
    if state.is_client_run(run_id) {
        let log = state.store().read_log(run_id).await.map_err(store_error)?;
        let drive_token = state.lease_client_run(run_id, request.record_prompts);
        return Ok((StatusCode::OK, Json(open_body(run_id, &drive_token, &log))));
    }

    // A run id with existing history that this process did not open as a
    // client-driven run is foreign (a server-driven run, or one from before a
    // restart): refuse it rather than adopt it.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if !log.is_empty() {
        return Err(ApiError::RunExists(format!(
            "run {} already has recorded history and is not a client-driven run on this server; \
             it cannot be opened for client-driven runs",
            run_id.as_uuid()
        )));
    }

    let drive_token = state.lease_client_run(run_id, request.record_prompts);
    Ok((
        StatusCode::CREATED,
        Json(open_body(run_id, &drive_token, &[])),
    ))
}

/// `GET /v1/client-runs/{id}/log`: the recorded envelopes, for cursor rebuild.
///
/// `?from_seq=<n>` returns only envelopes at or after `n`, so a resuming client
/// that already holds a prefix fetches just the tail. The read needs no drive
/// token (a second viewer may read), but it serves only client-driven runs this
/// process opened, keeping the two modes' surfaces apart.
pub async fn get_log(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    if !state.is_client_run(run_id) {
        return Err(unknown_client_run(run_id));
    }
    let mut log = state.store().read_log(run_id).await.map_err(store_error)?;
    if let Some(from) = query.from_seq {
        log.retain(|env| env.seq.get() >= from);
    }
    Ok(Json(json!({ "log": log })))
}

/// `POST /v1/client-runs/{id}/events`: the generic guarded append.
///
/// Each envelope is re-folded through the `salvor-replay` append-guard against
/// the run's current log. A byte-identical re-append at an existing position is
/// a `200` no-op (a safe retry after a network blip); different bytes there, or
/// an illegal next event, is a `409`. Model and tool events are refused: they
/// belong to the server-performed model-step and tool-step endpoints, not to
/// this generic append. The whole batch is validated
/// before anything is written, so a batch that turns illegal appends nothing.
pub async fn append(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;

    // The per-run lease gate.
    let lease = state
        .client_run(run_id)
        .ok_or_else(|| unknown_client_run(run_id))?;
    let presented = headers
        .get(DRIVE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    match presented {
        None => {
            return Err(ApiError::MissingDriveToken(format!(
                "run {} requires a drive token in the `{DRIVE_TOKEN_HEADER}` header",
                run_id.as_uuid()
            )));
        }
        Some(token) if token != lease.drive_token => {
            return Err(ApiError::InvalidDriveToken(format!(
                "the presented drive token is not the current lease for run {}",
                run_id.as_uuid()
            )));
        }
        Some(_) => {}
    }

    // Body-size discipline, as a fast precheck before parsing.
    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "append body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: AppendRequest = parse_body(&body)?;
    if request.events.len() > MAX_EVENTS_PER_BATCH {
        return Err(ApiError::PayloadTooLarge(format!(
            "append batch carries {} events, over the {MAX_EVENTS_PER_BATCH} cap",
            request.events.len()
        )));
    }

    let stored = state.store().read_log(run_id).await.map_err(store_error)?;
    let mut validator = LogValidator::new(stored);
    let mut appended: Vec<u64> = Vec::with_capacity(request.events.len());
    let mut to_append: Vec<EventEnvelope> = Vec::new();

    for candidate in request.events {
        if candidate.run_id != run_id {
            return Err(ApiError::Divergence(format!(
                "event names run {} but the path is run {}",
                candidate.run_id.as_uuid(),
                run_id.as_uuid()
            )));
        }
        reject_side_effecting_kind(&candidate)?;

        let next_seq = validator.next_seq();
        if candidate.seq < next_seq {
            // An already-recorded position: idempotent retry or divergence.
            let index = candidate.seq.get() as usize;
            let recorded = &validator.log()[index];
            if *recorded == candidate {
                appended.push(candidate.seq.get());
                continue;
            }
            return Err(ApiError::Divergence(format!(
                "different bytes submitted at the already-recorded seq {}",
                candidate.seq.get()
            )));
        }

        // A new position: the append-guard decides legality.
        validator
            .push(candidate.clone())
            .map_err(|error| ApiError::Divergence(error.to_string()))?;
        appended.push(candidate.seq.get());
        to_append.push(candidate);
    }

    // The batch validated end to end; commit the genuinely new events.
    for envelope in &to_append {
        state.store().append(envelope).await.map_err(append_error)?;
    }

    Ok((StatusCode::OK, Json(json!({ "appended": appended }))))
}

/// The `201`/`200` open response body.
fn open_body(run_id: RunId, drive_token: &str, log: &[EventEnvelope]) -> Value {
    json!({
        "run": run_id.as_uuid().to_string(),
        "drive_token": drive_token,
        "log": log,
    })
}

/// Refuses a model or tool event on the generic append: those are recorded
/// through the server-performed model-step and tool-step endpoints,
/// never hand-appended here.
fn reject_side_effecting_kind(candidate: &EventEnvelope) -> Result<(), ApiError> {
    use salvor_core::Event;
    let kind = match &candidate.event {
        Event::ModelCallRequested { .. } => "ModelCallRequested",
        Event::ModelCallCompleted { .. } => "ModelCallCompleted",
        Event::ToolCallRequested { .. } => "ToolCallRequested",
        Event::ToolCallCompleted { .. } => "ToolCallCompleted",
        _ => return Ok(()),
    };
    Err(ApiError::UnsupportedEventKind(format!(
        "the generic append accepts control and context events only; `{kind}` is recorded through \
         the model-step or tool-step endpoint"
    )))
}

/// Parses a JSON body into `T`, mapping a decode failure to a `400`.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::BadRequest(format!("request body is not valid JSON: {error}")))
}

/// Parses a run id from its UUID string, mapping a bad id to a `400`.
fn parse_run_id(text: &str) -> Result<RunId, ApiError> {
    Uuid::parse_str(text).map(RunId::from_uuid).map_err(|_| {
        ApiError::BadRequest(format!("`{text}` is not a valid run id (expected a UUID)"))
    })
}

/// The not-found error for a run that is not a client-driven run here.
fn unknown_client_run(run_id: RunId) -> ApiError {
    ApiError::UnknownRun(format!(
        "no client-driven run {} on this server; open it first",
        run_id.as_uuid()
    ))
}

/// Maps a store read error to a `500`.
fn store_error(error: salvor_store::StoreError) -> ApiError {
    ApiError::Internal(format!("store: {error}"))
}

/// Maps a store append error: a position taken out from under a validated batch
/// (a lost lease race) is a `409` divergence, anything else a `500`.
fn append_error(error: salvor_store::StoreError) -> ApiError {
    match error {
        salvor_store::StoreError::Conflict { seq, .. } => ApiError::Divergence(format!(
            "seq {} was taken by another writer before the append landed",
            SequenceNumber::get(seq)
        )),
        other => ApiError::Internal(format!("store: {other}")),
    }
}
