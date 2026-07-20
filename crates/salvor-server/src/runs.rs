//! The run endpoints and the task that drives a run server-side.
//!
//! # How a run is driven, and why it survives the request
//!
//! Starting or resuming a run means model calls and tool calls: long, and not
//! something to hold an HTTP request open for. So the handlers do the fast,
//! synchronous part (validate, refuse a bad state, mint or check the run id)
//! and then hand the run to a spawned task that drives it to its next resting
//! point. The handler returns immediately with the run id.
//!
//! The run outliving its request is the point, not a side effect. Every event
//! is persisted to the store the instant it happens, inside the driving task,
//! before the task moves on. The task holds no state the store does not
//! already have. So aborting the task, or dropping the whole server, mid-run
//! loses nothing: a fresh server over the same store recovers the run from its
//! log and continues it, re-executing no completed model or tool call. That is
//! the same durability guarantee the CLI has, over HTTP.
//!
//! # Resume, recover, resolve: the same dispatch as the CLI
//!
//! The resume endpoint reads the run's derived state and dispatches with the
//! shared [`crate::dispatch::classify`], exactly as `salvor resume` does: a
//! parked run resumes with a validated input, a crashed run recovers with
//! none, a run needing reconciliation is refused with its recorded intent as
//! evidence (`409`), and a finished run is reported. The resolve endpoint is
//! the operator override for that refusal: it records the completion of a
//! dangling write by hand.
//!
//! Resume and recover rebuild the agent from the definition registered under
//! the run's recorded `agent_def_hash`. The registry is in-process, so after a
//! restart the definition is re-registered first; its hash is stable, so the
//! run's recorded reference still resolves.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use salvor_core::{Event, EventEnvelope, PendingCall, RunId, RunStatus, derive_state};
use salvor_runtime::{
    RuntimeError, validate_against_schema, validate_extension_input, validate_labels,
};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::dispatch::{Disposition, ResumeKind, classify};
use crate::error::ApiError;
use crate::json;
use crate::state::{AppState, BuiltAgent};

/// The body of `POST /v1/runs`.
#[derive(Debug, Deserialize)]
struct StartRequest {
    /// The registered agent id (its `agent_def_hash`).
    agent: String,
    /// The run input. Defaults to JSON null when omitted.
    #[serde(default)]
    input: Value,
    /// An optional caller-chosen run id (a UUID). Minted when omitted.
    #[serde(default)]
    run_id: Option<String>,
    /// Optional correlation tags for the run (a build id, an environment),
    /// recorded once on `RunStarted`. Checked against the sanity bounds (see
    /// [`validate_labels`]) before the run is spawned, so a caller sees a
    /// synchronous `400` rather than a background task failure.
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

/// The body of `POST /v1/runs/{id}/resume`.
#[derive(Debug, Default, Deserialize)]
struct ResumeRequest {
    /// The resume input, required for a parked run, ignored when recovering.
    #[serde(default)]
    input: Option<Value>,
}

/// The body of `POST /v1/runs/{id}/resolve`.
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    /// The output to record for the dangling write, verbatim.
    output: Value,
}

/// Which verb a driver task runs.
enum DriveVerb {
    Start(Value, Option<BTreeMap<String, String>>),
    Resume(Value),
    Recover,
}

/// `POST /v1/runs`: start a fresh run and return its id at once.
pub async fn start(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let request: StartRequest = parse_body(&body)?;
    if let Some(labels) = &request.labels {
        validate_labels(labels).map_err(ApiError::BadRequest)?;
    }
    let registered = state.agent(&request.agent).ok_or_else(|| {
        ApiError::UnknownAgent(format!("no agent registered under `{}`", request.agent))
    })?;

    let built = state
        .build_agent(registered.definition)
        .await
        .map_err(ApiError::BadRequest)?;

    let run_id = match &request.run_id {
        Some(text) => parse_run_id(text)?,
        None => RunId::new(),
    };

    // A run that already has history is not startable; close the sessions the
    // build just opened before refusing.
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if !log.is_empty() {
        close_servers(built.servers).await;
        return Err(ApiError::RunExists(format!(
            "run {} already has recorded history; resume or recover it instead",
            run_id.as_uuid()
        )));
    }

    spawn_drive(
        state,
        run_id,
        built,
        DriveVerb::Start(request.input, request.labels),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "run": run_id.as_uuid().to_string(), "status": "running" })),
    ))
}

