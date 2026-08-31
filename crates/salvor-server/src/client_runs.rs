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
//! live driver without the current lease is refused.
//!
//! A lease is held until it lapses. Re-opening a run whose lease is still
//! current is refused with `409 lease_held` rather than handed a fresh token,
//! because the caller re-opening is usually not the driver that already has the
//! run: two app instances on one thread, a duplicated tab, a retrying
//! middleware. Handing the newest caller the lease would put both of them to
//! work on the same log, and the one that loses the race to a position dies on
//! a divergence after having already run the step. The driver that holds the
//! run keeps it until it goes quiet for the lease TTL or the run finishes; the
//! refusal says how long that is, so the second caller waits instead of
//! polling.
//!
//! Lapsing is the safety net, not the way a drive is meant to end. A driver
//! that is finished says so with [`release`], and the run is another driver's
//! on the very next request rather than a TTL later; a driver that will be busy
//! for longer than the TTL, inside one tool body or one streamed model call,
//! says THAT with [`heartbeat`], and keeps the run it never actually left.
//! Without the first, a short-lived process locks the process that follows it
//! out for a minute for nothing; without the second, a slow step loses a run
//! its driver is still working on. Recording a dangling write by hand drops the
//! lease as well, because a write nobody came back to record is a driver that
//! is gone (see [`resolve`] and [`crate::runs::resolve`]).
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
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::ACCEPT;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use salvor_core::{
    DedupOrigin, Effect, Event, EventEnvelope, LogValidator, Performer, RunId, RunStatus,
    SequenceNumber, TokenUsage, derive_state,
};
use salvor_llm::{ContentDelta, MessageAccumulator, StreamEvent};
use salvor_runtime::{
    RuntimeError, ToolFailure, ToolFailureKind, encode_failure, hash_value, response_value,
    usage_of, validate_against_schema, validate_labels,
};
use salvor_store::{CallClaim, CallClaimant};
use salvor_tools::{ToolCtx, ToolOutcome};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::error::ApiError;
use crate::executor::{ModelExecutor, ModelStream};
use crate::state::{AppState, ClientRunLease, LeaseRelease};
use crate::tokens;
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
///
/// Two shapes, and exactly one of them: `output` for a call that returned a
/// result, `error` for a call that did not. A body carrying both, or neither,
/// is refused, because the two say opposite things about the same call and this
/// server has no way to pick.
#[derive(Debug, Deserialize)]
struct ClientToolCompletionRequest {
    /// The intent's position, which must be the pending intent at the log's end.
    seq: u64,
    /// What the client reports the call returned, checked against the declared
    /// `output_schema` before it is recorded.
    #[serde(default)]
    output: Option<Value>,
    /// What the client reports went wrong instead, when the call produced no
    /// result at all.
    #[serde(default)]
    error: Option<ReportedFailure>,
}

