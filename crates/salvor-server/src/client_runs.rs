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
//!
//! # Labels on a client-driven run
//!
//! The client, not this server, synthesizes the run's `RunStarted` (see [`open`]):
//! there is no server-side "creation" step here the way [`crate::runs::start`]'s
//! `StartRequest` has one. So the correlation `labels` a caller wants land in the
//! `RunStarted` payload the client builds and appends, and the one place this
//! server ever inspects them is [`append`], the moment that event is accepted:
//! the sanity bounds (see `salvor_runtime::validate_labels`) are checked there,
//! against whatever `labels` the submitted event carries, before it is written.

use std::convert::Infallible;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use salvor_core::{Effect, Event, EventEnvelope, LogValidator, RunId, SequenceNumber, TokenUsage};
use salvor_llm::{ContentDelta, MessageAccumulator, StreamEvent};
use salvor_runtime::{RuntimeError, hash_value, response_value, usage_of, validate_labels};
use salvor_tools::{ToolCtx, ToolOutcome};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::error::ApiError;
use crate::executor::{ModelExecutor, ModelStream};
use crate::state::{AppState, ClientRunLease};
use std::sync::Arc;

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

/// The body of `POST /v1/client-runs/{id}/model-step`.
#[derive(Debug, Deserialize)]
struct ModelStepRequest {
    /// The log position the client's cursor reserved for the model intent.
    seq: u64,
    /// The client's canonical model request value (a `MessageRequest` as JSON).
    /// The server hashes and forwards exactly these bytes.
    request: Value,
}

/// The `?stream=` query on the model step.
#[derive(Debug, Default, Deserialize)]
pub struct ModelStepQuery {
    /// When `1` or `true`, stream provider events for a live ticker. The
    /// `Accept: text/event-stream` header selects streaming too.
    #[serde(default)]
    stream: Option<String>,
}

/// The body of `POST /v1/client-runs/{id}/tool-step`.
#[derive(Debug, Deserialize)]
struct ToolStepRequest {
    /// The log position the client's cursor reserved for the tool intent.
    seq: u64,
    /// The registered tool's name. Unknown to the registry is an error, and no
    /// intent is written.
    tool: String,
    /// The typed input passed to the tool, recorded on the intent verbatim.
    input: Value,
    /// The idempotency key for this attempt, when the tool has one. The client
    /// draws it from a recorded `RandomObserved` so it reproduces on replay.
    #[serde(default)]
    idempotency_key: Option<String>,
    /// A client-declared effect, accepted for shape parity but deliberately
    /// ignored: the recorded effect is the registry's operator-declared
    /// one, so a caller cannot up- or down-grade it.
    #[serde(default)]
    #[allow(dead_code)]
    effect: Option<Effect>,
}

/// The body of `POST /v1/client-runs/{id}/resolve`.
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    /// The output to record for the dangling write, verbatim, exactly as the
    /// server-driven resolve takes it.
    output: Value,
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
    authorize_drive(&state, run_id, &headers)?;

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
        // The client synthesizes its own `RunStarted` (see the module docs);
        // this append is the one place the server ever sees it, so it is
        // where the sanity bounds on any carried `labels` are enforced. A
        // byte-identical retry at an already-recorded position (handled just
        // below) was validated the first time it landed, so re-checking here
        // is cheap and harmless, never a behavior change.
        if let Event::RunStarted {
            labels: Some(labels),
            ..
        } = &candidate.event
        {
            validate_labels(labels).map_err(ApiError::BadRequest)?;
        }

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