/// `GET /v1/runs`: one entry per run with its folded status, plus per-run
/// `usage` totals, `step_count`, `agent_def_hash`, `labels`, and `driver` —
/// additive fields folded from the SAME log read and the SAME [`derive_state`]
/// call this handler has always run for `status`. No second read of the log and
/// no second fold pass: `usage` comes straight off the already-derived
/// [`RunState`](salvor_core::RunState), and `step_count`/`agent_def_hash`/
/// `labels` are read off the same in-memory `log` slice already in hand.
///
/// `driver` is the run's liveness evidence — `"attached"` when a driver is
/// currently running it (a live server task or a current client lease),
/// `"none"` when none is, and omitted entirely for a terminal run — see
/// [`driver_evidence`]. It reads no log: it consults only the process's own
/// driving-run set and client-run leases, the truth the server already holds.
/// The newest envelope's `last_recorded_at` (already carried, unchanged) is the
/// companion "when did anything last happen" evidence; the dashboard reads the
/// two together to derive a `stalled` verdict for a `running` run with no
/// driver that has gone quiet.
///
/// `run`, `status`, `event_count`, `first_recorded_at`, and
/// `last_recorded_at` keep their exact pre-existing shape and values; every
/// consumer reading only those fields sees byte-identical JSON to before
/// this change.
///
/// # The zero-vs-absent rule
///
/// A count is reported as a real number whenever it is genuinely known, and
/// omitted (never `0`) whenever it is not, because `0` is a claim ("this run
/// made no model calls") that would be a lie if the truth is merely
/// "unknown":
///
/// - **The log folds.** `usage` and `step_count` are always present and are
///   real counts — a run with no model calls truthfully folds to
///   `step_count: 0` and `usage: {"input_tokens": 0, "output_tokens": 0}`.
///   `agent_def_hash` is read off the run's `RunStarted` event, which is
///   recorded first on every started run by construction, so it is present
///   in practice; it stays `Option` (omitted, not defaulted, if ever
///   missing) as a defensive honesty measure rather than a guarantee to
///   invent a value for. `labels` is the same `Option` treatment one step
///   further: absent whenever the run recorded none (an unlabeled run, or an
///   old run from before labels existed), and *also* absent when the
///   recorded value is an explicit empty map — the API never emits `"labels":
///   {}`, because an empty map is not a fact worth claiming any more than an
///   unknown count is. A run that recorded at least one label reports exactly
///   what was recorded, and only that.
/// - **The log cannot be read.** `store.read_log` can fail for one run's row
///   alone (a corrupt or unreadable envelope: [`StoreError::Serialization`]
///   or [`StoreError::Backend`]) without implicating any other run's row.
///   [`EventStore::list_runs`]'s summary (`event_count`,
///   `first_recorded_at`, `last_recorded_at`) is a cheap SQL aggregate that
///   never parses the row's JSON payload, so it stays available even when
///   the payload itself does not. So an unreadable log degrades only that
///   one run's entry rather than failing the whole list: `status`,
///   `usage`, `step_count`, `agent_def_hash`, and `labels` are all omitted for
///   it (every one of them needs the fold, which needs the log this run's row
///   could not produce), while `run`, `event_count`, `first_recorded_at`,
///   and `last_recorded_at` stay present, since they never depended on that
///   read. This is strictly additive: before this change such a run took
///   the whole request down (a `500`), so no existing consumer ever
///   observed — or could have pinned — a shape for this case.
///
/// [`StoreError::Serialization`]: salvor_store::StoreError::Serialization
/// [`StoreError::Backend`]: salvor_store::StoreError::Backend
/// [`EventStore::list_runs`]: salvor_store::EventStore::list_runs
pub async fn list(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let store = state.store();
    let summaries = store.list_runs().await.map_err(store_error)?;
    let mut runs = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let mut entry = json!({
            "run": summary.run_id.as_uuid().to_string(),
            "event_count": summary.event_count,
            "first_recorded_at": rfc3339(summary.first_recorded_at),
            "last_recorded_at": rfc3339(summary.last_recorded_at),
        });
        match store.read_log(summary.run_id).await {
            Ok(log) => {
                let derived = derive_state(&log);
                let step_count = log
                    .iter()
                    .filter(|envelope| matches!(envelope.event, Event::ModelCallRequested { .. }))
                    .count();
                let map = entry.as_object_mut().expect("entry is a JSON object");
                map.insert("status".to_owned(), json::status(&derived.status));
                map.insert(
                    "usage".to_owned(),
                    json!({
                        "input_tokens": derived.usage.input_tokens,
                        "output_tokens": derived.usage.output_tokens,
                    }),
                );
                map.insert("step_count".to_owned(), json!(step_count));
                if let Some(hash) = recorded_agent_hash(&log) {
                    map.insert("agent_def_hash".to_owned(), json!(hash));
                }
                if let Some(labels) = recorded_labels(&log) {
                    map.insert("labels".to_owned(), json!(labels));
                }
                if let Some(driver) = driver_evidence(&state, summary.run_id, &derived.status) {
                    map.insert("driver".to_owned(), json!(driver));
                }
            }
            Err(error) => {
                // Scoped to this one run: the summary above already read fine,
                // so the list still reports it, just without the fields that
                // needed this read. See the zero-vs-absent rule above.
                tracing::warn!(
                    run = %summary.run_id.as_uuid(),
                    %error,
                    "list: run's log could not be read; status and folded fields omitted for it"
                );
            }
        }
        runs.push(entry);
    }
    Ok(Json(json!({ "runs": runs })))
}

