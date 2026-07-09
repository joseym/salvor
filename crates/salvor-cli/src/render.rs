//! Turning runtime and store values into the text the CLI prints.
//!
//! Everything here is a pure function from a value to a `String`: no IO, no
//! store access, no clock. That keeps the formatting unit-testable in
//! isolation and keeps the command handlers ([`crate::commands`]) about
//! control flow rather than layout. Two output surfaces share this module:
//! the event detail line is reused by `history` (to stdout) and by `run`
//! progress (to the tracing log on stderr), so a tool call reads the same
//! way whether you watch a run or inspect it later.

use salvor_core::{BudgetKind, Event, EventEnvelope, PendingCall, RunState, RunStatus};
use salvor_runtime::{ParkReason, decode_failure, decode_suspension};
use salvor_store::RunSummary;
use serde_json::Value;
use std::path::Path;
use time::OffsetDateTime;

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
/// and payloads truncated so the line stays scannable; the `--json` mode of
/// `history` is the escape hatch for the untruncated envelope.
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
                budget_kind(budget.kind),
                fmt_num(budget.limit),
                fmt_num(*observed)
            )
        }
        Event::RunCompleted { output } => format!("output {}", truncate_json(output)),
        Event::RunFailed { error } => format!("error: {}", truncate_str(error)),
    }
}

/// One `history` line: sequence, recorded time, kind, and the detail.
#[must_use]
pub fn history_line(envelope: &EventEnvelope) -> String {
    format!(
        "{:>4}  {}  {:<19}  {}",
        envelope.seq.get(),
        format_ts(envelope.recorded_at),
        event_kind(&envelope.event),
        event_detail(&envelope.event),
    )
}

/// The parked report a `run` (or a parking `resume`) prints: why the run
/// parked and the exact command to type to continue it. Non-error output: a
/// parked run is a success, not a failure.
#[must_use]
pub fn parked_report(run_uuid: &str, reason: &ParkReason, agent_path: &Path) -> String {
    let agent = agent_path.display();
    match reason {
        ParkReason::Suspended {
            reason,
            input_schema,
        } => format!(
            "Run {run_uuid} parked: suspended.\n  \
             reason: {reason}\n  \
             the resume input must satisfy this schema:\n{}\n\
             Resume once you have the input:\n  \
             salvor resume {run_uuid} --agent {agent} --input @resume.json\n",
            indent(&pretty_json(input_schema), 4),
        ),
        ParkReason::BudgetExceeded { budget, observed } => {
            let kind = budget_kind(budget.kind);
            let extend_key = extend_key(budget.kind);
            format!(
                "Run {run_uuid} parked: budget exceeded ({kind}).\n  \
                 limit:    {}\n  \
                 observed: {}\n\
                 Raise the limit and resume:\n  \
                 salvor resume {run_uuid} --agent {agent} --input '{{\"extend\": {{\"{extend_key}\": <more>}}}}'\n",
                fmt_num(budget.limit),
                fmt_num(*observed),
            )
        }
    }
}

/// The refusal report for a run that derived to
/// [`RunStatus::NeedsReconciliation`](salvor_core::RunStatus::NeedsReconciliation):
/// the recorded write intent, shown as the evidence a human needs to decide
/// whether the write reached its target. Printed before a non-zero exit.
#[must_use]
pub fn reconciliation_report(run_uuid: &str, pending: Option<&PendingCall>) -> String {
    let mut out = format!(
        "Run {run_uuid} needs reconciliation and cannot be resumed automatically.\n\
         A write tool call was recorded but never completed, so it may or may not have taken effect.\n"
    );
    if let Some(PendingCall::Tool {
        seq,
        tool,
        input,
        effect,
        idempotency_key,
    }) = pending
    {
        let key = idempotency_key.as_deref().unwrap_or("<none>");
        out.push_str(&format!(
            "  seq:             {seq}\n  \
             tool:            {tool}\n  \
             effect:          {effect:?}\n  \
             input:           {}\n  \
             idempotency key: {key}\n",
            pretty_json(input),
        ));
    }
    out.push_str(
        "A human must decide whether this write reached its target; only then can the run continue.\n",
    );
    out
}