/// A failure a client reports for a call it performed, the `error` half of
/// [`ClientToolCompletionRequest`].
///
/// It carries what the client can honestly say and nothing more. `attempts` is
/// not on the wire: it counts executions inside salvor's own retry loop, and
/// there is no such loop here, so the recorded failure says one attempt rather
/// than taking a number from a caller that could say anything.
#[derive(Debug, Deserialize)]
struct ReportedFailure {
    /// The failure, in full. Recorded verbatim as the sentinel's `message`, the
    /// same field a native tool's error chain lands in.
    message: String,
    /// Which dispatch layer failed, one of `invalid_input`, `handler`, or
    /// `output_serialization`. Absent means `handler`, which is what a client
    /// tool that ran and threw is: the layers either side of it are salvor's
    /// own argument checking and result decoding, and neither exists for a call
    /// salvor never dispatched.
    #[serde(default)]
    kind: Option<String>,
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
/// # A held lease is not taken away
///
/// A re-open only mints a fresh lease when nobody is driving the run: this
/// process holds no lease for it (it never opened it, or it restarted), the
/// lease it holds has lapsed because the driver went quiet for the TTL, or the
/// run has finished and there is nothing left to drive. While a driver's lease
/// is current, a re-open from anyone else is `409 lease_held`, carrying
/// `details.lapses_in_seconds` so the caller knows when the hold expires.
///
/// The alternative, handing the run to whoever asked most recently, reads
/// well for the one case it was written for (a tab the user refreshed, whose
/// old driver is gone) and badly for every other: two app instances on one
/// thread, a duplicated tab, a middleware that re-opens on a failed call. Each
/// of those leaves two live drivers appending the same steps to one log, and
/// the one that loses a position race takes a divergence after it has already
/// done the work. A refreshed tab still resumes as before, because its old
/// driver stopped presenting a token and its lease lapses.
///
/// A driver re-opening its OWN run, presenting its current token in the
/// `X-Drive-Token` header, is allowed and keeps the lease it already has: it
/// gets the recorded log back under the same token, which is what a client
/// rebuilding its cursor after losing local state needs, and no second writer
/// appears because the only writer is the one asking. No new token is minted,
/// so a request already in flight under that token is not invalidated by the
/// re-open. `record_prompts` on such a re-open is ignored; the lease keeps the
/// setting it was opened with.
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
    headers: HeaderMap,
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
    // the registry does not). Return the recorded log, and a fresh lease unless
    // a driver still holds one.
    if state.is_client_run(run_id) || log_is_client_driven(&log) {
        if let Some((held, remaining)) = state.current_client_lease(run_id)
            && !run_is_finished(&log)
        {
            let presented = headers
                .get(DRIVE_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok());
            // Constant-time, like every other comparison of secret material
            // in this crate: a drive token is a lease credential.
            if !presented.is_some_and(|token| tokens::secrets_equal(token, &held.drive_token)) {
                return Err(lease_held(run_id, remaining));
            }
            // The holder re-opening its own run. It is the only writer either
            // way, so nothing needs taking away: hand back the recorded log
            // under the token it already has, and count the request as the
            // proof of life it is.
            state.touch_client_run(run_id);
            return Ok((
                StatusCode::OK,
                Json(open_body(run_id, &held.drive_token, &log)),
            ));
        }
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

/// `POST /v1/client-runs/{id}/release`: hand the lease back, so the next open
/// takes the run at once instead of waiting out the TTL.
///
/// A driver that is finished for now says so with this rather than by going
/// quiet. The lapse is the safety net for a driver that cannot say anything
/// any more (it crashed, the tab closed); it is a poor way to end a drive that
/// ended in an orderly fashion, because the run stays unopenable for the rest
/// of the TTL. That is exactly what a short-lived process hits: an SDK invoke
/// returns, the process exits, and the very next process is refused
/// `409 lease_held` for up to a minute for no reason at all.
///
/// Only the lease goes. The log is untouched, and the run keeps its recorded
/// `driven_by: client`, so it is still a client-driven run: a later open adopts
/// it exactly as it adopts one after a restart, `POST /v1/runs/{id}/resume`
/// still refuses it, and the wake sweeper still leaves its timer to its client,
/// all three of which read that marker rather than this registry.
///
/// Idempotent: a run with no lease here (already released, lapsed, or never
/// opened by this process) answers `200` with `released: false`. Nothing to
/// give back is not an error, because the caller's goal, a run nobody is
/// holding, is already true. Presenting a token that is not the current lease,
/// or none at all, IS refused (`403 invalid_drive_token`), because that caller
/// is asking to end somebody else's hold.
pub async fn release(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let presented = headers
        .get(DRIVE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    match state.release_client_run(run_id, presented) {
        LeaseRelease::Released => Ok(Json(json!({ "released": true }))),
        LeaseRelease::NoLease => Ok(Json(json!({ "released": false }))),
        // A missing token lands here alongside a wrong one, unlike the driving
        // endpoints, which answer `401 missing_drive_token` for it. Here the
        // question is not "did you bring credentials" but "is this hold
        // yours to end", and the answer to that is no either way.
        LeaseRelease::NotTheHolder => Err(ApiError::InvalidDriveToken(format!(
            "the presented drive token is not the current lease for run {}, so it cannot release it",
            run_id.as_uuid()
        ))),
    }
}

/// `POST /v1/client-runs/{id}/heartbeat`: refresh the lease without driving.
/// Requires the `X-Drive-Token` header.
///
/// Presenting the drive token has always been the heartbeat, and every driving
/// call carries it. What that misses is the driver that is busy for longer than
/// the TTL between two calls: a tool that takes minutes, a model body streaming
/// to the client's own screen. Nothing refreshes the lease while that runs, so
/// it lapses mid-work and another opener can take the run out from under a
/// driver that never went anywhere. So a driver with a long stretch of work
/// ahead of it beats every so often instead.
///
/// The answer carries `lapses_in_seconds`, the whole TTL as of this beat, which
/// is what a client needs to pick its interval without being told the server's
/// configuration some other way.
///
/// A driver could get the same effect by re-opening the run under its own token
/// (that keeps the lease and counts as proof of life), and that is what the
/// SDKs did before this existed. It re-reads the whole recorded log every beat
/// to do it, which is the wrong price for saying "still here".
pub async fn heartbeat(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    // The lease gate IS the beat: it checks the token is this run's current
    // lease and refreshes `last_seen` on the way through, exactly as it does
    // for an append.
    authorize_drive(&state, run_id, &headers)?;
    Ok(Json(
        json!({ "lapses_in_seconds": whole_seconds(state.client_lease_ttl()) }),
    ))
}

/// `GET /v1/client-runs/{id}/log`: the recorded envelopes, for cursor rebuild.
///
/// `?from_seq=<n>` returns only envelopes at or after `n`, so a resuming client
/// that already holds a prefix fetches just the tail. The read needs no drive
/// token (a second viewer may read), and it needs no lease either: a run whose
/// driver released it (see [`release`]), whose lease lapsed, or that this
/// process only knows from a log written before a restart is still a
/// client-driven run's log, and this is a read, not a step in driving it. So
/// the gate asks the same two questions [`open`] does for the same reason (see
/// [`log_is_client_driven`]): this process's lease registry, or, failing that,
/// the log's own `driven_by: client` marker on its `RunStarted`. A run neither
/// says is client-driven, because it is server-driven or unknown outright,
/// still answers `404 unknown_run`.
pub async fn get_log(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let mut log = state.store().read_log(run_id).await.map_err(store_error)?;
    if !state.is_client_run(run_id) && !log_is_client_driven(&log) {
        return Err(unknown_client_run(run_id));
    }
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
            append_tool_completion(&state, run_id, seq, &output, None).await?;
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
///
/// `deduplicated_from` names the completion this output was copied from, on the
/// one path that copies one (a repeated call under a declared idempotency key);
/// every other caller passes `None`, and the recorded bytes are then exactly
/// what this helper wrote before the field existed.
///
/// `settled_by` is never stamped here. This is the run recording what it was
/// told; only `Runtime::resolve`, where a person records a completion over the
/// run's head, names a settler.
///
/// # It settles the store's claim, when this call holds one
///
/// A call opened under a DECLARED idempotency key claimed that identity in the
/// store before its intent was written (see [`client_tool_intent`]). The
/// completion has to release it, in the same atomic step the event is appended,
/// or the store would go on saying the call is in flight while its result sits
/// recorded, and every later call under that key would be refused forever with
/// nothing anywhere to say why. This is the same reconciliation
/// `Runtime::resolve` performs for the hand-recorded path, and it is a no-op
/// for a positional key, which claims nothing.
async fn append_tool_completion(
    state: &AppState,
    run_id: RunId,
    seq: u64,
    output: &Value,
    deduplicated_from: Option<DedupOrigin>,
) -> Result<(), ApiError> {
    let completion = EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq + 1),
        state.now(),
        Event::ToolCallCompleted {
            seq: SequenceNumber::new(seq),
            output: output.clone(),
            deduplicated_from,
            settled_by: None,
            settled_caller: None,
        },
    );
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    let held = held_claim(state, &log, run_id, seq).await?;
    let mut validator = LogValidator::new(log);
    validator
        .push(completion.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;
    match held {
        Some(key) => state
            .store()
            .append_settling_call(
                &completion,
                CallClaimant {
                    tool: &key.0,
                    idempotency_key: &key.1,
                    run_id,
                    intent_seq: SequenceNumber::new(seq),
                },
            )
            .await
            .map_err(append_error),
        None => state
            .store()
            .append(&completion)
            .await
            .map_err(append_error),
    }
}

/// The `(tool, idempotency key)` this run's intent at `seq` holds an unsettled
/// claim on, if it holds one at all.
///
/// Mirrors the lookup `Runtime::resolve` does before it settles: a commitment
/// that names this exact run and this exact intent, and is not already settled,
/// is one this completion owns and must close. Anything else (no commitment,
/// somebody else's, one already settled) is left alone, because settling a
/// commitment one does not own is refused by the store and would be a bug here
/// rather than a race.
async fn held_claim(
    state: &AppState,
    log: &[EventEnvelope],
    run_id: RunId,
    seq: u64,
) -> Result<Option<(String, String)>, ApiError> {
    let Some(EventEnvelope {
        event:
            Event::ToolCallRequested {
                tool,
                idempotency_key: Some(key),
                ..
            },
        ..
    }) = log.get(seq as usize)
    else {
        return Ok(None);
    };
    let commitment = state
        .store()
        .lookup_call(tool, key)
        .await
        .map_err(store_error)?;
    let ours = commitment.is_some_and(|commitment| {
        commitment.run_id == run_id
            && commitment.intent_seq.get() == seq
            && commitment.completion_seq.is_none()
    });
    Ok(ours.then(|| (tool.clone(), key.clone())))
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
///
/// # The lease a resolve clears, and the one it does not
///
/// A dangling write means the driver that opened it never came back to record
/// what happened, so a resolve is normally the sign that that driver is gone
/// and the lease it left behind is holding the run for nobody. Both resolve
/// endpoints say so the same way (see
/// [`AppState::clear_client_lease`](crate::state::AppState::clear_client_lease)):
/// the run's lease is dropped once the resolution is recorded, and the next
/// open takes the run at once instead of waiting out the TTL. The operator's
/// path, `POST /v1/runs/{id}/resolve`, is where that matters, because the
/// caller there presents no token and is by definition not the driver.
///
/// This endpoint is the exception, and passes its own token to be kept. Getting
/// in here at all means presenting the run's current lease, which is the driver
/// saying it is right here; revoking it would strand the very caller that just
/// proved it is alive, mid-run, over a write it is about to carry on past.
pub async fn resolve(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let lease = authorize_drive(&state, run_id, &headers)?;
    let request: ResolveRequest = parse_body(&body)?;

    // The same declaration check the operator's resolve endpoint makes, through
    // the same helper, so a hand-recorded output meets one set of rules however
    // it arrives. Salvor witnessed neither the call nor the resolution, and the
    // declaration is the only thing that says what a finished call looks like.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    crate::client_tools::check_client_resolution(&state.client_tools(), &log, &request.output)?;

    match state.runtime().resolve(run_id, request.output).await {
        Ok(_) => {
            // The resolve rule, with this caller's own lease held back from
            // it. In the one case where the stored lease is no longer the one
            // authorized above (it lapsed while the completion was being
            // written and another driver took the run), this does clear that
            // driver's lease, which is the same outcome it would meet on its
            // next call anyway.
            state.clear_client_lease(run_id, Some(&lease.drive_token));
            Ok(Json(json!({
                "run": run_id.as_uuid().to_string(),
                "resolved": true,
            })))
        }
        Err(RuntimeError::NotReconcilable { status, .. }) => {
            // Always a client-driven run: getting in here meant presenting its
            // drive token.
            Err(resolve_refusal(run_id, &log, &status, true))
        }
        Err(error) => Err(ApiError::Internal(error.to_string())),
    }
}

/// The `409 wrong_state` a resolve answers when the run has no dangling write
/// to settle, shared by both resolve endpoints.
///
/// # Why a client-driven run gets its own sentence
///
/// The generic form quotes the runtime's status name, and two of those names
/// end in "use recover". `recover` is a server-driven verb: it spawns a driver
/// task over the run, which is exactly what must never happen to a run whose
/// client holds the single-writer lease, and `POST /v1/runs/{id}/resume`
/// refuses such a run for that reason. Telling an operator to reach for it is
/// telling them to do the one thing the next endpoint will refuse.
///
/// What a client-driven run's unfinished call actually needs is either nothing
/// or a resolve, and which one is a fact about the call, so the message says
/// it: an unfinished read or model call is re-performed by the client on its
/// next drive, with no operator action at all, while a dangling write is the
/// one thing this endpoint settles (and a run holding one never reaches this
/// refusal, because it folds to `needs_reconciliation` and the resolve
/// succeeds).
pub(crate) fn resolve_refusal(
    run_id: RunId,
    log: &[EventEnvelope],
    status: &str,
    client_driven: bool,
) -> ApiError {
    if client_driven && let Some(unfinished) = unfinished_call_sentence(log) {
        return ApiError::WrongState(format!(
            "run {} has no dangling write to settle: {unfinished}. A write recorded with no \
             completion after it is the one thing this endpoint records by hand",
            run_id.as_uuid()
        ));
    }
    ApiError::WrongState(format!(
        "run {} does not need reconciliation (status: {status}); there is no dangling write to \
         resolve",
        run_id.as_uuid()
    ))
}

/// How a client-driven run's unfinished call reads in a refusal, or `None` when
/// the log does not end at one (the run finished, parked, or never started, all
/// of which the status name already describes honestly).
fn unfinished_call_sentence(log: &[EventEnvelope]) -> Option<String> {
    match &log.last()?.event {
        Event::ToolCallRequested {
            seq, tool, effect, ..
        } if !matches!(effect, Effect::Write) => Some(format!(
            "its log ends at an unfinished {effect:?} call to `{tool}` at seq {}, which the client \
             performs again on its next drive rather than by being resolved",
            seq.get()
        )),
        Event::ModelCallRequested { seq, .. } => Some(format!(
            "its log ends at an unfinished model call at seq {}, which the client performs again \
             on its next drive rather than by being resolved",
            seq.get()
        )),
        _ => None,
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
///
/// # What the hash is over, and who chooses
///
/// The client never chooses, on either shape. What the OPERATOR chooses, in the
/// declaration's [`idempotency_key`](crate::client_tools::ClientToolDecl::idempotency_key),
/// is what the hash is over:
///
/// - **No fields declared: `{ run, seq, tool }`.** The call's position in the
///   run. This is an attempt identifier and promises exactly one thing: the
///   same position, retried, presents the same key. Two calls at two positions
///   are two calls, however alike their arguments.
/// - **Fields declared: `{ run, tool, <field>: <value>, ... }`.** The call's
///   content. `seq` is deliberately absent, which is the whole difference: the
///   same refund asked for twice in one run derives one key both times, so the
///   second is the same call rather than a second refund. Each value is the
///   intent's own recorded input, so the key is a fact about what was
///   authorized. The order the operator wrote the names in does not change the
///   key: `hash_value` canonicalizes, which sorts object keys, so a
///   reordered declaration derives the identical hash and a client deriving it
///   independently need not mirror the file's ordering.
///
/// The run id is on both shapes, so a declared key is an identity within one
/// run and never collides with another run's. That is deliberate. Only a tool
/// can honestly say two calls in different runs are the same effect, and a
/// declaration is the operator's word about a tool this server holds no code
/// for; scoping the identity to the run keeps the claim to something the
/// operator can actually know.
///
/// # Errors
///
/// [`ApiError::BadRequest`] naming the field when a declared key field is
/// absent from the input. The load-time rule makes every key field required by
/// the input schema, so an input that passed validation carries them all; this
/// is the check that the two rules stay in step rather than deriving a key over
/// a missing value and collapsing two calls onto one identity.
fn client_tool_idempotency_key(
    run_id: RunId,
    seq: u64,
    tool: &str,
    key_fields: &[String],
    input: &Value,
) -> Result<String, ApiError> {
    if key_fields.is_empty() {
        return Ok(hash_value(&json!({
            "run": run_id.as_uuid().to_string(),
            "seq": seq,
            "tool": tool,
        })));
    }
    let mut identity = serde_json::Map::new();
    identity.insert("run".to_owned(), json!(run_id.as_uuid().to_string()));
    identity.insert("tool".to_owned(), json!(tool));
    for field in key_fields {
        let value = input.get(field).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "tool `{tool}` derives its idempotency key from `{field}`, but the input carries \
                 no `{field}`; the key names what the call is, so it cannot be derived without it"
            ))
        })?;
        identity.insert(field.clone(), value.clone());
    }
    Ok(hash_value(&Value::Object(identity)))
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
    let key_fields = decl.idempotency_key.clone();

    let key = client_tool_idempotency_key(
        run_id,
        request.seq,
        &request.tool,
        &key_fields,
        &request.input,
    )?;
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
        // the position or of the input, never of anything this request could
        // vary independently, so an identical re-post re-derives the recorded
        // key and can simply be handed it back: the client retries its own call
        // under the same key and the provider collapses the duplicate. Compare
        // the events rather than the envelopes, because `recorded_at` is this
        // store's stamp from the first attempt and would never match a fresh one.
        let recorded = &log[request.seq as usize];
        if recorded.event == intent.event {
            let output = recorded_tool_output(&log, request.seq);
            return Ok(Json(intent_body(request.seq, &key, effect, output)));
        }
        return Err(ApiError::Divergence(format!(
            "seq {} already holds a different event; it is not this client-tool intent's position",
            request.seq
        )));
    }

    // The same append-guard the generic append and both server-performed steps
    // push through: it decides whether this is the legal next event. It runs
    // before the claim below, and the order matters: a claim is permanent and
    // nothing releases it, so claiming an identity for an intent that then
    // turns out to be illegal would strand that key forever over a call that
    // was never recorded.
    let mut validator = LogValidator::new(log);
    validator
        .push(intent.clone())
        .map_err(|error| ApiError::Divergence(error.to_string()))?;

    // A declared key is an identity, not an attempt number, so this is the
    // moment to find out whether the call it names has already happened. It
    // mirrors `RunCtx::tool_call` exactly: the store's claim is the arbiter,
    // asked live, before the write-ahead intent is persisted and before the
    // client is told it may perform anything.
    //
    // Only a Write or an Idempotent call asks. A Read has no effect worth an
    // identity, and answering a repeated read from an older call would quietly
    // freeze a loop that is polling for a change on purpose.
    let identity = (!key_fields.is_empty() && deduplicates(effect)).then_some(CallClaimant {
        tool: &request.tool,
        idempotency_key: &key,
        run_id,
        intent_seq: SequenceNumber::new(request.seq),
    });
    let mut copied = None;
    if let Some(claimant) = identity {
        match state
            .store()
            .claim_call(claimant)
            .await
            .map_err(store_error)?
        {
            // This position is the one execution of the call.
            CallClaim::Claimed => {}
            CallClaim::Held(commitment) if commitment.completion_seq.is_some() => {
                copied = Some(committed_output(&state, commitment).await?);
            }
            // Held by a call that has not finished. Within one run the
            // append-guard's one-pending-call rule refuses the second intent
            // before it ever gets here, so this is the guard for the case the
            // rule cannot see, and it refuses rather than guessing: nothing is
            // recorded, and the run is drivable again the moment the holder is
            // settled.
            CallClaim::Held(commitment) => {
                return Err(ApiError::Divergence(format!(
                    "the call `{}` names is already open at seq {} of run {} and has not \
                     finished; settle that call before opening the same one again",
                    request.tool,
                    commitment.intent_seq.get(),
                    commitment.run_id.as_uuid()
                )));
            }
        }
    }

    // Write-ahead on both paths. An intent that resolves as a duplicate is
    // still an honest record of what this run asked for, which is the rule
    // `RunCtx::tool_call` follows for the same case.
    state.store().append(&intent).await.map_err(append_error)?;

    if let Some((output, origin)) = copied {
        // The call already happened, so nothing performs it a second time: the
        // completion is written here, correlated to the intent just recorded
        // and naming what it copied.
        append_tool_completion(&state, run_id, request.seq, &output, Some(origin)).await?;
        return Ok(Json(intent_body(request.seq, &key, effect, Some(output))));
    }
    // A freshly-recorded intent can never already be settled: the append above
    // just placed it at the log's new end, with nothing after it yet.
    Ok(Json(intent_body(request.seq, &key, effect, None)))
}

/// Whether a call with this effect carries an identity worth deduplicating on,
/// the same rule `RunCtx::tool_call` applies to a tool's declared key.
fn deduplicates(effect: Effect) -> bool {
    matches!(effect, Effect::Write | Effect::Idempotent)
}

/// The recorded output of the completion a settled commitment points at, and
/// the origin to name on the copy.
///
/// The output is read back through `read_log`, so the origin run's hash chain
/// is verified before a single byte is copied, exactly as the runtime's own
/// deduplication reads it.
async fn committed_output(
    state: &AppState,
    commitment: salvor_store::CallCommitment,
) -> Result<(Value, DedupOrigin), ApiError> {
    let origin_log = state
        .store()
        .read_log(commitment.run_id)
        .await
        .map_err(store_error)?;
    let output = recorded_tool_output(&origin_log, commitment.intent_seq.get()).ok_or_else(|| {
        ApiError::Internal(format!(
            "the store says run {} settled this call at seq {}, but that log holds no completion \
             there",
            commitment.run_id.as_uuid(),
            commitment.intent_seq.get()
        ))
    })?;
    Ok((
        output,
        DedupOrigin {
            run_id: commitment.run_id,
            seq: commitment.intent_seq,
        },
    ))
}

/// The output recorded for the tool intent at `seq`, when its
/// `ToolCallCompleted` is already in `log`; `None` while the call is still
/// open.
///
/// The append-guard only ever admits a completion for the same `seq`
/// immediately after its intent (see [`append_tool_completion`]), so it is
/// enough to check the very next slot.
fn recorded_tool_output(log: &[EventEnvelope], seq: u64) -> Option<Value> {
    match &log.get(seq as usize + 1)?.event {
        Event::ToolCallCompleted {
            seq: completed_seq,
            output,
            ..
        } if completed_seq.get() == seq => Some(output.clone()),
        _ => None,
    }
}

/// The `200` client-tool-intent body: the position, the DERIVED idempotency key
/// the client must perform under, the operator-declared effect it was
/// recorded with, whether this position's completion is ALREADY recorded, and
/// that completion's output when it is.
///
/// `settled` exists for a caller re-posting an intent it already believes it
/// opened, most pointedly a payments caller checking a write before it acts on
/// the response: without it, a retried intent and a fresh one look identical
/// (same `200`, same key), and a caller cannot tell "safe to perform" from
/// "already done, do not perform it again" without separately reading the log.
///
/// The recorded `output` rides along on a settled answer, the same way
/// [`client_model_intent_body`] carries a recorded response, and for a sharper
/// reason here: a call answered from a DECLARED idempotency key is settled the
/// instant its intent is opened, without the client having performed anything,
/// so this response is the only place the client learns what the call it just
/// asked for returned. The key is omitted entirely while the call is open, so
/// an unsettled answer is byte for byte what it was before there was an output
/// to carry.
fn intent_body(seq: u64, idempotency_key: &str, effect: Effect, output: Option<Value>) -> Value {
    let mut body = json!({
        "seq": seq,
        "idempotency_key": idempotency_key,
        "effect": effect,
        "settled": output.is_some(),
    });
    if let Some(output) = output {
        body.as_object_mut()
            .expect("the intent body is a JSON object")
            .insert("output".to_owned(), output);
    }
    body
}

/// Which of the two shapes a client-tool completion arrived in, after the
/// either-or rule has been applied to the request body.
enum Reported {
    /// The call returned this.
    Output(Value),
    /// The call produced nothing and failed like this.
    Error(ReportedFailure),
}

/// Records a client-reported failure as the completion for the intent at `seq`.
///
/// The recorded output is the `__salvor_error` sentinel, built by
/// `salvor_runtime::wire`'s own [`encode_failure`], so the bytes are the ones
/// the runtime writes when a native tool exhausts its retries. That parity is
/// the whole point: a failure is not a new kind of event and not a new run
/// state, it is the outcome a completion is allowed to carry, and a log written
/// through this endpoint has to mean to a replay exactly what a natively
/// recorded one means.
///
/// # Errors
///
/// [`ApiError::BadRequest`] for a `kind` that is not one of the three recorded
/// layers, and whatever [`append_tool_completion`] reports.
async fn record_reported_failure(
    state: &AppState,
    run_id: RunId,
    seq: u64,
    failure: ReportedFailure,
) -> Result<Json<Value>, ApiError> {
    let kind = match failure.kind.as_deref() {
        None => ToolFailureKind::Handler,
        Some(named) => ToolFailureKind::from_wire(named).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "`{named}` is not a failure kind; use `invalid_input`, `handler`, or \
                 `output_serialization`, or omit it for `handler`"
            ))
        })?,
    };
    let output = encode_failure(&ToolFailure {
        kind,
        message: failure.message,
        // One attempt. `attempts` counts executions inside salvor's own retry
        // loop, and salvor ran no loop over a call it did not dispatch, so a
        // number taken from the wire would be the client describing machinery
        // that never touched its call.
        attempts: 1,
    });
    append_tool_completion(state, run_id, seq, &output, None).await?;
    Ok(Json(json!({ "seq": seq, "completed": true })))
}