/// `GET /v1/runs/{id}`: the run's folded status, usage, and pending intent.
pub async fn get(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if log.is_empty() {
        // A run whose start task has not yet written its first event still
        // exists; report it as running rather than unknown.
        if state.is_run_active(run_id) {
            return Ok(Json(json!({
                "run": run_id.as_uuid().to_string(),
                "status": { "state": "running" },
                "event_count": 0,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
                "pending": Value::Null,
                // A driver task is running this run in this process, by the very
                // condition of this branch; report that evidence.
                "driver": "attached",
            })));
        }
        return Err(unknown_run(run_id));
    }
    let derived = derive_state(&log);
    let mut body = json!({
        "run": run_id.as_uuid().to_string(),
        "status": json::status(&derived.status),
        "event_count": log.len(),
        "usage": {
            "input_tokens": derived.usage.input_tokens,
            "output_tokens": derived.usage.output_tokens,
        },
        "pending": json::pending(derived.pending_call.as_ref()),
        "first_recorded_at": rfc3339(log[0].recorded_at),
        "last_recorded_at": rfc3339(log[log.len() - 1].recorded_at),
    });
    if let Some(driver) = driver_evidence(&state, run_id, &derived.status) {
        body["driver"] = json!(driver);
    }
    Ok(Json(body))
}

/// `GET /v1/runs/{id}/replay`: the dry-run replay projection, executing
/// nothing. This is the full derived [`RunState`](salvor_core::RunState) as a
/// pure fold of the recorded log.
pub async fn replay(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if log.is_empty() {
        return Err(unknown_run(run_id));
    }
    Ok(Json(json::run_state(&derive_state(&log))))
}

