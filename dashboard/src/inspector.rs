//! The Run Inspector: the core view of the dashboard, at route `/runs/:id`.
//!
//! It reads one run end to end and lets an operator understand how the run
//! reached its current state. Everything here is a projection over one live
//! event stream; the inspector stores nothing of its own.
//!
//! # How it is wired
//!
//! On mount, [`RunInspector`] opens the SSE stream for the id via
//! [`connect_run_stream`](crate::sse::connect_run_stream) (which replays the
//! whole log from sequence 0, then live-tails) and mirrors that stream's
//! connection state into the shared [`AppConnection`] so the shell's indicator
//! reflects this run. The connection is single-per-mount: the SSE client does
//! not yet expose teardown, so the inspector connects
//! once for the id it mounted with rather than reconnecting on an in-place id
//! change.
//!
//! # The two readers of the fold
//!
//! Both the header and the scrubber read run state from the REAL
//! [`derive_state`](crate::replay::derive_state) fold, never a hand-rolled walk:
//!
//! - the header shows the head state ([`head_state`](crate::replay::head_state),
//!   the fold of the whole log),
//! - the scrubber shows the state as of a prefix
//!   ([`state_as_of`](crate::replay::state_as_of), the fold of a prefix), which
//!   is what makes scrubbing instant and always consistent with the runtime.
//!
//! Token totals shown anywhere are therefore exact (they come from the fold);
//! dollar costs are estimates layered on by [`crate::pricing`].

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;

use crate::app::AppConnection;
use crate::config::Config;
use crate::pricing;
use crate::replay::{
    BudgetKind, Effect as ToolEffect, Event, EventEnvelope, PendingCall, head_state, state_as_of,
};
use crate::sse::{ConnectionState, RunStream, connect_run_stream};
use crate::status::{RunRef, StatusBadge, StatusGroup, status_group, truncate_id};

/// The mounted inspector: opens the stream, mirrors connection state, and lays
/// out the header, the timeline, and the scrubber.
#[component]
pub fn RunInspector() -> impl IntoView {
    let params = use_params_map();
    // Connect once, for the id this component mounted with. Reading it untracked
    // is deliberate: teardown does not exist yet, so this view holds one stream
    // for its lifetime rather than reconnecting on an in-place id change.
    let run_id = params.with_untracked(|p| p.get("id").unwrap_or_default());

    let config = expect_context::<Config>();
    let stream = connect_run_stream(config, run_id);

    // Mirror this stream's connection into the shell's shared indicator, so the
    // app-wide connected / reconnecting / offline dot reflects the run on screen.
    let AppConnection(app_connection) = expect_context();
    Effect::new(move |_| {
        app_connection.set(stream.connection.get());
    });

    view! {
        <div class="inspector">
            <RunHeader stream=stream />
            <div class="inspector-body">
                <EventTimeline stream=stream />
                <Scrubber stream=stream />
            </div>
        </div>
    }
}