/// `POST /v1/client-runs/{id}/model-step`: the server-performed model call.
///
/// The client's cursor reserved `seq` as the model intent's position and hands
/// the server the request to perform. The server recomputes `request_hash` from
/// the body with the same canonical hash the runtime uses (so the client cannot
/// lie about the hash), appends `ModelCallRequested` write-ahead, performs the
/// call through the injected [`ModelExecutor`], appends `ModelCallCompleted`,
/// and returns the completion. It mirrors `RunCtx::model_call` server-side.
///
/// Retry identity is `(seq, request_hash)`, mirroring `ReplayCursor::model_call`:
///
/// - A completed step already recorded at `seq` with the same hash returns the
///   recorded completion; the provider is not called and the log does not grow.
/// - A dangling intent at `seq` with the same hash (the tab died mid-call) is
///   re-executed: an unanswered model request has no external effect to double,
///   so the fresh completion correlates to the recorded intent.
/// - A different hash at `seq`, or a non-model event there, is `409 divergence`.
///
/// With `Accept: text/event-stream` (or `?stream=1`) the provider's events
/// stream as server-sent frames for a live ticker, and the assembled completion
/// is recorded once at the end (byte-identical to the non-streaming path), so a
/// tab that drops mid-stream leaves a dangling intent, re-issued safely.
pub async fn model_step(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    Query(query): Query<ModelStepQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let lease = authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "model-step body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let ModelStepRequest { seq, request } = parse_body(&body)?;

    // Recompute the hash from the submitted body with the runtime's own
    // canonical hash: the hash the server records is the hash it will send.
    let request_hash = hash_value(&request);
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let plan = plan_model_step(&log, seq, &request_hash)?;
    let streaming = wants_stream(&headers, &query);

    match plan {
        ModelStepPlan::Replay { response, usage } => {
            // Already recorded: answer from the log, call nothing, grow nothing.
            if streaming {
                Ok(single_complete_stream(&response, usage))
            } else {
                Ok(completion_body(&response, usage).into_response())
            }
        }
        ModelStepPlan::Perform { append_intent } => {
            let executor = state.model_executor().ok_or_else(|| {
                ApiError::ModelExecutorUnavailable(
                    "this server has no model executor wired, so it cannot perform a model step"
                        .to_owned(),
                )
            })?;

            // Write-ahead: record the intent before the provider is contacted,
            // so a crash mid-call leaves a dangling intent (re-issued on retry).
            // A dangling-intent retry skips this: the intent is already recorded.
            if append_intent {
                let request_body = lease.record_prompts.then(|| request.clone());
                let intent = EventEnvelope::new(
                    run_id,
                    SequenceNumber::new(seq),
                    state.now(),
                    Event::ModelCallRequested {
                        seq: SequenceNumber::new(seq),
                        request_hash: request_hash.clone(),
                        request_body,
                    },
                );
                let mut validator = LogValidator::new(log);
                validator
                    .push(intent.clone())
                    .map_err(|error| ApiError::Divergence(error.to_string()))?;
                state.store().append(&intent).await.map_err(append_error)?;
            }

            if streaming {
                perform_streaming(state, run_id, seq, request, executor).await
            } else {
                perform_unary(&state, run_id, seq, request, executor.as_ref()).await
            }
        }
    }
}

/// What a model step must do, decided from the recorded log alone.
enum ModelStepPlan {
    /// The step is already recorded: return this completion, execute nothing.
    Replay {
        /// The recorded response value.
        response: Value,
        /// The recorded token usage.
        usage: TokenUsage,
    },
    /// The step must be performed. `append_intent` is true for a fresh call and
    /// false for a dangling-intent re-issue (the intent is already recorded).
    Perform {
        /// Whether to write the intent before executing.
        append_intent: bool,
    },
}