/// `POST /v1/runs/{id}/resume`: continue a run, dispatching on its state.
pub async fn resume(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let request: ResumeRequest = parse_body_or_default(&body)?;

    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if log.is_empty() {
        return Err(unknown_run(run_id));
    }
    let derived = derive_state(&log);

    match classify(&derived) {
        Disposition::Completed(output) => Ok(Json(json!({
            "run": run_id.as_uuid().to_string(),
            "outcome": "completed",
            "status": { "state": "completed", "output": output },
        }))
        .into_response()),
        Disposition::Failed(error) => Ok(Json(json!({
            "run": run_id.as_uuid().to_string(),
            "outcome": "failed",
            "status": { "state": "failed", "error": error },
        }))
        .into_response()),
        Disposition::NotStarted => Err(unknown_run(run_id)),
        Disposition::Reconcile(pending) => Err(ApiError::NeedsReconciliation {
            message: format!(
                "run {} needs reconciliation: a write was recorded but never completed, so it \
                 may or may not have taken effect. Verify externally, then resolve it",
                run_id.as_uuid()
            ),
            intent: reconcile_intent(&log, &pending),
        }),
        Disposition::Resume(kind) => {
            let input = request.input.ok_or_else(|| {
                ApiError::BadRequest(
                    "this run is parked awaiting input; send a body of {\"input\": <json>}"
                        .to_owned(),
                )
            })?;
            // Validate up front, with the same validators the runtime uses, so
            // a bad input is a synchronous 400 rather than a silent no-op in
            // the driver task.
            match kind {
                ResumeKind::Suspension => {
                    if let RunStatus::Suspended { input_schema, .. } = &derived.status {
                        validate_against_schema(&input, input_schema)
                            .map_err(ApiError::BadRequest)?;
                    }
                }
                ResumeKind::Budget => {
                    validate_extension_input(&input).map_err(ApiError::BadRequest)?;
                }
            }
            // A graph run is an ordinary run with a richer log: the classify,
            // the input validation above, and the request/response contract are
            // all shared. Only the re-drive differs — a graph run continues over
            // the engine, resolving its document by the hash the log records,
            // where an agent run rebuilds its agent and continues the built-in
            // loop. This is the sole graph-specific branch the resume path needs.
            if crate::graph::is_graph_run(&log) {
                return crate::graph::drive_resume(state, run_id, &log, Some(input)).await;
            }
            let built = rebuild_agent(&state, &log).await?;
            spawn_drive(state, run_id, built, DriveVerb::Resume(input));
            Ok(driving(run_id).into_response())
        }
        Disposition::Recover => {
            if request.input.is_some() {
                tracing::warn!(
                    run_id = %run_id.as_uuid(),
                    "this run crashed mid-step; the resume input is ignored when recovering"
                );
            }
            // The graph branch, mirroring the Resume arm: a crashed graph run
            // recovers over the engine (no resume input), an agent run over the
            // built-in loop.
            if crate::graph::is_graph_run(&log) {
                return crate::graph::drive_resume(state, run_id, &log, None).await;
            }
            let built = rebuild_agent(&state, &log).await?;
            spawn_drive(state, run_id, built, DriveVerb::Recover);
            Ok(driving(run_id).into_response())
        }
    }
}

/// `POST /v1/runs/{id}/resolve`: record a dangling write's completion by hand.
pub async fn resolve(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = parse_run_id(&run_id_text)?;
    let request: ResolveRequest = parse_body(&body)?;

    let log = state.store().read_log(run_id).await.map_err(store_error)?;
    if log.is_empty() {
        return Err(unknown_run(run_id));
    }

    // resolve records exactly one completion and drives nothing, so it runs
    // inline rather than in a task.
    match state.runtime().resolve(run_id, request.output).await {
        Ok(_) => {
            let log = state.store().read_log(run_id).await.map_err(store_error)?;
            let derived = derive_state(&log);
            Ok(Json(json!({
                "run": run_id.as_uuid().to_string(),
                "resolved": true,
                "status": json::status(&derived.status),
            })))
        }
        Err(RuntimeError::NotReconcilable { status, .. }) => Err(ApiError::WrongState(format!(
            "run {} does not need reconciliation (status: {status}); there is no dangling write \
             to resolve",
            run_id.as_uuid()
        ))),
        Err(error) => Err(ApiError::Internal(error.to_string())),
    }
}

