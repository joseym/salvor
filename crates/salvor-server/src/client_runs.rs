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
//! `SleepStarted`, `SleepCompleted`, `BudgetExceeded`, `RunCompleted`,
//! `RunFailed`. The side-effecting steps (the model call and the tool call) are
//! not supported here, so a model or tool event is refused with a clear error.
//! Each has its own endpoint pair instead: [`model_step`] and [`tool_step`] for
//! a call this server performs because it holds the key or the binary, and
//! [`client_tool_intent`]/[`client_tool_completion`] and
//! [`client_model_intent`]/[`client_model_completion`] for a call the CLIENT
//! performs in its own process and reports back.
//!
//! # A client-driven run may sleep, and its client wakes it
//!
//! The durable-timer pair belongs on that list for the same reason the
//! suspension pair does: both halves are recorded facts the client's own
//! cursor produces, neither holds a secret, and neither has an effect outside
//! the log. What differs is who ends the wait. Nothing in this process waits
//! for a client-driven run's deadline: the wake sweeper skips every run a
//! client holds a lease on, because re-driving one here would be a second
//! writer racing the client's drive token for the same positions. So the
//! client wakes its own run, the way the runtime does. On a later drive it
//! replays its log, finds a `SleepStarted` with no `SleepCompleted` after it,
//! compares the recorded `wake_at` against a clock reading it records as a
//! `NowObserved`, and either stops (still asleep, nothing appended) or appends
//! the `SleepCompleted` and carries on.
//!
//! This surface enforces only what it can see. The order of the pair is one
//! such thing: a `SleepCompleted` may close only a sleep this log has open
//! (see [`is_sleeping`]). The deadline itself is not, and deliberately so.
//! `wake_at` is the client's own recorded instant and the clock that decides
//! it has arrived is the client's; a server that re-judged it against its own
//! clock would be making a determinism claim about a run it does not drive.
//!
//! # The single-writer lease
//!
//! Opening a run mints a per-run `drive_token`, required on every append. It is
//! the per-run gate that layers on top of the process-wide bearer: one
//! authenticated caller still cannot drive another caller's run, and a second
//! live driver without the current lease is refused. Re-opening a run mints a
//! fresh lease, so a resuming tab always holds the current one.
//!
//! # The lease is process-lived; the run is not
//!
//! The lease registry is in memory and dies with the process, which is right
//! for a lease: a token nobody is holding any more means nothing. What must
//! not die with it is the fact that the run is client-driven at all, because
//! every surface that must not become a second writer (this one when a run is
//! re-opened, [`crate::runs::resume`], the wake sweeper) turns on that fact.
//! So the run records it: [`append`] stamps `driven_by: client` on the
//! `RunStarted` it accepts, and [`log_is_client_driven`] reads it back. A
//! restarted server therefore re-opens a run its client is still driving,
//! keeps refusing to resume it, and still leaves its timer alone, none of
//! which it could do from memory it no longer has.
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
//!
//! # `recorded_at` is stamped here, never trusted from the wire
//!
//! Every [`EventEnvelope`] carries a `recorded_at`. On the server-performed
//! steps ([`model_step`], [`tool_step`], and their completions) it was always
//! [`AppState::now`], because this server built those envelopes itself. This
//! generic append is the one surface where the envelope arrives already built,
//! by the client, and it is the one place `recorded_at` used to be taken on
//! faith: a browser's clock is not this store's clock, and a run with an
//! honest server-performed step next to a client-appended `RunStarted` stamped
//! at the Unix epoch is a store that no longer tells the truth about when
//! things happened. So [`append`] overwrites every incoming envelope's
//! `recorded_at` with [`AppState::now`] before it is folded or written;
//! whatever the client sent in that field is discarded. The event kind, its
//! payload, and its `seq` are still exactly what the client submitted (those
//! remain the client's fact, since the client is the one driving the run); only
//! the "when was this durably recorded" stamp is the server's, uniformly,
//! everywhere an envelope is written.

use std::convert::Infallible;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use salvor_core::{
    Effect, Event, EventEnvelope, LogValidator, Performer, RunId, SequenceNumber, TokenUsage,
};
use salvor_llm::{ContentDelta, MessageAccumulator, StreamEvent};
use salvor_runtime::{
    RuntimeError, hash_value, response_value, usage_of, validate_against_schema, validate_labels,
};
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

/// The body of `POST /v1/client-runs/{id}/client-tool-intent`.
///
/// Notice what is NOT here, next to [`ToolStepRequest`]: no `effect` and no
/// `idempotency_key`. Both come from the operator's declaration or from the
/// server's own derivation, so there is no field for a caller to fill in.
#[derive(Debug, Deserialize)]
struct ClientToolIntentRequest {
    /// The log position the client's cursor reserved for the tool intent.
    seq: u64,
    /// The declared client-performed tool's name. Undeclared is an error, and
    /// no intent is written.
    tool: String,
    /// The input the client is about to perform the call with, checked against
    /// the declared `input_schema` and then recorded on the intent verbatim.
    input: Value,
}

