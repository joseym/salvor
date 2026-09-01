//! Rendering one event as the two short strings an operator reads: the stable
//! kind label ([`event_kind`]) and a one-line detail ([`event_detail`]).
//!
//! Both are pure functions from an [`Event`] to text: no clock, no IO, no
//! dependency on the layer that produced the event. That is what lets a single
//! implementation serve every surface. The runtime emits these two strings live
//! as each event becomes durable, the CLI prints them again when inspecting a
//! log afterward, and a browser inspector renders them from the same compiled
//! fold. A step therefore reads identically wherever you meet it.
//!
//! # What the detail withholds
//!
//! The detail deliberately does **not** carry full payloads. Inputs, outputs,
//! resume values, and error messages are truncated, and hashes are shortened,
//! so a model's raw output or a tool's raw arguments never reach a progress
//! stream in full. The untruncated form lives only in the event log.

use serde_json::Value;
use time::OffsetDateTime;

use crate::event::{BudgetKind, Event, Performer, SettledBy};
#[cfg(test)]
use crate::id::{RunId, SequenceNumber};

/// The stable `kind` label for one event, matching the enum variant name so
/// it reads the same as the wire form's `kind` tag.
#[must_use]
pub fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::RunStarted { .. } => "RunStarted",
        Event::ModelCallRequested { .. } => "ModelCallRequested",
        Event::ModelCallCompleted { .. } => "ModelCallCompleted",
        Event::ToolCallRequested { .. } => "ToolCallRequested",
        Event::ToolCallCompleted { .. } => "ToolCallCompleted",
        Event::NowObserved { .. } => "NowObserved",
        Event::RandomObserved { .. } => "RandomObserved",
        Event::Suspended { .. } => "Suspended",
        Event::Resumed { .. } => "Resumed",
        Event::RunRedriven { .. } => "RunRedriven",
        Event::SleepStarted { .. } => "SleepStarted",
        Event::SleepCompleted {} => "SleepCompleted",
        Event::BudgetExceeded { .. } => "BudgetExceeded",
        Event::RunCompleted { .. } => "RunCompleted",
        Event::RunFailed { .. } => "RunFailed",
        Event::RunAbandoned { .. } => "RunAbandoned",
        Event::GraphRunStarted { .. } => "GraphRunStarted",
        Event::NodeEntered { .. } => "NodeEntered",
        Event::NodeExited { .. } => "NodeExited",
        Event::NodeSkipped { .. } => "NodeSkipped",
        Event::BranchTaken { .. } => "BranchTaken",
        Event::MapFannedOut { .. } => "MapFannedOut",
        Event::MapIterationStarted { .. } => "MapIterationStarted",
        Event::MapIterationJoined { .. } => "MapIterationJoined",
        Event::FoldIterationStarted { .. } => "FoldIterationStarted",
        Event::FoldIterationJoined { .. } => "FoldIterationJoined",
        Event::FoldConverged { .. } => "FoldConverged",
    }
}