/// The `list` table: a header plus one row per run. `rows` pairs each summary
/// with its derived status label (the store does not carry status; it is a
/// replay-time projection, so the caller folds each log first).
#[must_use]
pub fn list_table(rows: &[(RunSummary, String)]) -> String {
    let mut out = format!(
        "{:<36}  {:<20}  {:>6}  {:<20}  {:<20}\n",
        "RUN ID", "STATUS", "EVENTS", "STARTED", "LAST ACTIVITY"
    );
    for (summary, status) in rows {
        out.push_str(&format!(
            "{:<36}  {:<20}  {:>6}  {:<20}  {:<20}\n",
            summary.run_id.as_uuid(),
            status,
            summary.event_count,
            format_ts(summary.first_recorded_at),
            format_ts(summary.last_recorded_at),
        ));
    }
    out
}

/// The `replay --dry-run` summary: the state a log folds to, without executing
/// anything. Names the status, the next sequence position, the accumulated
/// token usage, and any dangling call intent.
#[must_use]
pub fn replay_summary(state: &RunState) -> String {
    let mut out = format!(
        "status:      {}\n\
         next seq:    {}\n\
         usage:       in {} tokens, out {} tokens\n",
        status_label(&state.status),
        state.next_seq,
        state.usage.input_tokens,
        state.usage.output_tokens,
    );
    match &state.pending_call {
        None => out.push_str("pending:     none\n"),
        Some(PendingCall::Model { seq, request_hash }) => out.push_str(&format!(
            "pending:     model call at seq {seq} (request {})\n",
            short_hash(request_hash)
        )),
        Some(PendingCall::Tool {
            seq, tool, effect, ..
        }) => out.push_str(&format!(
            "pending:     tool `{tool}` [{effect:?}] at seq {seq}\n"
        )),
    }
    out
}

/// A short, human status word for a run, for the `list` table and the replay
/// summary. Terminal payloads are elided here; `history`/`replay` show them.
#[must_use]
pub fn status_label(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::NotStarted => "not-started",
        RunStatus::Running => "running",
        RunStatus::AwaitingModel => "awaiting-model",
        RunStatus::AwaitingTool => "awaiting-tool",
        RunStatus::Suspended { .. } => "suspended",
        RunStatus::BudgetExceeded { .. } => "budget-exceeded",
        RunStatus::NeedsReconciliation => "needs-reconciliation",
        RunStatus::Completed { .. } => "completed",
        RunStatus::Failed { .. } => "failed",
    }
}

/// Pretty-prints a JSON value over multiple lines. Used where a value is worth
/// reading in full (a suspension schema, a reconciliation input).
#[must_use]
pub fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Shortens a `sha256:...` hash to its prefix and the first seven hex digits,
/// so a log line names a request without a 64-character wall of hex.
#[must_use]
pub fn short_hash(hash: &str) -> String {
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

/// The extension key a budget crossing suggests in its resume command, so the
/// printed command matches the shape [`salvor_runtime::validate_extension_input`]
/// accepts for that dimension.
fn extend_key(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "steps",
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost_usd",
        BudgetKind::WallTime => "wall_time_seconds",
    }
}

/// A human word for a budget dimension.
fn budget_kind(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "steps",
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost_usd",
        BudgetKind::WallTime => "wall_time",
    }
}

/// Formats an `f64` budget figure without a needless `.0` when it is integral,
/// since steps and tokens are whole numbers on the wire even though the event
/// carries every budget dimension as a float.
fn fmt_num(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Formats a timestamp as `YYYY-MM-DD HH:MM:SSZ` from its components, avoiding
/// a dependency on the `time` crate's optional `formatting` feature so the
/// change stays contained to this crate.
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

/// Compact one-line JSON, truncated so a payload never blows out a log line.
fn truncate_json(value: &Value) -> String {
    truncate_str(&value.to_string())
}

/// Truncates a string to a scannable length with an ellipsis.
fn truncate_str(text: &str) -> String {
    const CAP: usize = 80;
    if text.chars().count() > CAP {
        let head: String = text.chars().take(CAP).collect();
        format!("{head}\u{2026}")
    } else {
        text.to_owned()
    }
}

/// Indents every line of `text` by `spaces`, for nesting a pretty JSON block
/// under a labeled heading.
fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