/// The body of `POST /v1/client-runs/{id}/client-tool-completion`.
#[derive(Debug, Deserialize)]
struct ClientToolCompletionRequest {
    /// The intent's position, which must be the pending intent at the log's end.
    seq: u64,
    /// What the client reports the call returned, checked against the declared
    /// `output_schema` before it is recorded.
    output: Value,
}

/// The body of `POST /v1/client-runs/{id}/client-model-intent`.
///
/// Notice what is NOT here, next to [`ModelStepRequest`]: no `request`. The
/// server never sees the request, because it is not the one sending it; the
/// client hashes its own request and reports the hash. Everything this struct
/// carries is therefore the client's claim, which is exactly the trust posture
/// a client-performed tool call already lives under.
#[derive(Debug, Deserialize)]
struct ClientModelIntentRequest {
    /// The log position the client's cursor reserved for the model intent.
    seq: u64,
    /// The client's canonical hash of the request it is about to send. This is
    /// the replay-correlation key, and salvor cannot recompute it: it never
    /// holds the request. A client that hashes inconsistently diverges against
    /// its own log and nobody else's.
    request_hash: String,
    /// The full request, recorded on the intent only when the run was opened
    /// with `record_prompts: true`, exactly as on the server-performed step.
    /// Informational: replay correlates on `request_hash` alone.
    #[serde(default)]
    request_body: Option<Value>,
}

/// The body of `POST /v1/client-runs/{id}/client-model-completion`.
#[derive(Debug, Deserialize)]
struct ClientModelCompletionRequest {
    /// The intent's position, which must be the pending intent at the log's end.
    seq: u64,
    /// What the client reports the provider returned, recorded verbatim.
    response: Value,
    /// The token usage the client reports for the call, in the shape
    /// [`Event::ModelCallCompleted`] records. Required, because it is what a
    /// token budget counts, and a completion that quietly reported none would
    /// under-count every budget the run is held to.
    usage: TokenUsage,
}

/// The body of `POST /v1/client-runs/{id}/resolve`.
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    /// The output to record for the dangling write, verbatim, exactly as the
    /// server-driven resolve takes it.
    output: Value,
}