/// The informative payload of one event, rendered as a single line. Picks the
/// fields that matter per kind: a tool call shows its name, effect, and (when
/// a client performed it) that fact, a model completion its token usage, a
/// suspension its reason. Hashes are shortened and every payload is
/// truncated, so no full input, output, or error text reaches the progress
/// stream; `salvor history --json` is the escape hatch for the untruncated
/// envelope.
#[must_use]
pub fn event_detail(event: &Event) -> String {
    match event {
        Event::RunStarted {
            agent_def_hash,
            input,
            caller,
            ..
        } => format!(
            "agent {} input {}{}",
            short_hash(agent_def_hash),
            truncate_json(input),
            caller_marker(caller.as_deref())
        ),
        Event::ModelCallRequested {
            request_hash,
            performed_by,
            ..
        } => {
            format!(
                "request {}{}",
                short_hash(request_hash),
                performer_marker(*performed_by)
            )
        }
        Event::ModelCallCompleted { usage, .. } => format!(
            "usage in {} out {}",
            usage.input_tokens, usage.output_tokens
        ),
        Event::ToolCallRequested {
            tool,
            input,
            effect,
            idempotency_key,
            performed_by,
            ..
        } => {
            let key = idempotency_key
                .as_deref()
                .map_or_else(String::new, |k| format!(" key {k}"));
            let performer = performer_marker(*performed_by);
            format!(
                "{tool} [{effect:?}]{performer}{key} input {}",
                truncate_json(input)
            )
        }
        Event::ToolCallCompleted {
            output,
            deduplicated_from,
            settled_by,
            settled_caller,
            ..
        } => {
            // Absent (the field's default, and every completion recorded before
            // it existed) means this call ran and this output is what it
            // produced. A recorded origin means the opposite, which a reader
            // must not have to infer from silence, so it is said out loud.
            let copied = deduplicated_from.map_or_else(String::new, |origin| {
                format!(
                    " (deduplicated: copied from run {} seq {})",
                    origin.run_id.as_uuid(),
                    origin.seq
                )
            });
            // Who settled the call, when it was not the run itself. It reads
            // in the same bracketed register the intent line uses for
            // `[Client]`, and it goes on every shape a completion can take,
            // because a hand-recorded completion is a hand-recorded completion
            // whatever output it carries.
            let settler = settled_by_marker(*settled_by, settled_caller.as_deref());
            if let Some(reason) = suspension_reason(output) {
                format!("suspends: {reason}{settler}{copied}")
            } else if let Some(wake_at) = sleep_wake_at(output) {
                format!("sleeps until {}{settler}{copied}", format_ts(wake_at))
            } else if let Some(failure) = recorded_failure(output) {
                format!(
                    "error ({}, {} attempt(s)): {}{settler}{copied}",
                    failure.kind,
                    failure.attempts,
                    truncate_str(failure.message)
                )
            } else {
                format!("output {}{settler}{copied}", truncate_json(output))
            }
        }
        Event::NowObserved { now } => format_ts(*now),
        Event::RandomObserved { value } => format!("value {value}"),
        Event::Suspended { reason, .. } => format!("reason: {reason}"),
        Event::Resumed { input, caller } => format!(
            "input {}{}",
            truncate_json(input),
            caller_marker(caller.as_deref())
        ),
        // A redrive has nothing to report but who asked for it, so the marker
        // that names them on `RunStarted` and `Resumed` is the whole line.
        Event::RunRedriven { caller } => {
            format!("redriven{}", caller_marker(caller.as_deref()))
        }
        // The wake instant is the whole of what a reader wants here, rendered
        // by the same component-wise formatter `NowObserved` uses.
        Event::SleepStarted { wake_at } => format!("until {}", format_ts(*wake_at)),
        Event::SleepCompleted {} => "woke".to_owned(),
        Event::BudgetExceeded { budget, observed } => {
            format!(
                "{} limit {}, observed {}",
                budget_label(budget.kind),
                fmt_num(budget.limit),
                fmt_num(*observed)
            )
        }
        Event::RunCompleted { output } => format!("output {}", truncate_json(output)),
        Event::RunFailed { error } => format!("error: {}", truncate_str(error)),
        Event::RunAbandoned {
            reason,
            unresolved_write,
            caller,
        } => {
            let why = reason
                .as_deref()
                .map_or_else(|| "no reason given".to_owned(), truncate_str);
            let who = caller_marker(caller.as_deref());
            match unresolved_write {
                Some(write) => format!(
                    "abandoned: {why} (unresolved write at seq {}, tool {}){who}",
                    write.seq.get(),
                    write.tool
                ),
                None => format!("abandoned: {why}{who}"),
            }
        }
        Event::GraphRunStarted {
            graph_hash,
            input,
            caller,
            ..
        } => format!(
            "graph {} input {}{}",
            short_hash(graph_hash),
            truncate_json(input),
            caller_marker(caller.as_deref())
        ),
        Event::NodeEntered { node } => format!("enter {node}"),
        Event::NodeExited { node } => format!("exit {node}"),
        Event::NodeSkipped { node, reason } => format!("skip {node}: {}", truncate_str(reason)),
        Event::BranchTaken { node, case } => format!("branch {node} -> {case}"),
        Event::MapFannedOut { node, items } => {
            format!("map {node} fan-out {}", truncate_json(items))
        }
        Event::MapIterationStarted {
            node,
            index,
            child_run,
        } => format!("map {node}[{index}] child {}", short_hash(child_run)),
        Event::MapIterationJoined { node, index } => format!("map {node}[{index}] joined"),
        Event::FoldIterationStarted { node, index } => format!("fold {node}[{index}] started"),
        Event::FoldIterationJoined { node, index } => format!("fold {node}[{index}] joined"),
        Event::FoldConverged {
            node,
            winner_index,
            reason,
        } => format!(
            "fold {node} converged on [{winner_index}]: {}",
            truncate_str(reason)
        ),
    }
}