/// Decides the model step from the log and the recomputed hash, mirroring
/// `ReplayCursor::model_call`'s replay/re-issue/divergence branches.
fn plan_model_step(
    log: &[EventEnvelope],
    seq: u64,
    request_hash: &str,
) -> Result<ModelStepPlan, ApiError> {
    let next = log.len() as u64;
    if seq == next {
        // A fresh intent at the next contiguous position.
        return Ok(ModelStepPlan::Perform {
            append_intent: true,
        });
    }
    if seq > next {
        return Err(ApiError::Divergence(format!(
            "model-step seq {seq} is beyond the log end {next}"
        )));
    }

    // The position is already recorded: it must be the model intent, its hash
    // must match, and its completion (if any) decides replay versus re-issue.
    let recorded = &log[seq as usize];
    let Event::ModelCallRequested {
        request_hash: recorded_hash,
        ..
    } = &recorded.event
    else {
        return Err(ApiError::Divergence(format!(
            "seq {seq} already holds a non-model event; it is not a model-step position"
        )));
    };
    if recorded_hash != request_hash {
        return Err(ApiError::Divergence(format!(
            "model-step at seq {seq} carries a request hash that differs from the recorded intent"
        )));
    }
    match log.get(seq as usize + 1) {
        Some(next_env) => match &next_env.event {
            Event::ModelCallCompleted {
                seq: corr,
                response,
                usage,
            } if corr.get() == seq => Ok(ModelStepPlan::Replay {
                response: response.clone(),
                usage: *usage,
            }),
            _ => Err(ApiError::Divergence(format!(
                "the event after the intent at seq {seq} is not its completion"
            ))),
        },
        // A dangling intent (the last event): re-issue the call.
        None => Ok(ModelStepPlan::Perform {
            append_intent: false,
        }),
    }
}

/// Performs a non-streaming model call: execute, record the completion, and
/// return `{ response, usage }`.
async fn perform_unary(
    state: &AppState,
    run_id: RunId,
    seq: u64,
    request: Value,
    executor: &dyn ModelExecutor,
) -> Result<Response, ApiError> {
    let response = executor
        .execute(request)
        .await
        .map_err(ApiError::ModelExecution)?;
    let usage = usage_of(&response);
    let response_value = response_value(&response);
    append_completion(state, run_id, seq, &response_value, usage).await?;
    Ok(completion_body(&response_value, usage).into_response())
}

/// Performs a streaming model call: open the provider stream, then hand a
/// server-sent-events body a background task drives (ticker frames, then the
/// recorded completion). Opening the stream synchronously means a failure to
/// open is a proper error envelope, not a half-open stream.
async fn perform_streaming(
    state: AppState,
    run_id: RunId,
    seq: u64,
    request: Value,
    executor: Arc<dyn ModelExecutor>,
) -> Result<Response, ApiError> {
    let stream = executor
        .open_stream(request)
        .await
        .map_err(ApiError::ModelExecution)?;
    let (tx, rx) = mpsc::channel::<Result<SseEvent, Infallible>>(64);
    tokio::spawn(drive_model_stream(state, run_id, seq, stream, tx));
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// Pumps the provider stream: forward each event as a ticker frame and fold it
/// into a [`MessageAccumulator`], then record the assembled completion once and
/// send the final `complete` frame. A mid-stream error, an accumulation
/// failure, or a completion-append failure sends an `error` frame and records
/// nothing, so the write-ahead intent is left dangling and the run stays
/// drivable.
async fn drive_model_stream(
    state: AppState,
    run_id: RunId,
    seq: u64,
    mut stream: Box<dyn ModelStream>,
    tx: mpsc::Sender<Result<SseEvent, Infallible>>,
) {
    let mut accumulator = MessageAccumulator::new();
    loop {
        match stream.next_event().await {
            Some(Ok(event)) => {
                if let Err(error) = accumulator.apply(&event) {
                    let _ = tx.send(Ok(error_frame(&error.to_string()))).await;
                    return;
                }
                if let Some(frame) = ticker_frame(&event)
                    && tx
                        .send(Ok(SseEvent::default()
                            .event("delta")
                            .data(frame.to_string())))
                        .await
                        .is_err()
                {
                    // The client hung up; stop, leaving the intent dangling.
                    return;
                }
            }
            Some(Err(message)) => {
                let _ = tx.send(Ok(error_frame(&message))).await;
                return;
            }
            None => break,
        }
    }

    let response = match accumulator.into_message() {
        Ok(response) => response,
        Err(error) => {
            let _ = tx.send(Ok(error_frame(&error.to_string()))).await;
            return;
        }
    };
    let usage = usage_of(&response);
    let response_value = response_value(&response);
    if append_completion(&state, run_id, seq, &response_value, usage)
        .await
        .is_err()
    {
        let _ = tx
            .send(Ok(error_frame("recording the model completion failed")))
            .await;
        return;
    }
    let complete = completion_json(&response_value, usage);
    let _ = tx
        .send(Ok(SseEvent::default()
            .event("complete")
            .data(complete.to_string())))
        .await;
}

/// Records the `ModelCallCompleted` at `seq + 1`, correlated to the intent at
/// `seq`, after validating it is the legal next event.
async fn append_completion(
    state: &AppState,
    run_id: RunId,
    seq: u64,
    response: &Value,
    usage: TokenUsage,
) -> Result<(), ApiError> {
    let completion = EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq + 1),
        state.now(),
        Event::ModelCallCompleted {
            seq: SequenceNumber::new(seq),
            response: response.clone(),
            usage,
        },
    );
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let mut validator = LogValidator::new(log);
    validator
        .push(completion.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;
    state
        .store()
        .append(&completion)
        .await
        .map_err(append_error)
}

