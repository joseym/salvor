//! Turning runtime and store values into the text the CLI prints.
//!
//! Everything here is a pure function from a value to a `String`: no IO, no
//! store access, no clock. That keeps the formatting unit-testable in
//! isolation and keeps the command handlers ([`crate::commands`]) about
//! control flow rather than layout. Two output surfaces share this module:
//! the event detail line is reused by `history` (to stdout) and by `run`
//! progress (to the tracing log on stderr), so a tool call reads the same
//! way whether you watch a run or inspect it later.

use salvor_core::{BudgetKind, EventEnvelope, PendingCall, RunState, RunStatus};
use salvor_runtime::{ParkReason, event_detail, event_kind};
use salvor_store::RunSummary;
use serde_json::Value;
use std::path::Path;
use time::OffsetDateTime;

use crate::serve_kill::RunningServer;

/// One `history` line: sequence, recorded time, kind, and the detail. The
/// per-event `kind` and `detail` come from `salvor-runtime`, the same functions
/// that format the live progress stream, so a step reads identically whether
/// you watch it as it happens or inspect it here afterward.
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
/// [`RunStatus::NeedsReconciliation`](salvor_core::RunStatus::NeedsReconciliation).
///
/// It gives a human what they need to actually resolve the run: the full
/// recorded write intent (tool, pretty-printed input, idempotency key, seq,
/// and when it was recorded), a plain statement of what the write may have
/// done externally, and the two honest ways forward, each written out as the
/// exact command to type. `recorded_at` is the timestamp of the intent
/// envelope; the caller finds it in the log. Printed before a non-zero exit.
#[must_use]
pub fn reconciliation_report(
    run_uuid: &str,
    pending: Option<&PendingCall>,
    recorded_at: Option<OffsetDateTime>,
) -> String {
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
        let when = recorded_at.map_or_else(|| "<unknown>".to_owned(), format_ts);
        out.push_str(&format!(
            "\nThe recorded intent:\n  \
             seq:             {seq}\n  \
             recorded at:     {when}\n  \
             tool:            {tool}\n  \
             effect:          {effect:?}\n  \
             idempotency key: {key}\n  \
             input:\n{}\n",
            indent(&pretty_json(input), 4),
        ));
    }
    out.push_str(&format!(
        "\nBecause the intent was durably recorded before the tool ran, the write may have\n\
         reached its target, partially applied, or never run at all. Salvor will not guess.\n\
         \n\
         There are two honest outcomes. Both begin by verifying externally whether the write\n\
         took effect, and both end by recording the completion so replay never re-runs it:\n  \
         1. The write took effect. Record what the tool returned:\n       \
         salvor resolve {run_uuid} --output '<json the tool returned>'\n  \
         2. The write did not take effect and still needs to happen. Perform it yourself\n     \
         first, then record its result the same way. There is no automatic retry for a write.\n"
    ));
    out
}

/// The report `salvor resolve` prints once it has recorded the missing write
/// completion by hand: the run has left reconciliation and can be continued.
#[must_use]
pub fn resolved_report(run_uuid: &str) -> String {
    format!(
        "Run {run_uuid} resolved: recorded the missing write completion by hand.\n\
         The run no longer needs reconciliation. Continue it with:\n  \
         salvor resume {run_uuid} --agent <agent.toml>\n"
    )
}

/// The report `salvor abandon` prints once it has appended the terminal
/// `RunAbandoned` by hand. `appended_seq` is the position it landed at, and
/// `unresolved` is the outstanding write (seq, tool) when a needs-reconciliation
/// run was abandoned, so the receipt states plainly that the write stays
/// unresolved and is recorded as such. Nothing was edited or re-run.
#[must_use]
pub fn abandoned_report(
    run_uuid: &str,
    appended_seq: u64,
    unresolved: Option<(u64, &str)>,
) -> String {
    let mut out = format!(
        "Run {run_uuid}: appended RunAbandoned at seq {appended_seq}. Status now abandoned.\n\
         Nothing was edited or re-run; the run is retired.\n"
    );
    if let Some((seq, tool)) = unresolved {
        out.push_str(&format!(
            "The write at seq {seq} ({tool}) stays unresolved and is recorded as such; \
             its effect remains unknown.\n"
        ));
    }
    out
}

/// Every label [`status_label`] can print, which is also every value `salvor list --status`
/// accepts and offers for completion.
///
/// Kept beside `status_label` and `status_group` because the three have to agree: a label the
/// column can print but the filter rejects would be a state you can see and cannot select. A test
/// asserts this list and `status_group` recognise exactly the same set.
pub const STATUS_LABELS: [&str; 10] = [
    "not-started",
    "running",
    "awaiting-model",
    "awaiting-tool",
    "suspended",
    "budget-exceeded",
    "needs-reconciliation",
    "completed",
    "failed",
    "abandoned",
];