/// The reserved key marking a completion output as a recorded suspension.
///
/// The two sentinel shapes below are a recorded wire contract. `salvor-runtime`
/// writes them into a tool call's completion output and decodes them back into
/// its own `Suspension` and `ToolFailure` types; this module reads the same
/// recorded fields to render the line. The key names, the field names, and the
/// three legal `kind` strings must therefore agree between the two.
///
/// The reading is spelled out here instead of calling the runtime's decoders
/// because those decoders return types owned by the tools layer, which is
/// executor-bound. A renderer needs the recorded fields, not those types, and
/// must stay free of that dependency to remain buildable for wasm32.
const SUSPEND_SENTINEL_KEY: &str = "__salvor_suspend";

/// The reserved key marking a completion output as a recorded sleep request.
/// See [`SUSPEND_SENTINEL_KEY`] for the shared-contract note.
const SLEEP_SENTINEL_KEY: &str = "__salvor_sleep";

/// The reserved key marking a completion output as a recorded tool failure.
/// See [`SUSPEND_SENTINEL_KEY`] for the shared-contract note.
const ERROR_SENTINEL_KEY: &str = "__salvor_error";

/// The three legal values of a recorded failure's `kind` field, mirroring the
/// runtime's `ToolFailureKind` wire strings. A completion carrying any other
/// value is not a failure sentinel and renders as an ordinary output.
const FAILURE_KINDS: [&str; 3] = ["invalid_input", "handler", "output_serialization"];

/// The recorded failure fields a detail line shows, borrowed out of the
/// sentinel body.
struct RecordedFailure<'v> {
    /// Which dispatch layer failed, as its recorded wire string.
    kind: &'v str,
    /// The full recorded error chain, truncated only at render time.
    message: &'v str,
    /// How many times the call executed, counting retries.
    attempts: u32,
}

/// The reason of a completion output that is the suspension sentinel; `None`
/// for every other value.
fn suspension_reason(output: &Value) -> Option<&str> {
    let body = sentinel_body(output, SUSPEND_SENTINEL_KEY)?;
    let reason = body.get("reason")?.as_str()?;
    // A recorded suspension always carries the schema its resume input must
    // satisfy; a body missing it is not one.
    body.get("input_schema")?;
    Some(reason)
}

/// The wake instant of a completion output that is the sleep sentinel; `None`
/// for every other value.
///
/// The recorded field is an RFC 3339 string, parsed back to an instant so the
/// line renders through the same formatter `Event::SleepStarted`'s does. A
/// value that will not parse is not a sleep sentinel: rendering an
/// unreadable deadline as if it were one would be worse than showing the
/// recorded output verbatim.
fn sleep_wake_at(output: &Value) -> Option<OffsetDateTime> {
    let body = sentinel_body(output, SLEEP_SENTINEL_KEY)?;
    let recorded = body.get("wake_at")?.as_str()?;
    OffsetDateTime::parse(recorded, &time::format_description::well_known::Rfc3339).ok()
}

/// The recorded fields of a completion output that is the failure sentinel;
/// `None` for every other value.
fn recorded_failure(output: &Value) -> Option<RecordedFailure<'_>> {
    let body = sentinel_body(output, ERROR_SENTINEL_KEY)?;
    let kind = body.get("kind")?.as_str()?;
    if !FAILURE_KINDS.contains(&kind) {
        return None;
    }
    Some(RecordedFailure {
        kind,
        message: body.get("message")?.as_str()?,
        attempts: u32::try_from(body.get("attempts")?.as_u64()?).ok()?,
    })
}

/// The sentinel's body when `output` is an object with exactly one key equal
/// to `key`; `None` for every other value.
fn sentinel_body<'v>(output: &'v Value, key: &str) -> Option<&'v Value> {
    let map = output.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.get(key)
}

/// The marker a call intent carries when the CLIENT performed it, and the
/// empty string otherwise.
///
/// Absent (the field's default, and every intent recorded before the field
/// existed) means salvor performed the call itself: the overwhelmingly common
/// case, so it renders nothing. Only a recorded [`Performer::Client`] gets a
/// marker, in the same bracketed register the effect class uses. Model and
/// tool intents share this function so the same fact reads the same way on
/// both lines.
fn performer_marker(performed_by: Option<Performer>) -> &'static str {
    match performed_by {
        Some(Performer::Client) => " [Client]",
        None | Some(Performer::Server) => "",
    }
}

