//! The server-sent-events client for one run's event stream.
//!
//! Feeds `GET /v1/runs/{id}/events` (see `crates/salvor-server/API.md`) into
//! two reactive signals: the growing log of [`EventEnvelope`]s and a
//! [`ConnectionState`]. The rest of the app is pure projection over those.
//!
//! # Why the browser `EventSource`
//!
//! Server-sent events are a browser primitive. `EventSource` frames the
//! `text/event-stream` for us: it splits `id:` / `data:` / named-event fields,
//! delivers unnamed `data` frames through `onmessage`, and delivers the
//! server's terminal `event: end` frame through a named `"end"` listener. So
//! there is no HTTP client or frame parser to hand-roll; the one thing left is
//! to decode each frame's `data` and manage reconnection.
//!
//! # Frames map straight onto the replay types
//!
//! Every envelope frame's `data` is the pinned event-envelope wire JSON, the
//! same bytes the log rows carry, so it deserializes directly into
//! [`salvor_replay::EventEnvelope`](crate::replay::EventEnvelope) with one
//! `serde_json::from_str`. There is no dashboard-local event type; see
//! [`crate::replay`].
//!
//! # Reconnect: the `from_seq` cursor
//!
//! On any transient drop the browser would auto-reconnect with `Last-Event-ID`,
//! but this client takes explicit control instead so the connection state is
//! honest and the cursor is visible: `onerror` closes the source, sets
//! [`ConnectionState::Reconnecting`], and schedules a reopen at
//! `?from_seq=<last_seq + 1>` (the non-browser cursor the API documents). The
//! log is append-only with contiguous ascending sequence numbers, so resuming
//! one past the last event held is gap-free and duplicate-free.
//!
//! # The terminal frame
//!
//! When the run reaches a resting point the server sends one `event: end` frame
//! carrying the final status (and `detached: true` when no driver is running in
//! that process), then closes. The `"end"` listener records that as an
//! [`EndFrame`], moves the connection to [`ConnectionState::Offline`], and
//! closes the source. A resting run is done streaming, so the drop that follows
//! must not trigger a reconnect; the `onerror` handler checks for a recorded
//! end first.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

use crate::config::Config;
use crate::replay::EventEnvelope;

/// How long to wait before a reconnect attempt, in milliseconds. A flat,
/// modest backoff; it can grow if it earns its place.
const RECONNECT_DELAY_MS: u32 = 1_000;

/// The live state of the stream, bound to the shell's connection indicator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// The stream is open and events are flowing.
    Connected,
    /// The stream dropped (or is first establishing) and a reopen is pending.
    Reconnecting,
    /// Not streaming and not retrying: either the run reached a resting point
    /// (an `end` frame arrived) or the source could not be opened.
    Offline,
}

/// The server's terminal `event: end` payload.
#[derive(Clone, Debug, PartialEq)]
pub struct EndFrame {
    /// The final status object, verbatim (its `state` and any extras, e.g. an
    /// `error` on a failed run). Kept as raw JSON here; the view layer renders
    /// it. This is where the API's "the end frame may carry an error" lands.
    pub status: serde_json::Value,
    /// True when the driving task is gone and no driver is running the run in
    /// this process; recovering the run opens a fresh stream.
    pub detached: bool,
}

/// Handles onto one run's stream. All three are reactive signals, so a view
/// binds to them and updates as the stream progresses. `Copy`, because Leptos
/// signals are arena handles.
#[derive(Clone, Copy)]
pub struct RunStream {
    /// The run's log, appended to as envelope frames arrive. Immutable in the
    /// past, which is what lets the scrubber fold any prefix (see
    /// [`crate::replay::state_as_of`]).
    pub events: RwSignal<Vec<EventEnvelope>>,
    /// The live connection state for the indicator.
    pub connection: RwSignal<ConnectionState>,
    /// `Some` once the terminal `end` frame has arrived.
    pub ended: RwSignal<Option<EndFrame>>,
}

/// Everything a stream's closures share: where to connect, the signals to
/// drive, the cursor, and the live source and its closures (kept alive here,
/// not leaked with `forget`).
struct StreamCtx {
    config: Config,
    run_id: String,
    events: RwSignal<Vec<EventEnvelope>>,
    connection: RwSignal<ConnectionState>,
    ended: RwSignal<Option<EndFrame>>,
    /// The sequence number of the last envelope appended, for the reconnect
    /// cursor. `None` until the first frame arrives.
    last_seq: Cell<Option<u64>>,
    /// The current source and its closures. Replacing this on reconnect drops
    /// the previous source and closures together.
    live: RefCell<Option<Live>>,
}

/// One open `EventSource` and the closures wired to it. Holding the closures
/// keeps them alive for as long as the source; dropping this whole value
/// (on reconnect or teardown) drops them cleanly.
struct Live {
    source: EventSource,
    _on_open: Closure<dyn FnMut()>,
    _on_error: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_end: Closure<dyn FnMut(MessageEvent)>,
}