/// Spawns the task that drives a run to its next resting point, then closes its
/// MCP sessions. Marks the run active before spawning so a concurrent stream
/// cannot miss it.
fn spawn_drive(state: AppState, run_id: RunId, built: BuiltAgent, verb: DriveVerb) {
    state.begin_run(run_id);
    let task_state = state.clone();
    let handle = tokio::spawn(async move {
        let BuiltAgent { agent, servers } = built;
        // The agent carries the resolved prompt-recording flag (per-agent
        // config over SALVOR_RECORD_PROMPTS over off), computed by the factory
        // when it built the agent; pass it to the runtime driving this run.
        let mut runtime = task_state
            .runtime()
            .with_record_prompts(agent.record_prompts());
        let result = match verb {
            DriveVerb::Start(input, labels) => {
                // The caller's labels win over any static labels on the
                // registered agent definition: they name this run, not the
                // agent it runs under. Bounds were already checked
                // synchronously in `start`, before this task was spawned.
                if let Some(labels) = labels {
                    runtime = runtime.with_labels(labels);
                } else if let Some(labels) = agent.labels() {
                    runtime = runtime.with_labels(labels.clone());
                }
                runtime.start_with_id(&agent, run_id, input).await
            }
            DriveVerb::Resume(input) => runtime.resume(&agent, run_id, input).await,
            DriveVerb::Recover => runtime.recover(&agent, run_id).await,
        };
        close_servers(servers).await;
        if let Err(error) = result {
            tracing::error!(run_id = %run_id.as_uuid(), %error, "run drive ended with an error");
        }
        task_state.end_run(run_id);
    });
    state.set_handle(run_id, handle);
}

/// Rebuilds the agent a run started under, from the definition registered
/// under its recorded `agent_def_hash`.
async fn rebuild_agent(state: &AppState, log: &[EventEnvelope]) -> Result<BuiltAgent, ApiError> {
    let hash = recorded_agent_hash(log)
        .ok_or_else(|| ApiError::Internal("run log has no RunStarted event".to_owned()))?;
    let registered = state.agent(&hash).ok_or_else(|| {
        ApiError::UnknownAgent(format!(
            "the agent `{hash}` this run started under is not registered on this server; register \
             its definition, then resume"
        ))
    })?;
    state
        .build_agent(registered.definition)
        .await
        .map_err(ApiError::BadRequest)
}

/// The liveness evidence for a run: whether a driver is currently attached to
/// it, or none is. The server reports this evidence; the dashboard derives the
/// `stalled` verdict from it (a `running` run with no driver, gone stale) — the
/// same division of labor `status` itself has, where the server folds the log
/// and the client reads the fold.
///
/// # What "attached" means, from evidence the server already holds
///
/// - A **server-driven** run is attached exactly when a driver task is still
///   running it in this process ([`AppState::is_run_active`]) — the same fact
///   the event stream's `detached` end-frame already reports (see `sse.rs`).
///   The task is removed the instant it ends (completes, parks, or errors), so
///   `is_run_active` is exact, not a heuristic.
/// - A **client-driven** run is attached exactly when this process holds a
///   current lease for it ([`AppState::client_run_driver_live`]): the driver
///   presented its drive token within the lease TTL. A lapsed lease (the tab
///   closed, the SDK exited) is not attached.
///
/// # Zero-vs-absent: a terminal run carries no driver field at all
///
/// A `completed` or `failed` run is done; asking whether a driver is attached
/// to it is not a meaningful question, so the field is omitted entirely (this
/// returns `None`), never `"driver": "none"`. That mirrors the house rule the
/// enriched list already follows for `usage`/`step_count`/`labels`: report a
/// fact when there is one, omit rather than assert a placeholder when there is
/// not. Every non-terminal run reports a real `"attached"` or `"none"`.
fn driver_evidence(state: &AppState, run_id: RunId, status: &RunStatus) -> Option<&'static str> {
    if matches!(
        status,
        RunStatus::Completed { .. } | RunStatus::Failed { .. }
    ) {
        return None;
    }
    if state.is_run_active(run_id) || state.client_run_driver_live(run_id) {
        Some("attached")
    } else {
        Some("none")
    }
}