/// `POST /v1/client-runs`: open a fresh client-driven run, or re-open (resume)
/// one whose log says it is client-driven.
///
/// A fresh run comes back with an empty log and a new drive token; the client
/// appends its own `RunStarted` as the first event through the append endpoint.
/// Re-opening a known client run returns its full recorded log and a fresh
/// lease, for a refreshed tab to rebuild its cursor.
///
/// Two things say a run id is client-driven, and either is enough. The first is
/// this process's own lease registry, which answers for every run opened since
/// the server started. The second is the run's log: the `RunStarted` at its
/// head carries `driven_by: client`, stamped by [`append`] when this server
/// accepted it. The registry dies with the process and the log does not, so
/// without the second an id opened before a restart would be refused as
/// foreign, and a client-driven run would be stranded by any restart, which is
/// the opposite of what a durable log is for. Adopting a run from its log mints
/// a fresh lease exactly as re-opening one this process already held does; the
/// client resumes by rebuilding its cursor from the returned log.
///
/// A chosen id whose history says nothing of the sort is a server-driven run,
/// and it is still refused, so the two modes cannot collide over one store.
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

    let log = state.store().read_log(run_id).await.map_err(store_error)?;

    // A re-open: either this process opened the run (the lease registry knows
    // it, which is also the only evidence a run opened but not yet started has)
    // or its recorded log says it is client-driven (which survives the restart
    // the registry does not). Return the recorded log and a fresh lease.
    if state.is_client_run(run_id) || log_is_client_driven(&log) {
        let drive_token = state.lease_client_run(run_id, request.record_prompts);
        return Ok((StatusCode::OK, Json(open_body(run_id, &drive_token, &log))));
    }

    // A run id with existing history and no client-driven marker is foreign: a
    // server-driven run, whose driver is this process's own. Refuse it rather
    // than adopt it and become a second writer on its log.
    if !log.is_empty() {
        return Err(ApiError::RunExists(format!(
            "run {} already has recorded history and its log does not record it as client-driven; \
             it is a server-driven run, so it cannot be opened for client-driven runs",
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
/// this generic append. A `SleepCompleted` that would close a sleep the log
/// never started is a `409` too, the one pair-ordering rule this surface adds
/// on top of the guard (see [`is_sleeping`]). The whole batch is validated
/// before anything is written, so a batch that turns illegal appends nothing.
///
/// Every envelope's `recorded_at` is overwritten with [`AppState::now`] before
/// it is folded or written (see the module docs): `recorded_at` is the store's
/// fact, not the client's claim, so whatever a submitted envelope carries in
/// that field is never trusted or stored.
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

    for mut candidate in request.events {
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
        //
        // It is also where the run records who drives it. Reaching this line
        // means the caller holds this run's lease, so the run IS client-driven,
        // and the head of its log is the one place that fact can be written
        // down durably. The server stamps it rather than trusting a submitted
        // value, exactly as it does with `recorded_at` just below: what the
        // client sent in the field is discarded, so a caller cannot mark a run
        // client-driven anywhere but here, under a lease this server minted.
        // Stamping before the retry comparison is what keeps a retry
        // byte-identical: the resubmitted event is canonicalized the same way
        // the recorded one was.
        if let Event::RunStarted {
            labels, driven_by, ..
        } = &mut candidate.event
        {
            if let Some(labels) = labels {
                validate_labels(labels).map_err(ApiError::BadRequest)?;
            }
            *driven_by = Some(Performer::Client);
        }

        let next_seq = validator.next_seq();
        if candidate.seq < next_seq {
            // An already-recorded position: idempotent retry or divergence.
            // `recorded_at` is the store's fact, not the client's claim (see
            // the module docs), so a retry's legality never turns on whatever
            // timestamp this attempt happened to carry: canonicalize it to
            // the already-recorded stamp before comparing the rest byte for
            // byte.
            let index = candidate.seq.get() as usize;
            let recorded = &validator.log()[index];
            candidate.recorded_at = recorded.recorded_at;
            if *recorded == candidate {
                appended.push(candidate.seq.get());
                continue;
            }
            return Err(ApiError::Divergence(format!(
                "different bytes submitted at the already-recorded seq {}",
                candidate.seq.get()
            )));
        }

        // The durable-timer pair's order is this surface's to check, because
        // the shared append-guard deliberately does not (see [`is_sleeping`]).
        // The working log, not the stored one, is what a batch carrying both
        // halves at once must be judged against.
        if matches!(candidate.event, Event::SleepCompleted {}) && !is_sleeping(validator.log()) {
            return Err(ApiError::Divergence(format!(
                "the SleepCompleted at seq {} would close a sleep this run has not started",
                candidate.seq.get()
            )));
        }

        // A new position: the server stamps its own clock reading, the same
        // source every server-performed step uses, and ignores whatever
        // `recorded_at` the client submitted. `recorded_at` is the store's
        // fact, not the client's claim.
        candidate.recorded_at = state.now();

        // The append-guard decides legality.
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
                        // This server is about to make the call itself, so the
                        // performer stays unrecorded: absent means salvor
                        // witnessed it, which is what every model intent
                        // written before the field existed meant.
                        performed_by: None,
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
        performed_by,
        ..
    } = &recorded.event
    else {
        return Err(ApiError::Divergence(format!(
            "seq {seq} already holds a non-model event; it is not a model-step position"
        )));
    };
    // A call the CLIENT performed is not this endpoint's to re-issue or to
    // answer. Re-issuing it would let this server witness and record a response
    // for an intent the log attributes to the client, smearing the one
    // distinction `performed_by` exists to keep; and a cursor that asks this
    // server to perform a step its own log says the client performed has
    // genuinely diverged from that log. Close it with client-model-completion.
    if *performed_by == Some(Performer::Client) {
        return Err(ApiError::Divergence(format!(
            "the model intent at seq {seq} was performed by the client, so this server may not \
             perform or answer it; record its result with POST \
             /v1/client-runs/{{id}}/client-model-completion"
        )));
    }
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
                        performed_by: None,
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
                // Refused for the same reason a suspension is: a step endpoint
                // performs one call and answers with its output. Parking the
                // run belongs to a driver, and the client owns the loop here.
                ToolOutcome::Sleep(_) => {
                    return Err(ApiError::ToolExecution(format!(
                        "tool `{tool_name}` asked to sleep, which a server-performed tool step \
                         does not support; no completion recorded"
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
            Event::ToolCallCompleted {
                seq: corr, output, ..
            } if corr.get() == seq => Ok(ToolStepPlan::Replay {
                output: output.clone(),
            }),
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
        ..
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
            deduplicated_from: None,
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

/// The idempotency key a CLIENT-performed tool call presents: derived by this
/// server from where the call sits in the run (run id, sequence, tool name),
/// never supplied by the caller.
///
/// This deliberately differs from the server-performed [`tool_step`], where the
/// client supplies `idempotency_key` on the request body and the server records
/// what it was given. The difference is not an oversight, and the older
/// endpoint should not be "fixed" to match.
///
/// There, salvor performs the call. The party choosing the key is not the party
/// making the write, and a key chosen badly costs the caller nothing but its own
/// retry failing to collapse. Here the client both chooses the key and performs
/// the write, in a process salvor never sees. That is the one case where the
/// party choosing the key is also the party who benefits from a duplicate
/// landing: a client that wants to be paid twice supplies a fresh key for the
/// second attempt and the provider, seeing two distinct calls, honors both,
/// while salvor's log shows two honest-looking intents. Deriving the key removes
/// the choice. The same (run, seq, tool) always derives the same key, so an
/// honest retry after a dropped response presents the identical key the first
/// attempt did and the provider collapses the pair, and a second attempt cannot
/// present a different one.
///
/// Shaped after `salvor_engine`'s `fork_safe_idempotency_key`: a canonical hash
/// of a small JSON object, using the same `hash_value` the rest of the workspace
/// hashes with, so the key is reproducible across processes and languages and a
/// client can derive it independently to check the server's work.
fn client_tool_idempotency_key(run_id: RunId, seq: u64, tool: &str) -> String {
    hash_value(&json!({
        "run": run_id.as_uuid().to_string(),
        "seq": seq,
        "tool": tool,
    }))
}

/// `POST /v1/client-runs/{id}/client-tool-intent`: open a client-performed tool
/// call.
///
/// The counterpart of [`tool_step`] for a tool salvor holds no code for. The
/// client is about to run the call in its OWN process, with its own secrets;
/// this endpoint records that it is about to, so the intent is in the log before
/// the effect happens, exactly as the write-ahead rule demands of a call salvor
/// performs itself. Requires the `X-Drive-Token` header, like every other
/// driving endpoint.
///
/// What the server takes from the operator's declaration rather than the
/// request: the [`Effect`] (so a caller cannot up- or down-grade its own write
/// into a freely retried read), the input schema the input is checked against
/// before anything is written, and the idempotency key, which is DERIVED here
/// (see [`client_tool_idempotency_key`]). The client supplies only the position,
/// the name, and the input.
///
/// The intent goes through the same [`LogValidator`] guard every other append on
/// this surface uses, so ordering and correlation stay enforced: an intent at a
/// position the log is not ready for is a `409 divergence` and nothing is
/// written. A byte-identical re-post at an already-recorded position is a `200`
/// that re-derives the same key and writes nothing, the safe retry a dropped
/// response leaves behind.
///
/// The response carries the derived key and a `settled` flag: `true` when the
/// intent at this position already has its completion recorded, so a caller
/// re-posting an intent it believes it already opened can tell "safe to
/// perform" from "already done" without reading the log. The client performs
/// the work under the key and then posts [`client_tool_completion`].
pub async fn client_tool_intent(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "client-tool-intent body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: ClientToolIntentRequest = parse_body(&body)?;

    // The declaration is looked up before anything is written. Declarations are
    // loaded by the operator and never registered over HTTP (see
    // `crate::client_tools`), so an unknown name is a `404` the operator fixes,
    // not something a caller can create for itself.
    let decls = state.client_tools();
    let decl = decls.get(&request.tool).ok_or_else(|| {
        ApiError::UnknownTool(format!(
            "no client-performed tool named `{}` is declared on this server; declarations are \
             loaded by the operator (`salvor serve --client-tool <FILE>`) and are never \
             registered over HTTP",
            request.tool
        ))
    })?;

    // The input is checked against the OPERATOR's schema before the intent is
    // recorded, so a malformed call never becomes history: on the failure path
    // this endpoint writes nothing at all and the run is untouched.
    validate_against_schema(&request.input, &decl.input_schema).map_err(|error| {
        ApiError::BadRequest(format!(
            "the input does not match the declared input_schema for `{}`: {error}",
            request.tool
        ))
    })?;
    let effect = decl.effect;

    let key = client_tool_idempotency_key(run_id, request.seq, &request.tool);
    let intent = EventEnvelope::new(
        run_id,
        SequenceNumber::new(request.seq),
        state.now(),
        Event::ToolCallRequested {
            seq: SequenceNumber::new(request.seq),
            tool: request.tool.clone(),
            input: request.input.clone(),
            effect,
            idempotency_key: Some(key.clone()),
            // The whole point of the stage: the log says who performed this, so
            // a later reader can tell a call salvor witnessed from a call it was
            // told about.
            performed_by: Some(Performer::Client),
        },
    );

    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if (request.seq as usize) < log.len() {
        // An already-recorded position. The derivation is a pure function of
        // (run, seq, tool), so an identical re-post re-derives the recorded key
        // and can simply be handed it back: the client retries its own call
        // under the same key and the provider collapses the duplicate. Compare
        // the events rather than the envelopes, because `recorded_at` is this
        // store's stamp from the first attempt and would never match a fresh one.
        let recorded = &log[request.seq as usize];
        if recorded.event == intent.event {
            let settled = intent_is_settled(&log, request.seq);
            return Ok(Json(intent_body(request.seq, &key, effect, settled)));
        }
        return Err(ApiError::Divergence(format!(
            "seq {} already holds a different event; it is not this client-tool intent's position",
            request.seq
        )));
    }

    // The same append-guard the generic append and both server-performed steps
    // push through: it decides whether this is the legal next event.
    let mut validator = LogValidator::new(log);
    validator
        .push(intent.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;
    state.store().append(&intent).await.map_err(append_error)?;
    // A freshly-recorded intent can never already be settled: the append above
    // just placed it at the log's new end, with nothing after it yet.
    Ok(Json(intent_body(request.seq, &key, effect, false)))
}

/// Whether the tool intent at `seq` already has its `ToolCallCompleted`
/// recorded in `log`. The append-guard only ever admits a completion for the
/// same `seq` immediately after its intent (see [`append_tool_completion`]),
/// so it is enough to check the very next slot.
fn intent_is_settled(log: &[EventEnvelope], seq: u64) -> bool {
    log.get(seq as usize + 1).is_some_and(|envelope| {
        matches!(
            &envelope.event,
            Event::ToolCallCompleted { seq: completed_seq, .. } if completed_seq.get() == seq
        )
    })
}

/// The `200` client-tool-intent body: the position, the DERIVED idempotency key
/// the client must perform under, the operator-declared effect it was
/// recorded with, and whether this position's completion is ALREADY recorded.
///
/// `settled` exists for a caller re-posting an intent it already believes it
/// opened, most pointedly a payments caller checking a write before it acts on
/// the response: without it, a retried intent and a fresh one look identical
/// (same `200`, same key), and a caller cannot tell "safe to perform" from
/// "already done, do not perform it again" without separately reading the log.
/// On a freshly-recorded intent it is always `false`; on a byte-identical
/// re-post it reflects whether the completion has landed since.
fn intent_body(seq: u64, idempotency_key: &str, effect: Effect, settled: bool) -> Value {
    json!({
        "seq": seq,
        "idempotency_key": idempotency_key,
        "effect": effect,
        "settled": settled,
    })
}

/// `POST /v1/client-runs/{id}/client-tool-completion`: record that a
/// client-performed tool call finished.
///
/// The client ran the call in its own process and is now reporting the result.
/// Salvor did not witness it, so everything this endpoint can check, it checks
/// before the report becomes history. Requires the `X-Drive-Token` header.
///
/// It refuses, recording nothing, when:
///
/// - the log does not end at a tool intent, or ends at one whose `seq` is not
///   the one this request names (`409 divergence`);
/// - the pending intent was performed by the SERVER (`403`): a client must not
///   close a call salvor made, since salvor holds the real result;
/// - the declaration says `trust_completion = false` (`403`);
/// - the declaration carries no `output_schema` (`403`): with nothing to check
///   the report against, the completion is unfalsifiable, which is exactly what
///   the schema exists to prevent;
/// - the reported output fails the declared `output_schema` (`400`);
/// - a `require_equal` field's reported value differs from the value the intent
///   recorded (`403`): the output schema is a shape check and cannot know what
///   was authorized, so a client report may not alter a pinned field.
///
/// The checks run in that order: the trust refusal fires before any value is
/// compared, then the output shape, then the per-field equality.
///
/// # Where a refused completion leaves the run, and why nothing else changes
///
/// A refusal is not a dead end and needed no new state to express. The log still
/// ends at the recorded `ToolCallRequested`, and for an `Effect::Write` the pure
/// fold in `salvor-replay` ALREADY reports that as
/// [`RunStatus::NeedsReconciliation`](salvor_replay::RunStatus), because an
/// uncompleted write intent as the log's last word is precisely what that status
/// means. `POST /v1/client-runs/{id}/resolve` already exists to settle it by
/// hand, once a person has verified externally whether the call landed.
///
/// So `trust_completion = false` is fully implemented here, at the completion
/// boundary, and deliberately NOT in `derive_state`. That fold is a pure
/// function of the log with no access to declarations, and it must stay that
/// way: a log has to mean the same thing to a replay on another machine that
/// has never seen this server's `--client-tool` files. A later reader who goes
/// looking for the strict mode in the fold will not find it, and that is the
/// design, not an omission.
pub async fn client_tool_completion(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "client-tool-completion body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: ClientToolCompletionRequest = parse_body(&body)?;

    // A completion settles the log's LAST event, which must be the intent this
    // request names. Anything else and the client and the log disagree about
    // what is outstanding.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let pending = log.last().ok_or_else(|| {
        ApiError::Divergence(format!(
            "run {} has recorded nothing, so it has no client-performed tool call to complete",
            run_id.as_uuid()
        ))
    })?;
    let Event::ToolCallRequested {
        seq: intent_seq,
        tool,
        input: intent_input,
        performed_by,
        ..
    } = &pending.event
    else {
        return Err(ApiError::Divergence(format!(
            "run {} does not end at a tool intent, so there is no tool call to complete",
            run_id.as_uuid()
        )));
    };
    if intent_seq.get() != request.seq {
        return Err(ApiError::Divergence(format!(
            "the pending tool intent is at seq {}, not the seq {} this completion names",
            intent_seq.get(),
            request.seq
        )));
    }
    // A client may close only a call a client made. The server-performed
    // tool-step records its own completion from the output it saw, so a client
    // completion there would be overwriting a witnessed fact with a claim.
    if *performed_by != Some(Performer::Client) {
        return Err(ApiError::ClientCompletionRefused(format!(
            "the pending tool call at seq {} was performed by this server, not by the client, so \
             a client may not record its completion",
            request.seq
        )));
    }
    let tool = tool.clone();

    let decls = state.client_tools();
    let decl = decls.get(&tool).ok_or_else(|| {
        ApiError::UnknownTool(format!(
            "no client-performed tool named `{tool}` is declared on this server, so the completion \
             reported for the intent at seq {} cannot be checked",
            request.seq
        ))
    })?;

    if !decl.trust_completion {
        return Err(ApiError::ClientCompletionRefused(format!(
            "tool `{tool}` is declared with trust_completion = false, so a client may not record \
             its own completion for it; verify the call externally, then settle it by hand with \
             POST /v1/client-runs/{}/resolve",
            run_id.as_uuid()
        )));
    }
    let Some(output_schema) = &decl.output_schema else {
        return Err(ApiError::ClientCompletionRefused(format!(
            "tool `{tool}` declares no output_schema, so a client-reported completion carries \
             nothing this server can check; declare an output_schema for it, or settle the call \
             by hand with POST /v1/client-runs/{}/resolve",
            run_id.as_uuid()
        )));
    };
    validate_against_schema(&request.output, output_schema).map_err(|error| {
        ApiError::BadRequest(format!(
            "the reported output does not match the declared output_schema for `{tool}`: {error}"
        ))
    })?;

    // The output schema is a shape check and cannot know what was authorized, so
    // a report claiming a different amount than the intent recorded passes it. A
    // require_equal field closes that gap: the reported value must be JSON-equal
    // to the value the intent recorded. The load-time rule guarantees each named
    // field is required on both sides, so both values are present to compare.
    for field in &decl.require_equal {
        let authorized = intent_input.get(field).unwrap_or(&Value::Null);
        let reported = request.output.get(field).unwrap_or(&Value::Null);
        if authorized != reported {
            return Err(ApiError::ClientCompletionRefused(format!(
                "tool `{tool}` reported `{field}` as {reported} for the intent at seq {}, but the \
                 intent recorded {authorized}; a client report may not alter a require_equal field. \
                 If the provider genuinely did something different, settle it by hand with POST \
                 /v1/client-runs/{}/resolve",
                request.seq,
                run_id.as_uuid()
            )));
        }
    }

    // The completion goes through the same guard and the same helper the
    // server-performed tool step records its own completion with, so the two
    // surfaces write byte-identical `ToolCallCompleted` events.
    append_tool_completion(&state, run_id, request.seq, &request.output).await?;
    Ok(Json(json!({
        "seq": request.seq,
        "completed": true,
    })))
}

/// `POST /v1/client-runs/{id}/client-model-intent`: open a model call the
/// CLIENT performs.
///
/// The counterpart of [`model_step`] for a call this server does not make. The
/// client is about to call the provider in its OWN process, with its own key
/// and its own model configuration; this endpoint records that it is about to,
/// so the intent is in the log before the call happens, exactly as the
/// write-ahead rule demands of a call salvor performs itself. Requires the
/// `X-Drive-Token` header, like every other driving endpoint.
///
/// # What salvor is trusting, and what it buys
///
/// [`model_step`] recomputes `request_hash` from the request body it was handed,
/// so the client cannot record a hash that does not match what was sent. Here
/// it cannot: the request never reaches this server, because this server is not
/// the one sending it. The hash is the client's claim over its own request, and
/// the recorded response is the client's claim about what came back, in exactly
/// the sense a client-performed tool result is (see [`Performer`]). Salvor did
/// not witness the call; it is trusting the report.
///
/// What the trust buys is the whole point of the feature: a resume replays the
/// recorded answer instead of paying the provider for it a second time. The
/// claim is also self-punishing rather than dangerous to anyone else, which is
/// why it is safe to take: the hash is a key into this run's own log, so a
/// client that hashes inconsistently diverges against its own history and
/// nobody else's.
///
/// # Replay, mirroring [`client_tool_intent`] exactly
///
/// A recorded intent at this position whose `request_hash` matches is a replay:
/// nothing is written, and the answer carries the recorded completion when one
/// exists, so a middleware can short-circuit without a separate log read. A
/// different hash there, a non-model event, or an intent the SERVER performed is
/// `409 divergence` and nothing is written. A fresh position goes through the
/// same [`LogValidator`] guard every other append on this surface uses.
pub async fn client_model_intent(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let lease = authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "client-model-intent body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: ClientModelIntentRequest = parse_body(&body)?;

    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if (request.seq as usize) < log.len() {
        // An already-recorded position. Correlation is on the hash alone, the
        // identical rule [`plan_model_step`] and `ReplayCursor::model_call`
        // use: the body is informational and a log captured with bodies must
        // replay the same as one captured without, so a re-post that omits the
        // body it once sent is still the same call.
        let recorded = &log[request.seq as usize];
        let Event::ModelCallRequested {
            request_hash: recorded_hash,
            performed_by,
            ..
        } = &recorded.event
        else {
            return Err(ApiError::Divergence(format!(
                "seq {} already holds a non-model event; it is not this client-model intent's \
                 position",
                request.seq
            )));
        };
        if *performed_by != Some(Performer::Client) {
            return Err(ApiError::Divergence(format!(
                "the model intent at seq {} was performed by this server, not by the client",
                request.seq
            )));
        }
        if recorded_hash != &request.request_hash {
            return Err(ApiError::Divergence(format!(
                "the model intent at seq {} carries a request hash that differs from the recorded \
                 one",
                request.seq
            )));
        }
        return Ok(Json(client_model_intent_body(
            request.seq,
            recorded_completion(&log, request.seq),
        )));
    }

    // Recorded only when the run was opened with `record_prompts: true`, the
    // same rule and the same `Option::then` shape the server-performed step
    // reads off the lease. A body sent with recording off is dropped here and
    // never written.
    let request_body = lease
        .record_prompts
        .then_some(request.request_body)
        .flatten();
    let intent = EventEnvelope::new(
        run_id,
        SequenceNumber::new(request.seq),
        state.now(),
        Event::ModelCallRequested {
            seq: SequenceNumber::new(request.seq),
            request_hash: request.request_hash.clone(),
            request_body,
            // The whole point of the endpoint: the log says who performed this,
            // so a later reader can tell a call salvor witnessed from a call it
            // was told about.
            performed_by: Some(Performer::Client),
        },
    );

    let mut validator = LogValidator::new(log);
    validator
        .push(intent.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;
    state.store().append(&intent).await.map_err(append_error)?;
    // A freshly-recorded intent can never already be settled: the append above
    // just placed it at the log's new end, with nothing after it yet.
    Ok(Json(client_model_intent_body(request.seq, None)))
}

/// The recorded `(response, usage)` for the model intent at `seq`, when its
/// completion is already in `log`.
///
/// The append-guard only ever admits a completion for the same `seq`
/// immediately after its intent, so it is enough to check the very next slot,
/// the same way [`intent_is_settled`] checks a tool intent's.
fn recorded_completion(log: &[EventEnvelope], seq: u64) -> Option<(&Value, TokenUsage)> {
    match &log.get(seq as usize + 1)?.event {
        Event::ModelCallCompleted {
            seq: completed_seq,
            response,
            usage,
        } if completed_seq.get() == seq => Some((response, *usage)),
        _ => None,
    }
}

/// The `200` client-model-intent body: the position, whether this position's
/// completion is ALREADY recorded, and, when it is, that completion.
///
/// `settled` is the same flag [`intent_body`] carries for a tool intent, and it
/// is here for a sharper version of the same reason. A middleware re-posting an
/// intent it believes it already opened cannot otherwise tell "safe to call the
/// provider" from "already called, do not pay for it again", and paying twice
/// is precisely what recording the call was for. So the recorded completion
/// rides along on a settled answer: the middleware short-circuits on the
/// response it already has, with no second request.
fn client_model_intent_body(seq: u64, completion: Option<(&Value, TokenUsage)>) -> Value {
    match completion {
        Some((response, usage)) => json!({
            "seq": seq,
            "settled": true,
            "response": response,
            "usage": usage,
        }),
        None => json!({ "seq": seq, "settled": false }),
    }
}

/// `POST /v1/client-runs/{id}/client-model-completion`: record that a
/// client-performed model call finished.
///
/// The client called the provider in its own process and is now reporting the
/// response and what it cost. Requires the `X-Drive-Token` header.
///
/// It refuses, recording nothing, when:
///
/// - the log does not end at a model intent, or ends at one whose `seq` is not
///   the one this request names (`409 divergence`);
/// - the pending intent was performed by the SERVER (`403`): a client must not
///   close a call salvor made, since salvor holds the real response.
///
/// That is the whole list, and it is shorter than [`client_tool_completion`]'s
/// on purpose. The tool completion's remaining refusals all come from the
/// operator's declaration (`trust_completion`, `output_schema`,
/// `require_equal`), and a model call has no such declaration to check against:
/// its response shape is the provider's, not an operator's. The response is
/// recorded verbatim, as the server-performed step records its own.
///
/// Once recorded, the completion is byte-identical to a server-performed one
/// (it goes through the same [`append_completion`] helper), so the fold treats
/// the call exactly the same: pending while open, closed by this event, and its
/// tokens counted toward every budget the run is held to.
pub async fn client_model_completion(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    authorize_drive(&state, run_id, &headers)?;

    if body.len() > MAX_EVENTS_BODY {
        return Err(ApiError::PayloadTooLarge(format!(
            "client-model-completion body is {} bytes, over the {MAX_EVENTS_BODY}-byte cap",
            body.len()
        )));
    }
    let request: ClientModelCompletionRequest = parse_body(&body)?;

    // A completion settles the log's LAST event, which must be the intent this
    // request names. Anything else and the client and the log disagree about
    // what is outstanding.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let pending = log.last().ok_or_else(|| {
        ApiError::Divergence(format!(
            "run {} has recorded nothing, so it has no client-performed model call to complete",
            run_id.as_uuid()
        ))
    })?;
    let Event::ModelCallRequested {
        seq: intent_seq,
        performed_by,
        ..
    } = &pending.event
    else {
        return Err(ApiError::Divergence(format!(
            "run {} does not end at a model intent, so there is no model call to complete",
            run_id.as_uuid()
        )));
    };
    if intent_seq.get() != request.seq {
        return Err(ApiError::Divergence(format!(
            "the pending model intent is at seq {}, not the seq {} this completion names",
            intent_seq.get(),
            request.seq
        )));
    }
    // A client may close only a call a client made. The server-performed
    // model-step records its own completion from the response it saw, so a
    // client completion there would be overwriting a witnessed fact with a
    // claim.
    if *performed_by != Some(Performer::Client) {
        return Err(ApiError::ClientCompletionRefused(format!(
            "the pending model call at seq {} was performed by this server, not by the client, so \
             a client may not record its completion",
            request.seq
        )));
    }

    // The same guard and the same helper the server-performed model step
    // records its own completion with, so the two surfaces write byte-identical
    // `ModelCallCompleted` events.
    append_completion(
        &state,
        run_id,
        request.seq,
        &request.response,
        request.usage,
    )
    .await?;
    Ok(Json(json!({
        "seq": request.seq,
        "completed": true,
    })))
}

/// Refuses a model or tool event on the generic append: those are recorded
/// through the server-performed model-step and tool-step endpoints, or, for a
/// call the CLIENT performs in its own process, through the client-tool-intent
/// and client-tool-completion endpoints, or the client-model-intent and
/// client-model-completion pair. All four kinds stay refused here.
///
/// A client-performed tool call is possible, in other words; it is just not
/// possible by hand-appending an event. That is the same rule the server-
/// performed steps live under, and for the same reason: the effect class, the
/// input check, and the idempotency key are the server's to decide from an
/// operator's declaration, and an event submitted whole would carry the caller's
/// answers to all three.
///
/// A client-performed MODEL call is possible on the same terms, and stays
/// refused here for a narrower reason: the endpoints are where `performed_by`
/// is stamped, where prompt recording is read off the run's lease rather than
/// taken from the request, and where a completion is checked against the intent
/// it claims to close. An event submitted whole would carry the caller's
/// answers to all three, including the ability to write `performed_by: null` on
/// a call salvor never made and pass a claim off as a witnessed fact.
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
         the model-step or tool-step endpoint, or, for a call the client performs itself, through \
         the client-model or client-tool endpoint pair"
    )))
}

/// Whether `log` leaves the run asleep: the last durable-timer event it holds
/// is a `SleepStarted` that no `SleepCompleted` has closed.
///
/// The pure append-guard is lenient about the pair on purpose, mirroring the
/// cursor: a run that is still asleep has recorded only the start, so nothing
/// may demand the completion. That leniency leaves one shape it cannot refuse,
/// a `SleepCompleted` for a run that was never asleep, and on this surface that
/// is a real mistake a driver can make, since here the client hand-appends both
/// halves itself. Checking it at the endpoint keeps the pair ordered without
/// teaching the shared guard a rule the runtime's own cursor does not enforce.
fn is_sleeping(log: &[EventEnvelope]) -> bool {
    log.iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            Event::SleepStarted { .. } => Some(true),
            Event::SleepCompleted {} => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// Whether `log` is a client-driven run's own log, on the log's own evidence:
/// the `RunStarted` at its head carries `driven_by: client`.
///
/// This is the durable half of the answer to "who drives this run". The other
/// half is [`AppState::is_client_run`], the in-memory lease registry, which is
/// authoritative only for runs this process opened and knows nothing after a
/// restart. Every surface that must not become a second writer against a
/// client's drive token asks both: [`open`] (to adopt rather than refuse a run
/// from an earlier process), [`crate::runs::resume`] (to keep refusing it), and
/// the wake sweeper (to keep leaving its timer to its client). Asking only the
/// registry would make a restart quietly re-arm this server as a driver of runs
/// it does not own.
///
/// The check itself lives in [`salvor_replay::log_is_client_driven`], the
/// pure crate both this server and `salvor-cli`'s `wake` sweep depend on, so
/// the two processes that must each leave a client-driven run alone read the
/// same marker the same way. This wrapper only narrows visibility to the
/// crate, matching the narrower one this module used before the check moved.
pub(crate) fn log_is_client_driven(log: &[EventEnvelope]) -> bool {
    salvor_replay::log_is_client_driven(log)
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
        Some(_) => {
            // The driver presented its current token: it is alive. Refresh the
            // lease's `last_seen` so the liveness evidence on GET /v1/runs reads
            // "attached". This is the whole heartbeat: it rides on the real
            // guarded operation, never a separate ping.
            state.touch_client_run(run_id);
            Ok(lease)
        }
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