/// The run's header: identity, current status, start time and age, and totals.
#[component]
fn RunHeader(stream: RunStream) -> impl IntoView {
    let params = use_params_map();
    let run_id = params.with_untracked(|p| p.get("id").unwrap_or_default());

    // Head state = the real fold of the whole log. The header never walks events
    // itself to decide status.
    let head = move || stream.events.with(|log| head_state(log));

    // The agent identity we can show: the agent-definition hash from RunStarted.
    // The agent's human name is not in the log (see the note rendered below).
    let agent_hash = move || {
        stream.events.with(|log| {
            log.iter().find_map(|env| match &env.event {
                Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
                _ => None,
            })
        })
    };

    let started = move || {
        stream.events.with(|log| {
            log.first().map(|env| {
                let dt = env.recorded_at;
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
                    dt.year(),
                    u8::from(dt.month()),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                )
            })
        })
    };
    let age = move || {
        stream.events.with(|log| {
            log.first()
                .map(|env| format_age(env.recorded_at.unix_timestamp()))
        })
    };
    let event_count = move || stream.events.with(std::vec::Vec::len);
    let parked = move || matches!(status_group(&head().status), StatusGroup::WaitingOnHuman);

    view! {
        <header class="run-header">
            <div class="run-header-top">
                <RunRef id=run_id.clone() />
                {move || view! { <StatusBadge status=head().status /> }}
            </div>
            <div class="run-header-agent">
                {move || match agent_hash() {
                    Some(hash) => view! {
                        <span class="agent-hash" title=hash.clone()>"agent " {truncate_id(&hash)}</span>
                    }.into_any(),
                    None => view! { <span class="agent-hash muted">"agent unknown"</span> }.into_any(),
                }}
                <span class="hint">
                    " (identified by agent-definition hash; the agent name is not recorded in the log)"
                </span>
            </div>
            <dl class="run-header-totals">
                <dt>"Started"</dt>
                <dd>{move || started().unwrap_or_else(|| "\u{2014}".to_string())}</dd>
                <dt>"Age"</dt>
                <dd>{move || age().unwrap_or_else(|| "\u{2014}".to_string())}</dd>
                <dt>"Events"</dt>
                <dd>{move || event_count().to_string()}</dd>
                <dt>"Cost / tokens"</dt>
                <dd><CostMeter stream=stream /></dd>
            </dl>
            {move || parked().then(|| view! {
                <p class="parked-pointer" data-group="waiting-on-human">
                    "This run is waiting on a human. "
                    <A href="/inbox">"Open the Inbox"</A>
                    " to resume or resolve it (actions live there, not here)."
                </p>
            })}
        </header>
    }
}

/// Exact token totals from the fold, plus a dollar estimate from the price
/// table. Degrades to tokens-only when a completed call's model is unknown.
#[component]
fn CostMeter(stream: RunStream) -> impl IntoView {
    // Tokens are exact: taken from the real fold's accumulated usage.
    let usage = move || stream.events.with(|log| head_state(log).usage);
    // Dollars are an estimate: derived by pricing::estimate_cost over the log.
    let summary = move || stream.events.with(|log| pricing::estimate_cost(log));

    view! {
        <span
            class="cost-meter"
            data-available=move || summary().is_available().to_string()
        >
            <span class="cost-tokens">
                {move || {
                    let u = usage();
                    format!("{} in / {} out tokens (exact)", u.input_tokens, u.output_tokens)
                }}
            </span>
            <span class="cost-usd">
                {move || {
                    let s = summary();
                    if s.is_available() {
                        format!("~${:.4} (estimate)", s.usd)
                    } else {
                        "cost unavailable (unknown model)".to_string()
                    }
                }}
            </span>
        </span>
    }
}

/// The vertical, sequence-ordered list of events, one [`EventRow`] each, with a
/// sequence gutter. Owns the loading / empty / error states.
#[component]
fn EventTimeline(stream: RunStream) -> impl IntoView {
    // Still establishing the stream and nothing has arrived: loading.
    let is_loading = move || {
        matches!(stream.connection.get(), ConnectionState::Reconnecting)
            && stream.events.with(std::vec::Vec::is_empty)
    };
    // Offline with no events and no terminal frame: the stream could not load.
    let is_error = move || {
        matches!(stream.connection.get(), ConnectionState::Offline)
            && stream.events.with(std::vec::Vec::is_empty)
            && stream.ended.with(std::option::Option::is_none)
    };
    // Connected/at-rest but no events recorded: a genuine empty timeline.
    let is_empty =
        move || stream.events.with(std::vec::Vec::is_empty) && !is_loading() && !is_error();

    view! {
        <section class="event-timeline">
            {move || is_loading().then(|| view! {
                <p class="timeline-state" data-state="loading">"Connecting to the run stream\u{2026}"</p>
            })}
            {move || is_error().then(|| view! {
                <p class="timeline-state" data-state="error">"Could not load this run."</p>
            })}
            {move || is_empty().then(|| view! {
                <p class="timeline-state" data-state="empty">"No events recorded yet."</p>
            })}
            <ul class="event-rows">
                <For
                    each=move || stream.events.get()
                    key=|env| env.seq.get()
                    let:env
                >
                    <EventRow envelope=env />
                </For>
            </ul>
        </section>
    }
}

