//! Live progress: turning each persisted event into a one-line `tracing`
//! record, emitted the instant the event becomes durable.
//!
//! Live emission replaces walking the log *after* a drive returned: the
//! runtime is the only layer that knows a step
//! happened as it happens, so it is the layer that emits the step. Every
//! [`crate::RunCtx`] persist calls [`emit_step`] right after the store append
//! succeeds; a subscriber installed by the binary (the CLI writes it to
//! stderr) renders the record. Replayed events are answered from the log and
//! never re-persisted, so they never re-emit: a resumed or recovered run
//! streams only its genuinely new activity.
//!
//! # What is emitted, and what is withheld
//!
//! Each record carries the correlation fields an operator needs (`run_id` and
//! `seq`) and a scannable message of the form `"<kind> <detail>"`. The detail
//! is the same one-line summary the `history` command prints, produced by the
//! shared [`event_kind`] / [`event_detail`] functions so a step reads the
//! same whether you watch it live or inspect it later.
//!
//! The detail deliberately does **not** carry full payloads. Inputs, outputs,
//! resume values, and error messages are truncated (see [`truncate_str`]) so
//! a model's raw output or a tool's raw arguments never land in the progress
//! stream in full. The untruncated form lives only in the event log, reachable
//! through `salvor history --json`.

use salvor_core::{BudgetKind, Event, RunId, SequenceNumber};
use time::OffsetDateTime;

use crate::wire::{decode_failure, decode_suspension};

/// Emits one info-level progress record for a freshly persisted event.
///
/// Called from the [`crate::RunCtx`] IO edge the moment an append returns
/// `Ok`, so the record reaches the subscriber while the run is still driving.
/// The `run_id` and `seq` fields correlate the line to its place in the log;
/// the message is `"<kind> <detail>"` with the payload truncated.
pub(crate) fn emit_step(run_id: RunId, seq: SequenceNumber, event: &Event) {
    tracing::info!(
        run_id = %run_id.as_uuid(),
        seq = seq.get(),
        "{} {}",
        event_kind(event),
        event_detail(event),
    );
}

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
        Event::BudgetExceeded { .. } => "BudgetExceeded",
        Event::RunCompleted { .. } => "RunCompleted",
        Event::RunFailed { .. } => "RunFailed",
    }
}

/// The informative payload of one event, rendered as a single line. Picks the
/// fields that matter per kind: a tool call shows its name and effect, a model
/// completion its token usage, a suspension its reason. Hashes are shortened
/// and every payload is truncated, so no full input, output, or error text
/// reaches the progress stream; `salvor history --json` is the escape hatch for
/// the untruncated envelope.
#[must_use]
pub fn event_detail(event: &Event) -> String {
    match event {
        Event::RunStarted {
            agent_def_hash,
            input,
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
            ..
        } => {
            let key = idempotency_key
                .as_deref()
                .map_or_else(String::new, |k| format!(" key {k}"));
            format!("{tool} [{effect:?}]{key} input {}", truncate_json(input))
        }
        Event::ToolCallCompleted { output, .. } => {
            if let Some(suspension) = decode_suspension(output) {
                format!("suspends: {}", suspension.reason)
            } else if let Some(failure) = decode_failure(output) {
                format!(
                    "error ({}, {} attempt(s)): {}",
                    failure.kind.as_str(),
                    failure.attempts,
                    truncate_str(&failure.message)
                )
            } else {
                format!("output {}", truncate_json(output))
            }
        }
        Event::NowObserved { now } => format_ts(*now),
        Event::RandomObserved { value } => format!("value {value}"),
        Event::Suspended { reason, .. } => format!("reason: {reason}"),
        Event::Resumed { input } => format!("input {}", truncate_json(input)),
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

    /// A long input is truncated, so a raw payload never reaches the stream in
    /// full: the detail line stays capped even for a large value.
    #[test]
    fn detail_truncates_long_payloads() {
        let big = "x".repeat(500);
        let detail = event_detail(&Event::RunStarted {
            agent_def_hash: "sha256:abcdef0123456789".into(),
            input: json!({ "prompt": big }),
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
}