/// The bracketed marker naming who settled a completion, for the detail line.
///
/// Absent (the field's default, and every completion recorded before the field
/// existed) means the run recorded what it saw, which is the overwhelmingly
/// common case, so it renders nothing. Only a completion a person recorded by
/// hand gets a marker, in the same bracketed register
/// [`performer_marker`] uses on the intent line, so "who did this" reads the
/// same way on both halves of a call.
/// `settled_caller` names that person when the surface that recorded the
/// completion had a name to record, and rides inside the same brackets:
/// `[Operator: ci]`. With no name recorded the marker is the `[Operator]` it
/// has always been, so a log written before the name existed renders exactly
/// as it did.
fn settled_by_marker(settled_by: Option<SettledBy>, settled_caller: Option<&str>) -> String {
    match (settled_by, settled_caller) {
        (Some(SettledBy::Operator), Some(name)) => format!(" [Operator: {name}]"),
        (Some(SettledBy::Operator), None) => " [Operator]".to_owned(),
        (None, _) => String::new(),
    }
}

/// The bracketed marker naming who asked for an event, for the detail line.
///
/// Absent (the field's default, and every event recorded before the field
/// existed) means no caller was named: a server running the pass-through
/// posture records none, so it renders nothing. A recorded name reads in the
/// same bracketed register [`performer_marker`] and [`settled_by_marker`] use,
/// labelled so a name is never read as one of the fixed markers.
fn caller_marker(caller: Option<&str>) -> String {
    match caller {
        Some(name) => format!(" [caller: {name}]"),
        None => String::new(),
    }
}

/// Shortens a `sha256:...` hash to its prefix and the first seven hex digits,
/// so a line names a request without a 64-character wall of hex.
fn short_hash(hash: &str) -> String {
    match hash.split_once(':') {
        Some((scheme, hex)) => {
            let head: String = hex.chars().take(7).collect();
            if hex.len() > 7 {
                format!("{scheme}:{head}\u{2026}")
            } else {
                format!("{scheme}:{hex}")
            }
        }
        None => hash.chars().take(12).collect(),
    }
}

/// A human word for a budget dimension.
fn budget_label(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "steps",
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost_usd",
        BudgetKind::WallTime => "wall_time",
    }
}

