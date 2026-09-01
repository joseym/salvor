//! `GET /v1/runs/{id}/events`: the event stream. This is the control plane's
//! headline feature, so its framing and its cursor are spelled out here.
//!
//! # Framing
//!
//! The response is `text/event-stream`. Every recorded event becomes one
//! server-sent-event frame:
//!
//! ```text
//! id: <seq>
//! data: <the pinned EventEnvelope JSON, on one line>
//!
//! ```
//!
//! The `data` line is exactly the envelope wire JSON the store holds, the same
//! bytes `salvor history --json` prints, so a client decodes stream frames and
//! log rows with one parser. The frame's `id` is the event's sequence number.
//! Envelope frames carry no `event:` field, so a browser `EventSource` receives
//! them through `onmessage`. When the run reaches a resting point (completed,
//! failed, abandoned, suspended, sleeping, budget-exceeded, or
//! needs-reconciliation) the stream sends one final `event: end` frame carrying
//! the status it rested at, then closes. A sleeping run's frame carries its
//! `wake_at`, so a client learns when the run may continue and opens a fresh
//! stream then rather than holding this one open for the length of the nap.
//!
//! # Replay then live tail
//!
//! On connect the server reads the run's whole log and sends every event at or
//! after the cursor, then polls the store for new events and sends them as they
//! land, until the resting frame. A run's log is append-only with contiguous,
//! ascending sequence numbers, so tracking one "next sequence to send" number
//! makes the stream gap-free and duplicate-free by construction.
//!
//! # The cursor: resuming a dropped stream
//!
//! A dropped connection resumes without gaps or duplicates in one of two ways:
//!
//! - **`Last-Event-ID`.** A browser `EventSource` resends the last `id` it saw
//!   as the `Last-Event-ID` header on reconnect. The server resumes from that
//!   sequence plus one, so the first event not yet seen is the first replayed.
//! - **`?from_seq=<n>`.** A non-browser client that tracks its own position
//!   asks for events from sequence `n` onward. Used when there is no
//!   `Last-Event-ID` to lean on.
//!
//! `Last-Event-ID` wins when both are present. With neither, the stream starts
//! at sequence 0, a full replay.
//!
//! # Revocation ends an open stream
//!
//! Auth checks a request once, on the way in, and a stream can outlive that
//! check by hours. So the stream re-checks: the handler captures the caller's
//! [`StreamCredential`] (a name and the bearer's SHA-256, never the bearer)
//! and the producer verifies it at the top of every poll pass, before it reads
//! the log. The cadence is therefore the stream poll interval,
//! [`AppState::poll_interval`], 50ms by default, and a revoked token ends its
//! streams within one pass. The check is a digest comparison over a token
//! file the auth layer already stats once per request, and a pass already
//! reads the whole log, so it is not what the pass costs.
//!
//! A credential that no longer verifies ends the stream the way every other
//! ending works, with a final `event: end` frame. That frame carries a
//! `reason` of `unauthorized` and an `error` saying to re-authenticate and
//! open a fresh stream. The run is untouched: it goes on being driven, and a
//! stream opened with a token that verifies picks its events up from the
//! cursor.
//!
//! A pass-through server, with no bearer configured, captures nothing and
//! checks nothing, and streams exactly as it did.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use salvor_core::{RunId, RunStatus, derive_state};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::auth::StreamCredential;
use crate::error::ApiError;
use crate::json;
use crate::state::AppState;

/// The `?from_seq=` cursor query.
#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// Send events from this sequence number onward. Overridden by a
    /// `Last-Event-ID` header when one is present.
    #[serde(default)]
    from_seq: Option<u64>,
}

/// The event-stream handler. See the module docs for the framing and cursor.
pub async fn stream(
    State(state): State<AppState>,
    Path(run_id_text): Path<String>,
    Query(params): Query<StreamParams>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let run_id = Uuid::parse_str(&run_id_text)
        .map(RunId::from_uuid)
        .map_err(|_| {
            ApiError::BadRequest(format!(
                "`{run_id_text}` is not a valid run id (expected a UUID)"
            ))
        })?;

    // A run that neither has history nor is being driven here does not exist.
    let log = state
        .store()
        .read_log(run_id)
        .await
        .map_err(|error| ApiError::Internal(format!("store: {error}")))?;
    if log.is_empty() && !state.is_run_active(run_id) {
        return Err(ApiError::UnknownRun(format!(
            "no run {} in this store",
            run_id.as_uuid()
        )));
    }

    let credential = credential(&state, &headers)?;
    let start = cursor(&headers, params.from_seq);
    let poll = state.poll_interval();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(produce(state, run_id, start, poll, credential, tx));

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

/// The starting sequence number: `Last-Event-ID` plus one when present, else
/// the `from_seq` query, else 0 (a full replay).
fn cursor(headers: &HeaderMap, from_seq: Option<u64>) -> u64 {
    if let Some(last) = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok())
    {
        return last + 1;
    }
    from_seq.unwrap_or(0)
}