/// One row per event, specialized by event kind, with a sequence gutter.
#[component]
fn EventRow(envelope: EventEnvelope) -> impl IntoView {
    let seq = envelope.seq.get();
    let kind = event_kind_slug(&envelope.event);
    let body = render_event_body(&envelope.event);
    view! {
        <li class="event-row" data-kind=kind>
            <span class="seq-gutter">{seq}</span>
            <div class="event-body">
                <span class="event-kind">{kind}</span>
                {body}
            </div>
        </li>
    }
}

/// The centerpiece: a control bound to an event index whose panel recomputes the
/// run's derived state AS OF that index by folding the prefix through the REAL
/// fold. Instant and in-browser: no server round-trip per step, and no second
/// state machine that could drift from the runtime.
#[component]
fn Scrubber(stream: RunStream) -> impl IntoView {
    // The prefix length being viewed. 0 is the empty log; log.len() is the head.
    let index = RwSignal::new(0usize);

    // Follow the live edge until the operator scrubs back. The effect's previous
    // value is the log length at the last run: if the index was sitting at that
    // edge, advance it to the new length as events append; otherwise leave the
    // scrubbed-to position alone (and clamp, defensively).
    Effect::new(move |prev: Option<usize>| {
        let len = stream.events.with(std::vec::Vec::len);
        let prev_len = prev.unwrap_or(0);
        index.update(|i| {
            if *i >= prev_len {
                *i = len;
            } else if *i > len {
                *i = len;
            }
        });
        len
    });

    let len = move || stream.events.with(std::vec::Vec::len);

    // Criterion-6 call: recompute point-in-time state by folding the prefix
    // through the REAL derive_state via crate::replay::state_as_of. This is the
    // whole premise of the scrubber; there is no hand-rolled walk here.
    let as_of = move || stream.events.with(|log| state_as_of(log, index.get()));

    view! {
        <aside class="scrubber">
            <div class="scrubber-control">
                <label>"Scrub through the log"</label>
                <input
                    type="range"
                    min="0"
                    max=move || len().to_string()
                    prop:value=move || index.get().to_string()
                    on:input=move |ev| {
                        if let Ok(value) = event_target_value(&ev).parse::<usize>() {
                            index.set(value);
                        }
                    }
                />
                <span class="scrubber-position">
                    {move || format!("event {} / {}", index.get(), len())}
                </span>
            </div>
            <div class="point-in-time">
                <h3>"State as of event " {move || index.get()}</h3>
                {move || view! { <StatusBadge status=as_of().status /> }}
                <dl class="pit-detail">
                    <dt>"Usage so far"</dt>
                    <dd>{move || {
                        let u = as_of().usage;
                        format!("{} in / {} out tokens", u.input_tokens, u.output_tokens)
                    }}</dd>
                    <dt>"Pending call"</dt>
                    <dd>{move || match as_of().pending_call {
                        Some(ref pending) => pending_summary(pending),
                        None => "none".to_string(),
                    }}</dd>
                </dl>
            </div>
        </aside>
    }
}

/// A stable kebab-case slug for an event kind, used as text and a `data-kind`
/// hook. Exhaustive, so a new [`Event`] variant upstream forces a case here.
fn event_kind_slug(event: &Event) -> &'static str {
    match event {
        Event::RunStarted { .. } => "run-started",
        Event::ModelCallRequested { .. } => "model-call-requested",
        Event::ModelCallCompleted { .. } => "model-call-completed",
        Event::ToolCallRequested { .. } => "tool-call-requested",
        Event::ToolCallCompleted { .. } => "tool-call-completed",
        Event::NowObserved { .. } => "now-observed",
        Event::RandomObserved { .. } => "random-observed",
        Event::Suspended { .. } => "suspended",
        Event::Resumed { .. } => "resumed",
        Event::BudgetExceeded { .. } => "budget-exceeded",
        Event::RunCompleted { .. } => "run-completed",
        Event::RunFailed { .. } => "run-failed",
    }
}