/// Formats an `f64` budget figure without a needless `.0` when it is integral.
/// Steps and tokens are whole numbers even though every budget dimension rides
/// the wire as a float; the integral cutoff stays inside the range where an
/// `f64` holds integers exactly (see the `Budget` docs).
fn fmt_num(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Formats a timestamp as `YYYY-MM-DD HH:MM:SSZ` from its components, avoiding
/// a dependency on the `time` crate's optional `formatting` feature.
fn format_ts(ts: OffsetDateTime) -> String {
    let utc = ts.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
}

/// Compact one-line JSON, truncated so a payload never blows out a line and
/// never streams in full.
fn truncate_json(value: &serde_json::Value) -> String {
    truncate_str(&value.to_string())
}

/// Truncates a string to a scannable length with an ellipsis, so no full
/// payload or error message reaches the progress stream.
fn truncate_str(text: &str) -> String {
    const CAP: usize = 80;
    if text.chars().count() > CAP {
        let head: String = text.chars().take(CAP).collect();
        format!("{head}\u{2026}")
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    /// A completion that copied its output says so, and names what it copied.
    /// A completion that executed says nothing extra, so every line ever
    /// rendered before this field existed reads exactly as it did.
    #[test]
    fn a_deduplicated_completion_says_what_it_copied() {
        let origin = crate::event::DedupOrigin {
            run_id: RunId::from_uuid(
                Uuid::parse_str("00000000-0000-4000-8000-0000000000aa").expect("uuid"),
            ),
            seq: SequenceNumber::new(4),
        };
        let executed = event_detail(&Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: None,
            settled_by: None,
            settled_caller: None,
        });
        assert_eq!(executed, r#"output {"charge_id":"po_1"}"#);

        let copied = event_detail(&Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: Some(origin),
            settled_by: None,
            settled_caller: None,
        });
        assert_eq!(
            copied,
            r#"output {"charge_id":"po_1"} (deduplicated: copied from run 00000000-0000-4000-8000-0000000000aa seq 4)"#
        );
    }

    /// A completion a person recorded by hand says so, in the same bracketed
    /// register the intent line says `[Client]` in. A completion the run
    /// recorded itself says nothing extra, so every line ever rendered before
    /// this field existed reads exactly as it did.
    #[test]
    fn a_hand_recorded_completion_names_its_settler() {
        let resolved = event_detail(&Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: None,
            settled_by: Some(crate::event::SettledBy::Operator),
            settled_caller: None,
        });
        assert_eq!(resolved, r#"output {"charge_id":"po_1"} [Operator]"#);

        // The same marker on the intent half of the call, so a client-performed
        // write an operator resolved reads as both facts in one log.
        let intent = event_detail(&Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "charge_card".into(),
            input: json!({"amount_cents": 500}),
            effect: crate::effect::Effect::Write,
            idempotency_key: None,
            performed_by: Some(Performer::Client),
        });
        assert_eq!(
            intent,
            r#"charge_card [Write] [Client] input {"amount_cents":500}"#
        );
    }

    /// The name rides inside the settler's own brackets when the surface that
    /// recorded the completion had one, so the mechanism and the person read
    /// as one fact rather than two markers.
    #[test]
    fn a_named_settler_reads_inside_the_operator_marker() {
        let resolved = event_detail(&Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: None,
            settled_by: Some(crate::event::SettledBy::Operator),
            settled_caller: Some("ops".into()),
        });
        assert_eq!(resolved, r#"output {"charge_id":"po_1"} [Operator: ops]"#);
    }

    /// The five events that record who asked for them render the name in the
    /// same bracketed register, and render nothing at all when no name was
    /// recorded, so every line written before the field existed reads exactly
    /// as it did.
    #[test]
    fn the_caller_marker_names_who_asked_and_is_silent_otherwise() {
        let started = |caller: Option<&str>| {
            event_detail(&Event::RunStarted {
                agent_def_hash: "sha256:abcdef0123456789".into(),
                input: json!("ship it"),
                labels: None,
                driven_by: None,
                caller: caller.map(str::to_owned),
            })
        };
        assert_eq!(
            started(None),
            "agent sha256:abcdef0\u{2026} input \"ship it\""
        );
        assert_eq!(
            started(Some("ci")),
            "agent sha256:abcdef0\u{2026} input \"ship it\" [caller: ci]"
        );

        let resumed = |caller: Option<&str>| {
            event_detail(&Event::Resumed {
                input: json!({"approved": true}),
                caller: caller.map(str::to_owned),
            })
        };
        assert_eq!(resumed(None), r#"input {"approved":true}"#);
        assert_eq!(
            resumed(Some("ops")),
            r#"input {"approved":true} [caller: ops]"#
        );

        let abandoned = |caller: Option<&str>| {
            event_detail(&Event::RunAbandoned {
                reason: Some("husk is dead forever".into()),
                unresolved_write: None,
                caller: caller.map(str::to_owned),
            })
        };
        assert_eq!(abandoned(None), "abandoned: husk is dead forever");
        assert_eq!(
            abandoned(Some("ops")),
            "abandoned: husk is dead forever [caller: ops]"
        );

        let redriven = |caller: Option<&str>| {
            event_detail(&Event::RunRedriven {
                caller: caller.map(str::to_owned),
            })
        };
        assert_eq!(redriven(None), "redriven");
        assert_eq!(
            redriven(Some("server:wake")),
            "redriven [caller: server:wake]"
        );

        let graph = |caller: Option<&str>| {
            event_detail(&Event::GraphRunStarted {
                graph_hash: "sha256:abcdef0123456789".into(),
                input: json!("ship it"),
                labels: None,
                forked_from: None,
                caller: caller.map(str::to_owned),
            })
        };
        assert_eq!(
            graph(None),
            "graph sha256:abcdef0\u{2026} input \"ship it\""
        );
        assert_eq!(
            graph(Some("ci")),
            "graph sha256:abcdef0\u{2026} input \"ship it\" [caller: ci]"
        );
    }

    /// A long input is truncated, so a raw payload never reaches the stream in
    /// full: the detail line stays capped even for a large value.
    #[test]
    fn detail_truncates_long_payloads() {
        let big = "x".repeat(500);
        let detail = event_detail(&Event::RunStarted {
            agent_def_hash: "sha256:abcdef0123456789".into(),
            input: json!({ "prompt": big }),
            labels: None,
            driven_by: None,
            caller: None,
        });
        assert!(detail.contains('\u{2026}'), "detail should be truncated");
        assert!(
            detail.chars().count() < 200,
            "truncated detail stays short: {} chars",
            detail.chars().count()
        );
        // The short hash appears; the full 64-hex form does not.
        assert!(detail.contains("sha256:abcdef0"));
    }

    /// Every kind maps to its variant name, matching the wire tag.
    #[test]
    fn kind_matches_variant_name() {
        assert_eq!(
            event_kind(&Event::RunCompleted { output: json!(1) }),
            "RunCompleted"
        );
        assert_eq!(
            event_kind(&Event::RandomObserved { value: 7 }),
            "RandomObserved"
        );
    }

    /// The compatibility test: a `ToolCallRequested` with `performed_by: None`
    /// (the default, and every entry recorded before the field existed)
    /// renders EXACTLY as it did before this field's marker was added. The
    /// string below is the pinned pre-change output; the field
    /// deliberately reads no `performed_by` at all so a change to this test
    /// would only ever mean the compatibility case regressed.
    #[test]
    fn detail_omits_performer_marker_when_absent() {
        let event = Event::ToolCallRequested {
            seq: crate::id::SequenceNumber::new(3),
            tool: "refund_card".into(),
            input: json!({"amount_cents": 15900}),
            effect: crate::effect::Effect::Write,
            idempotency_key: Some("sha256:d2bb005d".into()),
            performed_by: None,
        };
        assert_eq!(
            event_detail(&event),
            r#"refund_card [Write] key sha256:d2bb005d input {"amount_cents":15900}"#
        );
    }

    /// A `ToolCallRequested` performed by the server, explicitly recorded as
    /// such rather than left absent, still renders no marker: the field's
    /// meaning is "who performed this", and a server-performed call is not
    /// noteworthy however it got recorded.
    #[test]
    fn detail_omits_performer_marker_for_explicit_server() {
        let event = Event::ToolCallRequested {
            seq: crate::id::SequenceNumber::new(3),
            tool: "refund_card".into(),
            input: json!({"amount_cents": 15900}),
            effect: crate::effect::Effect::Write,
            idempotency_key: Some("sha256:d2bb005d".into()),
            performed_by: Some(Performer::Server),
        };
        assert_eq!(
            event_detail(&event),
            r#"refund_card [Write] key sha256:d2bb005d input {"amount_cents":15900}"#
        );
    }

    /// A client-performed call gets the `[Client]` marker, placed right after
    /// the effect class it sits beside.
    #[test]
    fn detail_marks_a_client_performed_call() {
        let event = Event::ToolCallRequested {
            seq: crate::id::SequenceNumber::new(3),
            tool: "refund_card".into(),
            input: json!({"amount_cents": 15900}),
            effect: crate::effect::Effect::Write,
            idempotency_key: Some("sha256:d2bb005d".into()),
            performed_by: Some(Performer::Client),
        };
        assert_eq!(
            event_detail(&event),
            r#"refund_card [Write] [Client] key sha256:d2bb005d input {"amount_cents":15900}"#
        );
    }

    /// A model call salvor performed itself renders exactly as it always did:
    /// the request hash and nothing else. This is the compatibility pin for
    /// the model line, the twin of
    /// [`detail_omits_performer_marker_when_absent`].
    #[test]
    fn model_detail_omits_performer_marker_when_absent() {
        let event = Event::ModelCallRequested {
            seq: crate::id::SequenceNumber::new(3),
            request_hash: "sha256:abcdef0123456789".into(),
            request_body: None,
            performed_by: None,
        };
        assert_eq!(event_detail(&event), "request sha256:abcdef0\u{2026}");
    }

    /// A model call the CLIENT performed is marked the same way a
    /// client-performed tool call is, so a person reading a history reads one
    /// fact one way whichever kind of call carries it.
    #[test]
    fn model_detail_marks_a_client_performed_call() {
        let event = Event::ModelCallRequested {
            seq: crate::id::SequenceNumber::new(3),
            request_hash: "sha256:abcdef0123456789".into(),
            request_body: None,
            performed_by: Some(Performer::Client),
        };
        assert_eq!(
            event_detail(&event),
            "request sha256:abcdef0\u{2026} [Client]"
        );
    }
}
