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
//! failed, suspended, budget-exceeded, or needs-reconciliation) the stream
//! sends one final `event: end` frame carrying the final status, then closes.
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

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use salvor_core::{RunId, RunStatus, derive_state};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

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

    let start = cursor(&headers, params.from_seq);
    let poll = state.poll_interval();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(produce(state, run_id, start, poll, tx));

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

/// Reads the log from `start`, sends each event, then polls for new ones until
/// the run rests, and sends a final `end` frame.
async fn produce(
    state: AppState,
    run_id: RunId,
    start: u64,
    poll: Duration,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let store = state.store();
    let mut next = start;
    loop {
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
                .data(json!({ "status": json::status(&status) }).to_string());
            let _ = tx.send(Ok(frame)).await;
            return;
        }

        // A run that is mid-step but no longer being driven in this process was
        // detached (its task was aborted, or the server that drove it is gone).
        // End the stream so the client does not wait forever; recovering the run
        // opens a fresh stream that tails the continuation.
        if !log.is_empty() && !state.is_run_active(run_id) {
            let frame = Event::default()
                .event("end")
                .data(json!({ "status": json::status(&status), "detached": true }).to_string());
            let _ = tx.send(Ok(frame)).await;
            return;
        }

        tokio::time::sleep(poll).await;
    }
}

/// Whether a status is a resting point at which driving has stopped.
fn is_resting(status: &RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed { .. }
            | RunStatus::Failed { .. }
            | RunStatus::Abandoned { .. }
            | RunStatus::Suspended { .. }
            | RunStatus::BudgetExceeded { .. }
            | RunStatus::NeedsReconciliation
    )
}