/// The tool effect class as its wire slug, for the effect badge.
fn effect_slug(effect: ToolEffect) -> &'static str {
    match effect {
        ToolEffect::Read => "read",
        ToolEffect::Idempotent => "idempotent",
        ToolEffect::Write => "write",
    }
}

/// A human label for a budget dimension.
fn budget_kind_label(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost (USD)",
        BudgetKind::WallTime => "wall time",
        BudgetKind::Steps => "steps",
    }
}

/// A one-line description of a dangling call, for the scrubber panel.
fn pending_summary(pending: &PendingCall) -> String {
    match pending {
        PendingCall::Model { seq, request_hash } => {
            format!("model call @ seq {seq} ({})", truncate_id(request_hash))
        }
        PendingCall::Tool {
            seq,
            tool,
            effect,
            idempotency_key,
            ..
        } => {
            let key = idempotency_key
                .as_deref()
                .map(|k| format!(", key {}", truncate_id(k)))
                .unwrap_or_default();
            format!("tool '{tool}' [{}] @ seq {seq}{key}", effect_slug(*effect))
        }
    }
}

/// Renders the kind-specific body of a row. Every variant is handled; payloads
/// go in a `<details>` so they expand and collapse with no per-row state.
fn render_event_body(event: &Event) -> AnyView {
    match event {
        Event::RunStarted {
            agent_def_hash,
            input,
        } => view! {
            <div>
                <span class="agent-hash" title=agent_def_hash.clone()>
                    "agent " {truncate_id(agent_def_hash)}
                </span>
                <span class="hint">" (agent hash; name not recorded)"</span>
                <details><summary>"input"</summary><pre>{pretty(input)}</pre></details>
            </div>
        }
        .into_any(),

        Event::ModelCallRequested {
            request_hash,
            request_body,
            ..
        } => {
            // The opt-in-recording contract: show the prompt only when the body
            // is actually present. When it is absent, say so plainly rather than
            // implying a prompt exists.
            let body_view = match request_body {
                Some(body) => view! {
                    <details><summary>"prompt (recorded)"</summary><pre>{pretty(body)}</pre></details>
                }
                .into_any(),
                None => view! {
                    <span class="hint">"prompt not recorded (recording is opt-in)"</span>
                }
                .into_any(),
            };
            view! {
                <div>
                    <span class="req-hash" title=request_hash.clone()>
                        "request " {truncate_id(request_hash)}
                    </span>
                    {body_view}
                </div>
            }
            .into_any()
        }

        Event::ModelCallCompleted {
            response, usage, ..
        } => {
            // Tokens are exact; cost is an estimate keyed by the response model.
            let model = pricing::model_id_of_response(response).map(str::to_string);
            let cost = pricing::call_cost_usd(model.as_deref(), *usage);
            let cost_label = match cost {
                Some(c) => format!("~${c:.4} (estimate)"),
                None => "cost unavailable".to_string(),
            };
            let model_label = model.unwrap_or_else(|| "unknown model".to_string());
            let (input_tokens, output_tokens) = (usage.input_tokens, usage.output_tokens);
            view! {
                <div>
                    <span class="usage">{format!("{input_tokens} in / {output_tokens} out tokens")}</span>
                    <span class="model">{model_label}</span>
                    <span class="cost">{cost_label}</span>
                    <details><summary>"response"</summary><pre>{pretty(response)}</pre></details>
                </div>
            }
            .into_any()
        }

        Event::ToolCallRequested {
            tool,
            input,
            effect,
            idempotency_key,
            ..
        } => {
            let key_view = match idempotency_key {
                Some(key) => view! {
                    <span class="idem-key" title=key.clone()>"idempotency " {truncate_id(key)}</span>
                }
                .into_any(),
                None => ().into_any(),
            };
            view! {
                <div>
                    <span class="tool-name">{tool.clone()}</span>
                    <span class="effect-badge" data-effect=effect_slug(*effect)>{effect_slug(*effect)}</span>
                    {key_view}
                    <details><summary>"input"</summary><pre>{pretty(input)}</pre></details>
                </div>
            }
            .into_any()
        }

        Event::ToolCallCompleted { output, .. } => view! {
            <div><details><summary>"output"</summary><pre>{pretty(output)}</pre></details></div>
        }
        .into_any(),

        Event::NowObserved { now } => {
            let observed = format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
                now.year(),
                u8::from(now.month()),
                now.day(),
                now.hour(),
                now.minute(),
                now.second(),
            );
            view! { <span class="ctx">"observed now " {observed}</span> }.into_any()
        }

        Event::RandomObserved { value } => {
            view! { <span class="ctx">"observed random " {value.to_string()}</span> }.into_any()
        }

        Event::Suspended {
            reason,
            input_schema,
        } => view! {
            <div>
                <span class="reason">{reason.clone()}</span>
                <details><summary>"awaited input schema"</summary><pre>{pretty(input_schema)}</pre></details>
            </div>
        }
        .into_any(),

        Event::Resumed { input } => view! {
            <div><details><summary>"resumed with input"</summary><pre>{pretty(input)}</pre></details></div>
        }
        .into_any(),

        Event::BudgetExceeded { budget, observed } => view! {
            <div>
                <span class="budget">
                    {format!(
                        "budget crossed: {} (limit {}, observed {})",
                        budget_kind_label(budget.kind),
                        budget.limit,
                        observed,
                    )}
                </span>
            </div>
        }
        .into_any(),

        Event::RunCompleted { output } => view! {
            <div><details><summary>"final output"</summary><pre>{pretty(output)}</pre></details></div>
        }
        .into_any(),

        Event::RunFailed { error } => view! {
            <div class="failed"><span class="error">{error.clone()}</span></div>
        }
        .into_any(),
    }
}