/// What a status asks of the reader. Scanning a long list, the question is never "which state is
/// this" but "does anything here need me", and these are the three answers.
///
/// The same three the web UI names, so an operator reading both surfaces does not learn two
/// vocabularies, and the same three `salvor list --group` filters on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusGroup {
    /// Stopped, and it will not move again until a person does something.
    Waiting,
    /// Moving on its own; nothing to do but wait.
    Progress,
    /// Finished, one way or another.
    Terminal,
}

impl StatusGroup {
    /// The name this group answers to on the command line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Progress => "progress",
            Self::Terminal => "terminal",
        }
    }
}

/// The group a status label belongs to, or `None` for a label this build does not recognise —
/// which a future status would be, until someone teaches this function about it.
#[must_use]
pub fn status_group(status: &str) -> Option<StatusGroup> {
    match status {
        "suspended" | "needs-reconciliation" | "budget-exceeded" => Some(StatusGroup::Waiting),
        "running" | "awaiting-model" | "awaiting-tool" => Some(StatusGroup::Progress),
        "completed" | "failed" | "abandoned" | "not-started" => Some(StatusGroup::Terminal),
        _ => None,
    }
}

/// The colour a status earns. Group decides the hue, so `--group waiting` returns exactly the rows
/// that read yellow; within the terminal group the outcome still matters, because "finished" and
/// "failed" are not the same news.
fn status_style(status: &str) -> anstyle::Style {
    use anstyle::AnsiColor;

    let color = match (status_group(status), status) {
        // Yellow because this group is the only one where the list is a to-do list.
        (Some(StatusGroup::Waiting), _) => AnsiColor::Yellow,
        (Some(StatusGroup::Progress), _) => AnsiColor::Cyan,
        (Some(StatusGroup::Terminal), "completed") => AnsiColor::Green,
        (Some(StatusGroup::Terminal), "failed") => AnsiColor::Red,
        // Terminal by choice, or never begun: present, and deliberately quiet.
        (Some(StatusGroup::Terminal), _) => return anstyle::Style::new().dimmed(),
        (None, _) => return anstyle::Style::new(),
    };
    anstyle::Style::new().fg_color(Some(color.into()))
}

/// The `list` table: a header plus one row per run. `rows` pairs each summary
/// with its derived status label (the store does not carry status; it is a
/// replay-time projection, so the caller folds each log first).
///
/// The status cell carries ANSI styling unconditionally. Stripping it is the writer's job, not
/// this function's: the caller prints through an [`anstream`] stream, which removes the codes when
/// stdout is not a terminal and honours `NO_COLOR`. That keeps `salvor list | grep completed`
/// working and keeps this function's output a pure function of its input.
#[must_use]
pub fn list_table(rows: &[(RunSummary, String)]) -> String {
    let header = anstyle::Style::new().bold();
    let mut out = format!(
        "{header}{:<36}  {:<20}  {:>6}  {:<20}  {:<20}{header:#}\n",
        "RUN ID",
        "STATUS",
        "EVENTS",
        "STARTED",
        "LAST ACTIVITY",
        header = header,
    );
    for (summary, status) in rows {
        // Pad BEFORE styling: escape codes are zero-width on screen but count as characters to
        // `{:<20}`, so a styled-then-padded cell would shear the columns to the right of it.
        let style = status_style(status);
        let padded = format!("{status:<20}");
        out.push_str(&format!(
            "{:<36}  {style}{padded}{style:#}  {:>6}  {:<20}  {:<20}\n",
            summary.run_id.as_uuid(),
            summary.event_count,
            format_ts(summary.first_recorded_at),
            format_ts(summary.last_recorded_at),
            style = style,
            padded = padded,
        ));
    }
    out
}

/// The `salvor serve --kill` table: one numbered row per discovered `salvor
/// serve` process, so an operator picking one at the prompt can name it by
/// number, pid, or port.
#[must_use]
pub fn server_table(servers: &[RunningServer]) -> String {
    let mut out = format!("{:>3}  {:<8}  {:<21}  {}\n", "#", "PID", "BIND", "STORE");
    for (index, server) in servers.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}  {:<8}  {:<21}  {}\n",
            index + 1,
            server.pid,
            server.bind,
            server.store,
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