/// The `agent_def_hash` recorded in a run's `RunStarted` event.
fn recorded_agent_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
        _ => None,
    })
}

/// The labels recorded in a run's `RunStarted` event, when present and
/// non-empty. Read off the same `RunStarted` `recorded_agent_hash` reads, off
/// the same in-memory log; no second read or second fold.
///
/// # The zero-vs-absent rule, extended to labels
///
/// `None` covers two cases the caller cannot (and need not) tell apart from
/// this function alone: a run that recorded no `labels` field at all (an old
/// or unlabeled run), and a run that recorded `labels: {}` explicitly. Both
/// report as "no labels" on the wire, never `"labels": {}`: the API makes no
/// empty-map claim, exactly as it never claims a zero it cannot back with a
/// real count. A run that recorded at least one label reports exactly what
/// was recorded.
fn recorded_labels(log: &[EventEnvelope]) -> Option<BTreeMap<String, String>> {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::RunStarted { labels, .. } => Some(labels.clone()),
            _ => None,
        })
        .flatten()
        .filter(|labels: &BTreeMap<String, String>| !labels.is_empty())
}

/// The reconciliation evidence: the recorded write intent, plus when it was
/// recorded, mirroring the CLI's refusal report.
fn reconcile_intent(log: &[EventEnvelope], pending: &PendingCall) -> Value {
    let mut intent = json::pending(Some(pending));
    if let PendingCall::Tool { seq, .. } = pending
        && let Some(envelope) = log.iter().find(|envelope| envelope.seq == *seq)
    {
        intent["recorded_at"] = json!(rfc3339(envelope.recorded_at));
    }
    intent
}

/// The 202 body for a run that is now driving in the background.
fn driving(run_id: RunId) -> impl IntoResponse {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "run": run_id.as_uuid().to_string(),
            "status": "running",
            "outcome": "driving",
        })),
    )
}

/// Closes every MCP session, logging (not propagating) a teardown hiccup.
async fn close_servers(servers: Vec<salvor_tools::mcp::McpServer>) {
    for server in servers {
        if let Err(error) = server.close().await {
            tracing::warn!(%error, "MCP session did not close cleanly");
        }
    }
}

/// Parses a JSON body into `T`, mapping a decode failure to a `400`.
fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|error| ApiError::BadRequest(format!("request body is not valid JSON: {error}")))
}

/// Parses a JSON body into `T`, treating an empty body as `{}` so an
/// input-optional request may be sent with no body at all.
fn parse_body_or_default<T: Default + for<'de> Deserialize<'de>>(
    body: &Bytes,
) -> Result<T, ApiError> {
    if body.is_empty() {
        return Ok(T::default());
    }
    parse_body(body)
}

/// Parses a run id from its UUID string, mapping a bad id to a `400`.
fn parse_run_id(text: &str) -> Result<RunId, ApiError> {
    Uuid::parse_str(text).map(RunId::from_uuid).map_err(|_| {
        ApiError::BadRequest(format!("`{text}` is not a valid run id (expected a UUID)"))
    })
}

/// The standard unknown-run error for a run id with no history.
fn unknown_run(run_id: RunId) -> ApiError {
    ApiError::UnknownRun(format!("no run {} in this store", run_id.as_uuid()))
}

/// Maps a store error to a `500`; the store failing is not the client's fault.
fn store_error(error: salvor_store::StoreError) -> ApiError {
    ApiError::Internal(format!("store: {error}"))
}

/// Formats a timestamp as RFC 3339, the same wire form the store's summaries
/// use.
fn rfc3339(ts: OffsetDateTime) -> String {
    ts.format(&Rfc3339).unwrap_or_default()
}