/// Pretty-prints a JSON payload for a `<pre>` block.
fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// A compact human age from a unix-seconds start, measured against the browser
/// clock. Only reached at runtime in the browser; it is not exercised by the
/// host unit tests, which stay off the wall clock.
fn format_age(start_unix_secs: i64) -> String {
    let now_secs = (js_sys::Date::now() / 1000.0) as i64;
    let mut secs = (now_secs - start_unix_secs).max(0);
    let days = secs / 86_400;
    secs %= 86_400;
    let hours = secs / 3_600;
    secs %= 3_600;
    let minutes = secs / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a log the way the SSE client does, from wire JSON, so the tests need
    // no clock or uuid dependency to name an envelope's internals.
    fn log(frames: &[&str]) -> Vec<EventEnvelope> {
        frames
            .iter()
            .map(|frame| serde_json::from_str(frame).expect("frame is valid envelope JSON"))
            .collect()
    }

    const STARTED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":0,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunStarted","payload":{"agent_def_hash":"sha256:agentdefhash","input":{"topic":"otters"}}}}"#;
    const WRITE_REQUESTED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":1,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallRequested","payload":{"seq":1,"tool":"charge_card","input":{"amount":10},"effect":"write","idempotency_key":null}}}"#;

    #[test]
    fn effect_slugs_are_the_wire_forms() {
        assert_eq!(effect_slug(ToolEffect::Read), "read");
        assert_eq!(effect_slug(ToolEffect::Idempotent), "idempotent");
        assert_eq!(effect_slug(ToolEffect::Write), "write");
    }

    #[test]
    fn every_kind_has_a_distinct_slug() {
        let events = log(&[STARTED, WRITE_REQUESTED]);
        assert_eq!(event_kind_slug(&events[0].event), "run-started");
        assert_eq!(event_kind_slug(&events[1].event), "tool-call-requested");
    }

    #[test]
    fn pending_summary_names_the_dangling_write() {
        // The prefix ending at the uncompleted write derives a pending tool call.
        let state = state_as_of(&log(&[STARTED, WRITE_REQUESTED]), 2);
        let pending = state.pending_call.expect("a pending call");
        let summary = pending_summary(&pending);
        assert!(summary.contains("charge_card"), "{summary}");
        assert!(summary.contains("[write]"), "{summary}");
    }
}