/// Whether the request selects the streaming variant: `?stream=1`/`true`, or an
/// `Accept: text/event-stream` header.
fn wants_stream(headers: &HeaderMap, query: &ModelStepQuery) -> bool {
    if let Some(flag) = &query.stream
        && (flag == "1" || flag == "true")
    {
        return true;
    }
    headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"))
}

/// The ticker frame for a provider event, or `None` for events with nothing a
/// live ticker shows (start/stop/ping). Text and thinking deltas and the final
/// usage are what a token/cost ticker consumes.
fn ticker_frame(event: &StreamEvent) -> Option<Value> {
    match event {
        StreamEvent::ContentBlockDelta { index, delta } => match delta {
            ContentDelta::Text { text } => {
                Some(json!({ "type": "text_delta", "index": index, "text": text }))
            }
            ContentDelta::Thinking { thinking } => {
                Some(json!({ "type": "thinking_delta", "index": index, "thinking": thinking }))
            }
            _ => None,
        },
        StreamEvent::MessageDelta { usage, .. } => {
            Some(json!({ "type": "usage", "output_tokens": usage.output_tokens }))
        }
        _ => None,
    }
}

/// A one-frame server-sent-events body carrying an already-recorded completion,
/// for a streaming request that resolves to a replay (no live tokens).
fn single_complete_stream(response: &Value, usage: TokenUsage) -> Response {
    let frame = SseEvent::default()
        .event("complete")
        .data(completion_json(response, usage).to_string());
    Sse::new(tokio_stream::once(Ok::<_, Infallible>(frame)))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The `{ response, usage }` JSON both the non-streaming body and the `complete`
/// frame carry.
fn completion_json(response: &Value, usage: TokenUsage) -> Value {
    json!({ "response": response, "usage": usage })
}

/// The non-streaming `200` body.
fn completion_body(response: &Value, usage: TokenUsage) -> Json<Value> {
    Json(completion_json(response, usage))
}

/// An `error` server-sent-events frame carrying a human message.
fn error_frame(message: &str) -> SseEvent {
    SseEvent::default()
        .event("error")
        .data(json!({ "message": message }).to_string())
}

/// The `201`/`200` open response body.
fn open_body(run_id: RunId, drive_token: &str, log: &[EventEnvelope]) -> Value {
    json!({
        "run": run_id.as_uuid().to_string(),
        "drive_token": drive_token,
        "log": log,
    })
}

/// `POST /v1/client-runs/{id}/tool-step`: the server-performed tool call.
///
/// The client's cursor reserved `seq` as the tool intent's position. The server
/// looks the tool up in its injected [`ToolRegistry`](crate::ToolRegistry),
/// takes the operator-declared [`Effect`] from that registration (never from
/// the client, so a caller cannot up- or down-grade it), appends
/// `ToolCallRequested` write-ahead, dispatches the tool, appends
/// `ToolCallCompleted`, and returns the output. It mirrors `RunCtx::tool_call`
/// server-side, and its retry and reconciliation branches mirror
/// `ReplayCursor::tool_call`:
///
/// - A completed step recorded at `seq` with the same (tool, input, effect,
///   key) returns the recorded output; the tool is not dispatched and the log
///   does not grow.
/// - A dangling `Read`/`Idempotent` intent at `seq` (the tab died mid-call) is
///   re-executed under the RECORDED idempotency key, so an idempotent retry
///   reuses the exact key the provider collapses duplicates on.
/// - A dangling `Write` intent is `409 needs_reconciliation` carrying the
///   recorded intent as evidence, and nothing is dispatched: the write may have
///   landed, and only [`resolve`] may record its completion.
/// - A different (tool, input, effect, key) at `seq`, or a non-tool event
///   there, is `409 divergence`.
///
/// An unknown tool (or no registry at all) writes nothing, mirroring the model
/// step's no-executor rule: the step is retriable once the tool is registered.
pub async fn tool_step(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "tool-step body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: ToolStepRequest = parse_body(&body)?;

    // Look the tool up before anything is written. No registry is a 503; a
    // registry without the named tool is a 404. Either way, nothing is written.
    let registry = state.tool_registry().ok_or_else(|| {
        ApiError::ToolRegistryUnavailable(
            "this server has no tool registry wired, so it cannot perform a tool step".to_owned(),
        )
    })?;
    let tool = registry.get(&request.tool).ok_or_else(|| {
        ApiError::UnknownTool(format!(
            "no tool named `{}` is registered on this server",
            request.tool
        ))
    })?;

    // The effect is the registry's operator declaration, never the client's.
    // The client-declared `effect` field on the body is dropped here.
    let effect = tool.effect();
    let ToolStepRequest {
        seq,
        tool: tool_name,
        input,
        idempotency_key,
        effect: _,
    } = request;

    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let plan = plan_tool_step(
        &log,
        seq,
        &tool_name,
        &input,
        effect,
        idempotency_key.as_deref(),
    )?;

    match plan {
        ToolStepPlan::Replay { output } => Ok(tool_output_body(&output)),
        ToolStepPlan::Reconcile { intent } => Err(ApiError::NeedsReconciliation {
            message: format!(
                "run {} needs reconciliation: a write was recorded but never completed, so it \
                 may or may not have taken effect. Verify externally, then resolve it",
                run_id.as_uuid()
            ),
            intent,
        }),
        ToolStepPlan::Perform {
            append_intent,
            exec_key,
        } => {
            // Write-ahead: record the intent before the tool runs, so a crash
            // mid-call leaves a dangling intent (re-issued or reconciled on
            // retry, per effect). A dangling re-issue skips this: the intent is
            // already recorded.
            if append_intent {
                let intent = EventEnvelope::new(
                    run_id,
                    SequenceNumber::new(seq),
                    state.now(),
                    Event::ToolCallRequested {
                        seq: SequenceNumber::new(seq),
                        tool: tool_name.clone(),
                        input: input.clone(),
                        effect,
                        idempotency_key: exec_key.clone(),
                    },
                );
                let mut validator = LogValidator::new(log);
                validator
                    .push(intent.clone())
                    .map_err(|error| ApiError::Divergence(error.to_string()))?;
                state.store().append(&intent).await.map_err(append_error)?;
            }

            // Dispatch through the same erased contract the runtime uses, with
            // the idempotency key on the context so an idempotent retry reuses
            // it. A dispatch failure is an error envelope with no completion, so
            // the intent is left dangling (legal, the crash story).
            let ctx = ToolCtx::new(exec_key);
            let outcome = tool
                .call_json(&ctx, input)
                .await
                .map_err(|error| ApiError::ToolExecution(error.to_string()))?;
            let output = match outcome {
                ToolOutcome::Output(value) => value,
                ToolOutcome::Suspend(_) => {
                    return Err(ApiError::ToolExecution(format!(
                        "tool `{tool_name}` suspended, which a server-performed tool step does \
                         not support; no completion recorded"
                    )));
                }
            };
            append_tool_completion(&state, run_id, seq, &output).await?;
            Ok(tool_output_body(&output))
        }
    }
}

/// What a tool step must do, decided from the recorded log and the registry's
/// effect alone.
enum ToolStepPlan {
    /// The step is already recorded: return this output, dispatch nothing.
    Replay {
        /// The recorded tool output.
        output: Value,
    },
    /// A dangling write: surface reconciliation with this intent evidence,
    /// dispatch nothing.
    Reconcile {
        /// The recorded write intent, for the error body.
        intent: Value,
    },
    /// The step must be performed. `append_intent` is true for a fresh call and
    /// false for a dangling re-issue (the intent is already recorded); `exec_key`
    /// is the idempotency key to dispatch under (the recorded key on a re-issue).
    Perform {
        /// Whether to write the intent before dispatching.
        append_intent: bool,
        /// The idempotency key handed to the tool for this attempt.
        exec_key: Option<String>,
    },
}

/// Decides the tool step from the log and the registry's effect, mirroring
/// `ReplayCursor::tool_call`'s replay, re-issue, reconciliation, and divergence
/// branches. The effect is the registry's, so a client cannot change it.
fn plan_tool_step(
    log: &[EventEnvelope],
    seq: u64,
    tool: &str,
    input: &Value,
    effect: Effect,
    idempotency_key: Option<&str>,
) -> Result<ToolStepPlan, ApiError> {
    let next = log.len() as u64;
    if seq == next {
        // A fresh intent at the next contiguous position.
        return Ok(ToolStepPlan::Perform {
            append_intent: true,
            exec_key: idempotency_key.map(ToOwned::to_owned),
        });
    }
    if seq > next {
        return Err(ApiError::Divergence(format!(
            "tool-step seq {seq} is beyond the log end {next}"
        )));
    }

    // The position is already recorded: it must be the tool intent, and its
    // (tool, input, effect, key) must all match, exactly as the cursor checks.
    let recorded = &log[seq as usize];
    let Event::ToolCallRequested {
        tool: recorded_tool,
        input: recorded_input,
        effect: recorded_effect,
        idempotency_key: recorded_key,
        ..
    } = &recorded.event
    else {
        return Err(ApiError::Divergence(format!(
            "seq {seq} already holds a non-tool event; it is not a tool-step position"
        )));
    };
    if recorded_tool != tool
        || recorded_input != input
        || *recorded_effect != effect
        || recorded_key.as_deref() != idempotency_key
    {
        return Err(ApiError::Divergence(format!(
            "tool-step at seq {seq} diverges from the recorded intent (tool, input, effect, or key)"
        )));
    }
    match log.get(seq as usize + 1) {
        Some(next_env) => match &next_env.event {
            Event::ToolCallCompleted { seq: corr, output } if corr.get() == seq => {
                Ok(ToolStepPlan::Replay {
                    output: output.clone(),
                })
            }
            _ => Err(ApiError::Divergence(format!(
                "the event after the intent at seq {seq} is not its completion"
            ))),
        },
        // A dangling intent (the last event): the effect decides. Write never
        // re-executes; Read/Idempotent re-execute under the RECORDED key.
        None => match effect {
            Effect::Write => Ok(ToolStepPlan::Reconcile {
                intent: intent_evidence(recorded),
            }),
            Effect::Read | Effect::Idempotent => Ok(ToolStepPlan::Perform {
                append_intent: false,
                exec_key: recorded_key.clone(),
            }),
        },
    }
}

/// The reconciliation evidence carried in a `needs_reconciliation` error body:
/// the recorded write intent plus when it was recorded, mirroring the
/// server-driven resolve's `reconcile_intent` and `json::pending` shapes.
fn intent_evidence(envelope: &EventEnvelope) -> Value {
    let Event::ToolCallRequested {
        seq,
        tool,
        input,
        effect,
        idempotency_key,
    } = &envelope.event
    else {
        return Value::Null;
    };
    json!({
        "kind": "tool",
        "seq": seq.get(),
        "tool": tool,
        "input": input,
        "effect": effect,
        "idempotency_key": idempotency_key,
        "recorded_at": envelope.recorded_at.format(&Rfc3339).unwrap_or_default(),
    })
}

/// Records the `ToolCallCompleted` at `seq + 1`, correlated to the intent at
/// `seq`, after validating it is the legal next event.
async fn append_tool_completion(
    state: &AppState,
    run_id: RunId,
    seq: u64,
    output: &Value,
) -> Result<(), ApiError> {
    let completion = EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq + 1),
        state.now(),
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(seq),
            output: output.clone(),
        },
    );
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let mut validator = LogValidator::new(log);
    validator
        .push(completion.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;
    state
        .store()
        .append(&completion)
        .await
        .map_err(append_error)
}