/// `POST /v1/client-runs/{id}/client-tool-completion`: record that a
/// client-performed tool call finished.
///
/// The client ran the call in its own process and is now reporting what
/// happened. Salvor did not witness it, so everything this endpoint can check,
/// it checks before the report becomes history. Requires the `X-Drive-Token`
/// header.
///
/// # Two shapes, and exactly one of them
///
/// The body carries `output`, what the call returned, or `error`, what went
/// wrong instead when it returned nothing at all. Both, or neither, is a `400`:
/// they say opposite things about the same call and this server has no way to
/// pick between them.
///
/// The `error` shape records the same `__salvor_error` sentinel completion the
/// runtime records when a NATIVE tool exhausts its retries, through
/// `salvor_runtime::wire`'s own encoder, so the bytes match (see
/// [`record_reported_failure`]). A failure is not a new event and not a new run
/// state: it is an outcome a completion is allowed to carry, so a recorded
/// failure SETTLES the call exactly as a native one does, and the run carries on
/// with the failure replaying from the log rather than the call happening again.
///
/// It refuses, recording nothing, when:
///
/// - the body carries both `output` and `error`, or neither (`400`);
/// - the log does not end at a tool intent, or ends at one whose `seq` is not
///   the one this request names (`409 divergence`);
/// - the pending intent was performed by the SERVER (`403`): a client must not
///   close a call salvor made, since salvor holds the real result;
/// - the declaration says `trust_completion = false` (`403`), for a reported
///   failure as much as for a reported result. "It did not land" is a claim
///   about money made by the party that benefits from it being believed, so an
///   untrusted write is left dangling for a person either way;
/// - the declaration carries no `output_schema` AND the body reports an output
///   (`403`): with nothing to check the report against, the completion is
///   unfalsifiable, which is exactly what the schema exists to prevent. A
///   reported failure carries no value to check and is unaffected;
/// - the reported output fails the declared `output_schema` (`400`);
/// - a `require_equal` field's reported value differs from the value the intent
///   recorded (`403`): the output schema is a shape check and cannot know what
///   was authorized, so a client report may not alter a pinned field;
/// - a reported `kind` names no recorded failure layer (`400`).
///
/// The checks run in that order: the either-or rule first, then correlation,
/// then the trust refusal before any value is compared, then the output shape,
/// then the per-field equality. The `output_schema`, `require_equal`, and
/// value-shape checks are skipped on the `error` path, which is the absence of a
/// value for them to look at rather than a relaxation of the rules.
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
    let reported = match (request.output, request.error) {
        (Some(output), None) => Reported::Output(output),
        (None, Some(error)) => Reported::Error(error),
        (Some(_), Some(_)) => {
            return Err(ApiError::BadRequest(
                "a completion carries `output` or `error`, never both: they say opposite things \
                 about the same call"
                    .to_owned(),
            ));
        }
        (None, None) => {
            return Err(ApiError::BadRequest(
                "a completion must carry `output` (what the call returned) or `error` (what went \
                 wrong instead)"
                    .to_owned(),
            ));
        }
    };

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
             POST /v1/runs/{}/resolve or `salvor resolve`",
            run_id.as_uuid()
        )));
    }
    // A reported failure stops here, on the checks that are about trust rather
    // than about a value. There is no output to hold against the declared
    // shape, and no field to pin to what was authorized, so the two remaining
    // guards have nothing to say: skipping them is the absence of a value, not
    // a relaxation of the rules. What IS recorded is byte for byte what the
    // runtime records when a native tool exhausts its retries, so a log replays
    // identically whichever side the call was performed on.
    let output = match reported {
        Reported::Output(output) => output,
        Reported::Error(failure) => {
            return record_reported_failure(&state, run_id, request.seq, failure).await;
        }
    };

    let Some(output_schema) = &decl.output_schema else {
        return Err(ApiError::ClientCompletionRefused(format!(
            "tool `{tool}` declares no output_schema, so a client-reported completion carries \
             nothing this server can check; declare an output_schema for it, or settle the call \
             by hand with POST /v1/runs/{}/resolve or `salvor resolve`",
            run_id.as_uuid()
        )));
    };
    validate_against_schema(&output, output_schema).map_err(|error| {
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
        let claimed = output.get(field).unwrap_or(&Value::Null);
        if authorized != claimed {
            return Err(ApiError::ClientCompletionRefused(format!(
                "tool `{tool}` reported `{field}` as {claimed} for the intent at seq {}, but the \
                 intent recorded {authorized}; a client report may not alter a require_equal field. \
                 If the provider genuinely did something different, settle it by hand with POST \
                 /v1/runs/{}/resolve or `salvor resolve`",
                request.seq,
                run_id.as_uuid()
            )));
        }
    }

    // The completion goes through the same guard and the same helper the
    // server-performed tool step records its own completion with, so the two
    // surfaces write byte-identical `ToolCallCompleted` events.
    append_tool_completion(&state, run_id, request.seq, &output, None).await?;
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
        Some(token) if !tokens::secrets_equal(token, &lease.drive_token) => {
            Err(ApiError::InvalidDriveToken(format!(
                "the presented drive token is not the current lease for run {}",
                run_id.as_uuid()
            )))
        }
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

/// Whether a run's log says it is over, so no driver could still be working on
/// it and a re-open may take it regardless of any lease left behind.
///
/// This asks the recorded log, not the lease, because a driver that completed a
/// run and then vanished leaves a lease that is still current for the rest of
/// the TTL. Refusing a re-open on that would make the last minute of every
/// finished run needlessly unopenable, and there is nothing to protect: a
/// finished run takes no more appends from anyone.
fn run_is_finished(log: &[EventEnvelope]) -> bool {
    matches!(
        derive_state(log).status,
        RunStatus::Completed { .. } | RunStatus::Failed { .. } | RunStatus::Abandoned { .. }
    )
}

/// The refusal for a re-open of a run whose driver still holds a current lease,
/// naming how long the hold has left, rounded up to whole seconds by
/// [`whole_seconds`] so the number is always a time at which retrying works.
fn lease_held(run_id: RunId, remaining: Duration) -> ApiError {
    let lapses_in_seconds = whole_seconds(remaining);
    ApiError::LeaseHeld {
        message: format!(
            "another driver holds run {}; its lease lapses in {lapses_in_seconds}s if that \
             driver goes quiet, and re-opening works then (or as soon as the run finishes)",
            run_id.as_uuid()
        ),
        lapses_in_seconds,
    }
}

/// A lease duration as the whole seconds the wire carries, for the `lease_held`
/// refusal and the heartbeat answer alike.
///
/// Rounded UP, and never below 1. Rounding down would let a hold with a
/// fraction of a second left report `0`, and a caller reading that as "try
/// again now" would come straight back into the same refusal; rounding up means
/// the number is always a time at which retrying can actually work. The same
/// reasoning covers a heartbeat interval: a driver told `0` would beat in a
/// tight loop.
fn whole_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos().div_ceil(1_000_000_000))
        .unwrap_or(i64::MAX)
        .max(1)
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