/// Opens the stream for `run_id` and returns the reactive handles.
///
/// Starts with a full replay from sequence 0 (`from_seq` unset), so the client
/// ends up holding the whole log, then tails live. The returned [`RunStream`]
/// is `Copy` and can be dropped by the caller; the connection is kept alive by
/// the `Rc<StreamCtx>` its own closures hold.
#[must_use]
pub fn connect_run_stream(config: Config, run_id: impl Into<String>) -> RunStream {
    let events = RwSignal::new(Vec::<EventEnvelope>::new());
    // Reconnecting reads as "establishing" for the very first open, too.
    let connection = RwSignal::new(ConnectionState::Reconnecting);
    let ended = RwSignal::new(None::<EndFrame>);

    let ctx = Rc::new(StreamCtx {
        config,
        run_id: run_id.into(),
        events,
        connection,
        ended,
        last_seq: Cell::new(None),
        live: RefCell::new(None),
    });

    open(&ctx, None);

    RunStream {
        events,
        connection,
        ended,
    }
}

/// Opens (or reopens) the `EventSource` at the given cursor and wires its
/// handlers. `from_seq` is `None` for the initial full replay, `Some(n)` for a
/// reconnect resuming at sequence `n`.
fn open(ctx: &Rc<StreamCtx>, from_seq: Option<u64>) {
    // A recorded end means the run is at rest; never reopen against it.
    if ctx.ended.get_untracked().is_some() {
        ctx.connection.set(ConnectionState::Offline);
        return;
    }

    let url = ctx.config.events_url(&ctx.run_id, from_seq);
    let source = match EventSource::new(&url) {
        Ok(source) => source,
        Err(_) => {
            // Could not even construct the source: treat as offline.
            ctx.connection.set(ConnectionState::Offline);
            return;
        }
    };

    let on_open = {
        let ctx = ctx.clone();
        Closure::<dyn FnMut()>::new(move || {
            ctx.connection.set(ConnectionState::Connected);
        })
    };

    let on_message = {
        let ctx = ctx.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Some(text) = event.data().as_string() else {
                return;
            };
            // The frame's data is the pinned envelope wire JSON: it decodes
            // straight into the real replay type, no dashboard-local copy.
            match serde_json::from_str::<EventEnvelope>(&text) {
                Ok(envelope) => {
                    ctx.last_seq.set(Some(envelope.seq.get()));
                    ctx.events.update(|log| log.push(envelope));
                }
                Err(_) => {
                    // A frame that does not parse is dropped rather than
                    // crashing the stream; a future view can surface it.
                }
            }
        })
    };

    let on_end = {
        let ctx = ctx.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let end = event
                .data()
                .as_string()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .map(|value| EndFrame {
                    status: value
                        .get("status")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    detached: value
                        .get("detached")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                })
                .unwrap_or(EndFrame {
                    status: serde_json::Value::Null,
                    detached: false,
                });
            ctx.ended.set(Some(end));
            ctx.connection.set(ConnectionState::Offline);
            // Close ourselves so the trailing socket close is expected.
            if let Some(live) = ctx.live.borrow().as_ref() {
                live.source.close();
            }
        })
    };

    let on_error = {
        let ctx = ctx.clone();
        Closure::<dyn FnMut()>::new(move || {
            // A clean end already closed us; do not fight it with a reconnect.
            if ctx.ended.get_untracked().is_some() {
                ctx.connection.set(ConnectionState::Offline);
                return;
            }
            ctx.connection.set(ConnectionState::Reconnecting);
            if let Some(live) = ctx.live.borrow().as_ref() {
                live.source.close();
            }
            schedule_reconnect(&ctx);
        })
    };

    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    // Named events do not reach onmessage; the terminal frame has event: end.
    let _ = source.add_event_listener_with_callback("end", on_end.as_ref().unchecked_ref());

    // Keep the source and its closures alive; dropping the previous Live (if a
    // reconnect) closes and frees the old one.
    *ctx.live.borrow_mut() = Some(Live {
        source,
        _on_open: on_open,
        _on_error: on_error,
        _on_message: on_message,
        _on_end: on_end,
    });
}

/// Schedules a reopen after [`RECONNECT_DELAY_MS`], resuming one past the last
/// sequence held (`?from_seq`), or from 0 if nothing has arrived yet.
fn schedule_reconnect(ctx: &Rc<StreamCtx>) {
    let ctx = ctx.clone();
    let timeout = Timeout::new(RECONNECT_DELAY_MS, move || {
        let from_seq = ctx.last_seq.get().map(|seq| seq + 1);
        open(&ctx, from_seq);
    });
    // Let it fire on its own; the whole stream is torn down by dropping the
    // Rc<StreamCtx>, which a future teardown path can own.
    timeout.forget();
}