/// The `200` tool-step body, `{ "output": <json> }`.
fn tool_output_body(output: &Value) -> Json<Value> {
    Json(json!({ "output": output }))
}

/// `POST /v1/client-runs/{id}/resolve`: record a dangling write's completion by
/// hand for a client-driven run, the drive-token-gated twin of the
/// server-driven `POST /v1/runs/{id}/resolve`.
///
/// State-validated exactly like the server-driven resolve: it is legal only
/// when the run's log ends at a dangling `Write` intent, it correlates the
/// caller-supplied output to that intent, and it dispatches nothing. It reuses
/// the same `Runtime::resolve` the server-driven endpoint does, so the two
/// share one reconciliation contract. After it records the completion the run
/// is drivable again, so the client re-fetches the log and its cursor sails
/// past the once-dangling intent.
pub async fn resolve(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    authorize_drive(&state, run_id, &headers)?;
    let request: ResolveRequest = parse_body(&body)?;

    match state.runtime().resolve(run_id, request.output).await {
        Ok(_) => Ok(Json(json!({
            "run": run_id.as_uuid().to_string(),
            "resolved": true,
        }))),
        Err(RuntimeError::NotReconcilable { status, .. }) => Err(ApiError::WrongState(format!(
            "run {} does not need reconciliation (status: {status}); there is no dangling write \
             to resolve",
            run_id.as_uuid()
        ))),
        Err(error) => Err(ApiError::Internal(error.to_string())),
    }
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

/// The per-run lease gate shared by every driving endpoint: the run must be a
/// client-driven run this server opened, and the request must carry its current
/// drive token in the `X-Drive-Token` header. Returns the lease so the caller
/// can read `record_prompts`.
fn authorize_drive(
    state: &AppState,
    run_id: RunId,
    headers: &HeaderMap,
) -> Result<ClientRunLease, ApiError> {
    let lease = state
        .client_run(run_id)
        .ok_or_else(|| unknown_client_run(run_id))?;
    let presented = headers
        .get(DRIVE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    match presented {
        None => Err(ApiError::MissingDriveToken(format!(
            "run {} requires a drive token in the `{DRIVE_TOKEN_HEADER}` header",
            run_id.as_uuid()
        ))),
        Some(token) if token != lease.drive_token => Err(ApiError::InvalidDriveToken(format!(
            "the presented drive token is not the current lease for run {}",
            run_id.as_uuid()
        ))),
        Some(_) => Ok(lease),
    }
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