/// The credential this stream re-checks, or `None` on a pass-through server,
/// where no bearer is configured and there is nothing to re-check.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when a bearer is configured and the request's
/// own value does not verify. [`require_bearer`](crate::auth::require_bearer)
/// refused that request already, so this answers a case the layer above makes
/// unreachable rather than opening a stream nothing re-checks if it ever
/// stopped being unreachable.
fn credential(state: &AppState, headers: &HeaderMap) -> Result<Option<StreamCredential>, ApiError> {
    let Some(auth) = state.auth() else {
        return Ok(None);
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    auth.capture(presented)
        .map(Some)
        .ok_or(ApiError::Unauthorized)
}

/// Reads the log from `start`, sends each event, then polls for new ones until
/// the run rests, and sends a final `end` frame.
///
/// `credential` is re-checked at the top of every pass; see the module docs.
async fn produce(
    state: AppState,
    run_id: RunId,
    start: u64,
    poll: Duration,
    credential: Option<StreamCredential>,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let store = state.store();
    let mut next = start;
    loop {
        if let (Some(credential), Some(auth)) = (credential.as_ref(), state.auth())
            && !credential.still_verifies(auth)
        {
            tracing::info!(
                caller = %credential.name(),
                run = %run_id.as_uuid(),
                reason = "token_revoked",
                "event stream ended: the bearer it opened under no longer verifies"
            );
            let frame = Event::default().event("end").data(
                json!({
                    "error": "the bearer this stream opened under no longer verifies; \
                              re-authenticate and open a fresh stream",
                    "reason": "unauthorized",
                })
                .to_string(),
            );
            let _ = tx.send(Ok(frame)).await;
            return;
        }

        let log = match store.read_log(run_id).await {
            Ok(log) => log,
            Err(error) => {
                let frame = Event::default()
                    .event("end")
                    .data(json!({ "error": format!("store: {error}") }).to_string());
                let _ = tx.send(Ok(frame)).await;
                return;
            }
        };

        let from = next;
        for envelope in log.iter().filter(|envelope| envelope.seq.get() >= from) {
            let data = serde_json::to_string(envelope).unwrap_or_default();
            let frame = Event::default()
                .id(envelope.seq.get().to_string())
                .data(data);
            if tx.send(Ok(frame)).await.is_err() {
                // The client hung up; stop producing.
                return;
            }
            next = envelope.seq.get() + 1;
        }

        let status = derive_state(&log).status;
        if is_resting(&status) {
            let frame = Event::default()
                .event("end")
                .data(json!({ "status": json::status(&status, state.now()) }).to_string());
            let _ = tx.send(Ok(frame)).await;
            return;
        }

        // A run that is mid-step but no longer being driven in this process was
        // detached (its task was aborted, or the server that drove it is gone).
        // End the stream so the client does not wait forever; recovering the run
        // opens a fresh stream that tails the continuation.
        if !log.is_empty() && !state.is_run_active(run_id) {
            let frame = Event::default().event("end").data(
                json!({ "status": json::status(&status, state.now()), "detached": true })
                    .to_string(),
            );
            let _ = tx.send(Ok(frame)).await;
            return;
        }

        tokio::time::sleep(poll).await;
    }
}

/// Whether a status is a resting point at which driving has stopped.
///
/// `Sleeping` is one of them. A run on a durable timer is passive data with
/// nothing driving it, and its deadline is measured in hours or weeks, so a
/// stream that kept polling for one would hold a connection open for the whole
/// nap and report nothing the end frame does not already carry: that frame's
/// status is `{"state": "sleeping", "wake_at": ...}`, which tells a client both
/// that the run stopped and exactly when to open a fresh stream. Waking is not
/// a continuation of this stream in any case; it is a new drive, and the events
/// it records are read by the stream a client opens then.
fn is_resting(status: &RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed { .. }
            | RunStatus::Failed { .. }
            | RunStatus::Abandoned { .. }
            | RunStatus::Suspended { .. }
            | RunStatus::Sleeping { .. }
            | RunStatus::BudgetExceeded { .. }
            | RunStatus::NeedsReconciliation
    )
}