/// The success report for `graph validate`: the node and edge counts and the
/// entry (no inbound) and terminal (no outbound) node ids. Pure formatting of a
/// [`salvor_graph::GraphSummary`]; a validation failure is printed by the
/// handler, not here.
#[must_use]
pub fn graph_summary(summary: &salvor_graph::GraphSummary) -> String {
    format!(
        "graph ok: {} node(s), {} edge(s)\n\
         entry:    {}\n\
         terminal: {}\n",
        summary.node_count,
        summary.edge_count,
        join_ids(&summary.entry_nodes),
        join_ids(&summary.terminal_nodes),
    )
}

/// Joins node ids for the summary, or "(none)" when the list is empty.
fn join_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "(none)".to_owned()
    } else {
        ids.join(", ")
    }
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
        RunStatus::Abandoned { .. } => "abandoned",
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

/// Indents every line of `text` by `spaces`, for nesting a pretty JSON block
/// under a labeled heading.
fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strips ANSI escape sequences, so a test can measure what a reader actually sees rather than
    /// what was written. Deliberately naive: it only needs to handle the CSI sequences anstyle
    /// emits here, not the full grammar.
    fn visible(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The grouping is the contract, not the individual colours: a reader scans for "does this need
    /// me", so anything waiting on a person must be visually distinct from anything still moving,
    /// and both from the two terminal outcomes.
    #[test]
    fn statuses_are_coloured_by_what_they_ask_of_the_reader() {
        let waiting: Vec<_> = ["suspended", "needs-reconciliation", "budget-exceeded"]
            .iter()
            .map(|s| status_style(s))
            .collect();
        let moving: Vec<_> = ["running", "awaiting-model", "awaiting-tool"]
            .iter()
            .map(|s| status_style(s))
            .collect();

        assert!(
            waiting.windows(2).all(|pair| pair[0] == pair[1]),
            "every waiting-on-a-human status shares one colour"
        );
        assert!(
            moving.windows(2).all(|pair| pair[0] == pair[1]),
            "every in-progress status shares one colour"
        );
        assert_ne!(waiting[0], moving[0], "waiting must not look like moving");
        assert_ne!(
            status_style("completed"),
            status_style("failed"),
            "the two terminal outcomes must not look alike"
        );
        assert_eq!(
            status_style("something-new-we-added-later"),
            anstyle::Style::new(),
            "an unrecognised status renders unstyled rather than miscoloured"
        );
    }

    /// `--group waiting` is documented as returning exactly the rows that read yellow. That is only
    /// true while both derive from `status_group`, so this asserts the pair cannot drift: every
    /// status in a group renders identically, and no two groups share a rendering.
    #[test]
    fn the_filter_groups_and_the_colours_are_the_same_partition() {
        let every_status = [
            "completed",
            "failed",
            "abandoned",
            "not-started",
            "running",
            "awaiting-model",
            "awaiting-tool",
            "suspended",
            "needs-reconciliation",
            "budget-exceeded",
        ];
        assert_eq!(
            every_status.len(),
            STATUS_LABELS.len(),
            "this test enumerates the labels; STATUS_LABELS is what the CLI offers, and a label in \
             one but not the other is a state you can see but cannot filter for"
        );
        for status in STATUS_LABELS {
            assert!(
                every_status.contains(&status),
                "{status} is offered by --status, so this test must cover it"
            );
        }
        for status in every_status {
            assert!(
                status_group(status).is_some(),
                "{status} is printed by the STATUS column, so it must belong to a group or \
                 `--group` silently drops it"
            );
        }

        let waiting: Vec<_> = every_status
            .iter()
            .filter(|s| status_group(s) == Some(StatusGroup::Waiting))
            .map(|s| status_style(s))
            .collect();
        assert!(
            waiting.windows(2).all(|pair| pair[0] == pair[1]),
            "the waiting group renders as one colour, so `--group waiting` selects one colour"
        );

        let progress: Vec<_> = every_status
            .iter()
            .filter(|s| status_group(s) == Some(StatusGroup::Progress))
            .map(|s| status_style(s))
            .collect();
        assert!(progress.windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(
            waiting[0], progress[0],
            "a reader must not have to check the label to tell the two apart"
        );
    }

    /// Escape codes are zero-width on screen but count toward `{:<20}`, so styling a cell before
    /// padding it shears every column to its right. This is the regression that would produce.
    #[test]
    fn styling_does_not_disturb_the_column_widths() {
        let padded = format!("{:<20}", "completed");
        let row = format!(
            "{:<36}  {style}{padded}{style:#}  {:>6}\n",
            "run-id",
            42,
            style = status_style("completed"),
        );
        let plain = format!("{:<36}  {:<20}  {:>6}\n", "run-id", "completed", 42);
        assert_eq!(
            visible(&row),
            plain,
            "with the styling stripped, a styled row is byte-identical to an unstyled one"
        );
    }
}
