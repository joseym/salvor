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

use crate::event::{BudgetKind, Event, Performer};
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
            ..
        } => format!(
            "agent {} input {}",
            short_hash(agent_def_hash),
            truncate_json(input)
        ),
        Event::ModelCallRequested { request_hash, .. } => {
            format!("request {}", short_hash(request_hash))
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
            // Absent (the field's default, and every entry recorded before it
            // existed) means salvor performed the call itself: the
            // overwhelmingly common case, so it renders nothing. Only a
            // recorded `Performer::Client` gets a marker, in the same
            // bracketed register as the effect class beside it.
            let performer = match performed_by {
                Some(Performer::Client) => " [Client]",
                None | Some(Performer::Server) => "",
            };
            format!(
                "{tool} [{effect:?}]{performer}{key} input {}",
                truncate_json(input)
            )
        }
        Event::ToolCallCompleted {
            output,
            deduplicated_from,
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
            if let Some(reason) = suspension_reason(output) {
                format!("suspends: {reason}{copied}")
            } else if let Some(failure) = recorded_failure(output) {
                format!(
                    "error ({}, {} attempt(s)): {}{copied}",
                    failure.kind,
                    failure.attempts,
                    truncate_str(failure.message)
                )
            } else {
                format!("output {}{copied}", truncate_json(output))
            }
        }
        Event::NowObserved { now } => format_ts(*now),
        Event::RandomObserved { value } => format!("value {value}"),
        Event::Suspended { reason, .. } => format!("reason: {reason}"),
        Event::Resumed { input } => format!("input {}", truncate_json(input)),
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
        } => {
            let why = reason
                .as_deref()
                .map_or_else(|| "no reason given".to_owned(), truncate_str);
            match unresolved_write {
                Some(write) => format!(
                    "abandoned: {why} (unresolved write at seq {}, tool {})",
                    write.seq.get(),
                    write.tool
                ),
                None => format!("abandoned: {why}"),
            }
        }
        Event::GraphRunStarted {
            graph_hash, input, ..
        } => format!(
            "graph {} input {}",
            short_hash(graph_hash),
            truncate_json(input)
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
        });
        assert_eq!(executed, r#"output {"charge_id":"po_1"}"#);

        let copied = event_detail(&Event::ToolCallCompleted {
            seq: SequenceNumber::new(1),
            output: json!({"charge_id": "po_1"}),
            deduplicated_from: Some(origin),
        });
        assert_eq!(
            copied,
            r#"output {"charge_id":"po_1"} (deduplicated: copied from run 00000000-0000-4000-8000-0000000000aa seq 4)"#
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
}
