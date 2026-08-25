//! Turning runtime and store values into the text the CLI prints.
//!
//! Everything here is a pure function from a value to a `String`: no IO, no
//! store access, no clock. That keeps the formatting unit-testable in
//! isolation and keeps the command handlers (`salvor_cli::commands`) about
//! control flow rather than layout. Two output surfaces share this module:
//! the event detail line is reused by `history` (to stdout) and by `run`
//! progress (to the tracing log on stderr), so a tool call reads the same
//! way whether you watch a run or inspect it later.

use salvor_replay::{
    BudgetKind, EventEnvelope, ParkReason, PendingCall, RunState, RunStatus, RunSummary,
    event_detail, event_kind,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// One `history` line: sequence, recorded time, kind, and the detail. The
/// per-event `kind` and `detail` come from `salvor-replay`, the same functions
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

/// The default column width the `salvor` binary passes to the report
/// functions below.
///
/// 80 is what a terminal is unless someone has resized it, and that is the
/// number that matters here: a line longer than the terminal is wrapped again
/// by the terminal itself, at whatever column the window happens to end,
/// which turns one deliberate break into a ragged one nobody chose. Wrapping
/// at 80 keeps every break ours. The hand-typed text this replaced ran to
/// about 90 columns and did double-wrap on a default terminal.
///
/// A caller that knows its own width, such as a browser terminal sized to its
/// container, passes that instead of this constant.
pub const DEFAULT_REPORT_WIDTH: usize = 80;

/// Wraps `text` onto lines no wider than `width` columns, breaking only at
/// whitespace so a word is never split. `first_prefix` opens the first line
/// and `rest_prefix` opens every line after it; both count toward `width`,
/// which is how a numbered list item gets a hanging indent that lines its
/// continuation up under its own text rather than under the number. A single
/// word that does not fit after its prefix is still placed on that line
/// rather than split, since a broken word reads worse than a long line.
///
/// This is the only function in this module that reflows text, and
/// `crate::graph_editor` shares it rather than growing a second wrapper, so
/// every surface in this crate breaks lines by one rule. A report
/// function calls it exactly on the spans meant to read as paragraphs:
/// headings and list-item prose. A command line, the aligned key/value
/// block, and pretty-printed JSON are written straight into the output and
/// never passed through here, which is the structural reason a command a
/// reader copies, or a recorded field's alignment, cannot end up broken
/// across lines: nothing in the report functions below hands those spans to
/// this function.
#[must_use]
pub(crate) fn wrap(text: &str, width: usize, first_prefix: &str, rest_prefix: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line = first_prefix.to_owned();
    let mut prefix_len = first_prefix.len();

    for word in text.split_whitespace() {
        let has_content = line.len() > prefix_len;
        if has_content && line.len() + 1 + word.len() > width {
            lines.push(line);
            line = rest_prefix.to_owned();
            prefix_len = rest_prefix.len();
        } else if has_content {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines.join("\n")
}

/// The `--store <path>` flag for a `resume` or `wake` hint, right after the
/// verb it drives, the way the printed command already carries `--agent`
/// after it names the file.
///
/// `store` is `None` when the caller has no resolved store path to print,
/// which the native CLI always does (it hands over `cli.store`, already
/// resolved from the flag, `SALVOR_STORE`, or the default) but a caller with
/// no store of its own, such as a browser terminal rendering this text with
/// no SQLite file open, cannot supply. `<STORE>` stands in then, the same way
/// `<FILE>` stands in for an agent path nobody gave.
fn store_flag(store: Option<&Path>) -> String {
    match store {
        Some(path) => format!(" --store {}", path.display()),
        None => " --store <STORE>".to_owned(),
    }
}

/// The parked report a `run` (or a parking `resume`) prints: why the run
/// parked and the exact command to type to continue it. Non-error output: a
/// parked run is a success, not a failure. `store` is the resolved store path
/// to print in the `resume`/`wake` hint (or `None` for the `<STORE>`
/// placeholder, see [`store_flag`]). `width` is the column count its prose
/// wraps to; the command line never wraps, see [`wrap`].
///
/// A suspension's recorded kind decides who the report addresses. A gate
/// tells the reader to resume once they have the input, because they are the
/// one who supplies it. A signal says the run is waiting on something else
/// and the reader owes it nothing, while still printing the resume command,
/// which is how an operator stands in for a webhook that never arrived.
#[must_use]
pub fn parked_report(
    run_uuid: &str,
    reason: &ParkReason,
    agent_path: &Path,
    store: Option<&Path>,
    width: usize,
) -> String {
    let agent = agent_path.display();
    let store = store_flag(store);
    match reason {
        // Both suspensions park the same way and resume through the same
        // command, so the schema block and the hint are shared. What the
        // recorded kind changes is who the report is addressed to: a gate is
        // the reader's to answer, and a signal is an external system's. Saying
        // "suspended, resume once you have the input" about a webhook wait
        // hands the reader a job that is not theirs.
        ParkReason::Suspended {
            reason,
            input_schema,
            kind,
        } => {
            let awaits_a_person = kind.is_none();
            let headline = if awaits_a_person {
                format!("Run {run_uuid} parked: suspended.")
            } else {
                format!("Run {run_uuid} parked: awaiting a signal.")
            };
            let mut out = wrap(&headline, width, "", "");
            out.push_str("\n  reason: ");
            out.push_str(reason);
            out.push('\n');
            out.push_str(&wrap(
                if awaits_a_person {
                    "the resume input must satisfy this schema:"
                } else {
                    "the payload that resumes it must satisfy this schema:"
                },
                width,
                "  ",
                "  ",
            ));
            out.push('\n');
            out.push_str(&indent(&pretty_json(input_schema), 4));
            out.push('\n');
            out.push_str(&wrap(
                if awaits_a_person {
                    "Resume once you have the input:"
                } else {
                    "Nothing is waiting on you: whatever the run is waiting on delivers that \
                     payload and the run continues. Resume by hand only to stand in for it:"
                },
                width,
                "",
                "",
            ));
            out.push_str(&format!(
                "\n  salvor resume {run_uuid}{store} --agent {agent} --input @resume.json\n"
            ));
            out
        }
        ParkReason::BudgetExceeded { budget, observed } => {
            let kind = budget_kind(budget.kind);
            let extend_key = extend_key(budget.kind);
            let mut out = wrap(
                &format!("Run {run_uuid} parked: budget exceeded ({kind})."),
                width,
                "",
                "",
            );
            out.push_str(&format!(
                "\n  limit:    {}\n  observed: {}\n",
                fmt_num(budget.limit),
                fmt_num(*observed),
            ));
            out.push_str(&wrap("Raise the limit and resume:", width, "", ""));
            out.push_str(&format!(
                "\n  salvor resume {run_uuid}{store} --agent {agent} --input '{{\"extend\": {{\"{extend_key}\": <more>}}}}'\n"
            ));
            out
        }
        // The one park that asks nothing of the reader. It carries no schema
        // and no limit to raise, so the report names the instant and the
        // command that drives whatever is due, with no `--input` on it: a
        // sleeping run takes none.
        ParkReason::Sleeping { wake_at } => {
            let mut out = wrap(
                &format!(
                    "Run {run_uuid} parked: sleeping until {}.",
                    format_ts(*wake_at)
                ),
                width,
                "",
                "",
            );
            out.push('\n');
            out.push_str(&wrap(
                "Nothing is waiting on you. The run continues once its deadline passes and \
                 something re-drives it:",
                width,
                "",
                "",
            ));
            out.push_str(&format!("\n  salvor wake{store} --agent {agent}\n"));
            out
        }
    }
}

/// The refusal report for a `resume` of a run that is still sleeping: the
/// deadline it is waiting for and how much of the wait is left.
///
/// A sleeping run is not refused because anything is wrong with it, so this
/// says what the reader has to do about it, which is nothing. `remaining` is
/// `wake_at` less the caller's clock, formatted by the same
/// [`format_duration`] the wake preview uses for "overdue by", so one span
/// reads one way across the CLI. `agents` and `graph` are the values `resume`
/// itself was given, printed into the `wake` command so it is the real one to
/// type, with a placeholder standing in for anything the operator did not
/// supply (the same rule [`resolved_report`] follows). `store` is the
/// resolved store path to print right after the verb (or `None` for the
/// `<STORE>` placeholder, see [`store_flag`]). Printed before a non-zero
/// exit. `width` is the column count its prose wraps to; the command line
/// never wraps, see [`wrap`].
#[must_use]
pub fn sleeping_report(
    run_uuid: &str,
    wake_at: OffsetDateTime,
    remaining: time::Duration,
    agents: &[PathBuf],
    graph: Option<&Path>,
    store: Option<&Path>,
    width: usize,
) -> String {
    let mut out = wrap(
        &format!(
            "Run {run_uuid} is sleeping until {} and will not resume for another {}. It is not \
             parked on you: a sleeping run takes no input, and driving it early records nothing.",
            format_ts(wake_at),
            format_duration(remaining),
        ),
        width,
        "",
        "",
    );
    out.push('\n');
    out.push_str(&wrap(
        "Wait for the deadline, then drive whatever is due:",
        width,
        "",
        "",
    ));
    let mut command = format!("salvor wake{}", store_flag(store));
    if let Some(graph) = graph {
        command.push_str(&format!(" --graph {}", graph.display()));
    }
    if agents.is_empty() {
        command.push_str(" --agent <FILE>");
    } else {
        for agent in agents {
            command.push_str(&format!(" --agent {}", agent.display()));
        }
    }
    out.push_str(&format!("\n  {command}\n"));
    out
}

/// The refusal report for a run that derived to
/// [`RunStatus::NeedsReconciliation`](salvor_replay::RunStatus::NeedsReconciliation).
///
/// It gives a human what they need to actually resolve the run: the full
/// recorded write intent (tool, pretty-printed input, idempotency key, seq,
/// and when it was recorded), a plain statement of what the write may have
/// done externally, and the two honest ways forward, each written out as the
/// exact command to type. `recorded_at` is the timestamp of the intent
/// envelope; the caller finds it in the log. Printed before a non-zero exit.
/// `width` is the column count its prose wraps to; the two command lines and
/// the recorded-intent block never wrap, see [`wrap`].
#[must_use]
pub fn reconciliation_report(
    run_uuid: &str,
    pending: Option<&PendingCall>,
    recorded_at: Option<OffsetDateTime>,
    width: usize,
) -> String {
    let mut out = wrap(
        &format!(
            "Run {run_uuid} needs reconciliation and cannot be resumed automatically. A write \
             tool call was recorded but never completed, so it may or may not have taken effect."
        ),
        width,
        "",
        "",
    );
    out.push('\n');
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
        out.push('\n');
        out.push_str(&wrap("The recorded intent:", width, "", ""));
        out.push_str(&format!(
            "\n  seq:             {seq}\n  \
             recorded at:     {when}\n  \
             tool:            {tool}\n  \
             effect:          {effect:?}\n  \
             idempotency key: {key}\n  \
             input:\n{}\n",
            indent(&pretty_json(input), 4),
        ));
    }
    out.push('\n');
    out.push_str(&wrap(
        "Because the intent was durably recorded before the tool ran, the write may have \
         reached its target, partially applied, or never run at all. Salvor will not guess.",
        width,
        "",
        "",
    ));
    out.push_str("\n\n");
    out.push_str(&wrap(
        "There are two honest outcomes. Both begin by verifying externally whether the write \
         took effect, and both end by recording the completion so replay never re-runs it:",
        width,
        "",
        "",
    ));
    out.push('\n');
    out.push_str(&wrap(
        "The write took effect. Record what the tool returned:",
        width,
        "  1. ",
        "     ",
    ));
    out.push_str(&format!(
        "\n       salvor resolve {run_uuid} --output '<json the tool returned>'\n"
    ));
    out.push_str(&wrap(
        "The write did not take effect and still needs to happen. Perform it yourself first, \
         then record its result the same way. There is no automatic retry for a write.",
        width,
        "  2. ",
        "     ",
    ));
    out.push('\n');
    out
}

/// The report `salvor resolve` prints once it has recorded the missing write
/// completion by hand: the run has left reconciliation and can be continued.
/// `width` is the column count its prose wraps to; the command line never
/// wraps, see [`wrap`].
///
/// `agents` and `graph` are the `--agent`/`--graph` values `resolve` itself
/// was given (see [`crate::cli::ResolveArgs`]); when the operator supplied
/// them, the printed command is the real, complete one, exactly as a graph
/// run's own parked report is (see `report_graph_outcome` in
/// `salvor_cli::commands`). `graph_run` is whether the resolved run itself is
/// a graph run, which the caller reads off the run's own log (its head event
/// is `GraphRunStarted`) rather than off `graph.is_some()`, because an
/// operator can resolve a graph run without happening to pass `--graph`; that
/// is what lets the printed command still hint at a `--graph <FILE>`
/// placeholder in that case instead of silently dropping the flag. Likewise a
/// missing agent falls back to an `--agent <FILE>` placeholder, since there is
/// nothing real to print. `store` is the resolved store path to print right
/// after the verb (or `None` for the `<STORE>` placeholder, see
/// [`store_flag`]).
#[must_use]
pub fn resolved_report(
    run_uuid: &str,
    agents: &[PathBuf],
    graph: Option<&Path>,
    graph_run: bool,
    store: Option<&Path>,
    width: usize,
) -> String {
    let mut out = wrap(
        &format!(
            "Run {run_uuid} resolved: recorded the missing write completion by hand. The run \
             no longer needs reconciliation. Continue it with:"
        ),
        width,
        "",
        "",
    );
    let mut command = format!("salvor resume {run_uuid}{}", store_flag(store));
    match graph {
        Some(graph) => command.push_str(&format!(" --graph {}", graph.display())),
        None if graph_run => command.push_str(" --graph <FILE>"),
        None => {}
    }
    if agents.is_empty() {
        command.push_str(" --agent <FILE>");
    } else {
        for agent in agents {
            command.push_str(&format!(" --agent {}", agent.display()));
        }
    }
    // No `--input`: a resolved run is a crashed run whose missing completion
    // was just recorded by hand, so `resume` recovers it rather than resuming a
    // parked one, and `recover` ignores `--input` (it warns and drops it). The
    // command printed here is meant to be copied as it stands, so it carries
    // only flags that do something.
    out.push_str(&format!("\n  {command}\n"));
    out
}

/// The report `salvor abandon` prints once it has appended the terminal
/// `RunAbandoned` by hand. `appended_seq` is the position it landed at, and
/// `unresolved` is the outstanding write (seq, tool) when a needs-reconciliation
/// run was abandoned, so the receipt states plainly that the write stays
/// unresolved and is recorded as such. Nothing was edited or re-run. `width`
/// is the column count its prose wraps to, see [`wrap`].
#[must_use]
pub fn abandoned_report(
    run_uuid: &str,
    appended_seq: u64,
    unresolved: Option<(u64, &str)>,
    width: usize,
) -> String {
    let mut out = wrap(
        &format!(
            "Run {run_uuid}: appended RunAbandoned at seq {appended_seq}. Status now abandoned. \
             Nothing was edited or re-run; the run is retired."
        ),
        width,
        "",
        "",
    );
    out.push('\n');
    if let Some((seq, tool)) = unresolved {
        out.push_str(&wrap(
            &format!(
                "The write at seq {seq} ({tool}) stays unresolved and is recorded as such; its \
                 effect remains unknown."
            ),
            width,
            "",
            "",
        ));
        out.push('\n');
    }
    out
}

/// Every label [`status_label`] can print, which is also every value `salvor list --status`
/// accepts and offers for completion.
///
/// Kept beside `status_label` and `status_group` because the three have to agree: a label the
/// column can print but the filter rejects would be a state you can see and cannot select. A test
/// asserts this list and `status_group` recognise exactly the same set.
pub const STATUS_LABELS: [&str; 11] = [
    "not-started",
    "running",
    "awaiting-model",
    "awaiting-tool",
    "suspended",
    "sleeping",
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

/// The group a status label belongs to, or `None` for a label this build does not recognise,
/// which a future status would be until someone teaches this function about it.
///
/// `sleeping` is in `progress`, not `waiting`, and the group definitions decide it rather than
/// the fact that the run has stopped. `waiting` means a person is the only thing that will move
/// this run; a sleeping run is waiting on an instant, and it continues when that instant arrives
/// whether anyone reads the list or not. Grouping it with the approval queue would put a run
/// nobody can act on into the one group that exists to be a to-do list, and `--group waiting`
/// would stop selecting exactly the rows that need a human.
#[must_use]
pub fn status_group(status: &str) -> Option<StatusGroup> {
    match status {
        "suspended" | "needs-reconciliation" | "budget-exceeded" => Some(StatusGroup::Waiting),
        "running" | "awaiting-model" | "awaiting-tool" | "sleeping" => Some(StatusGroup::Progress),
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
/// replay-time projection, so the caller folds each log first) and, for a
/// sleeping run, the instant its timer wakes it; every other status carries
/// `None` there and the WAKES AT cell prints blank, since the column answers
/// a question only a sleeping run has an answer to.
///
/// The status cell carries ANSI styling unconditionally. Stripping it is the writer's job, not
/// this function's: the caller prints through an `anstream` stream, which removes the codes when
/// stdout is not a terminal and honours `NO_COLOR`. That keeps `salvor list | grep completed`
/// working and keeps this function's output a pure function of its input.
#[must_use]
pub fn list_table(rows: &[(RunSummary, String, Option<OffsetDateTime>)]) -> String {
    let header = anstyle::Style::new().bold();
    let mut out = format!(
        "{header}{:<36}  {:<20}  {:>6}  {:<20}  {:<20}  {:<20}{header:#}\n",
        "RUN ID",
        "STATUS",
        "EVENTS",
        "STARTED",
        "LAST ACTIVITY",
        "WAKES AT",
        header = header,
    );
    for (summary, status, wake_at) in rows {
        // Pad BEFORE styling: escape codes are zero-width on screen but count as characters to
        // `{:<20}`, so a styled-then-padded cell would shear the columns to the right of it.
        let style = status_style(status);
        let padded = format!("{status:<20}");
        let wakes_at = wake_at.map_or_else(String::new, format_ts);
        out.push_str(&format!(
            "{:<36}  {style}{padded}{style:#}  {:>6}  {:<20}  {:<20}  {:<20}\n",
            summary.run_id.as_uuid(),
            summary.event_count,
            format_ts(summary.first_recorded_at),
            format_ts(summary.last_recorded_at),
            wakes_at,
            style = style,
            padded = padded,
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
        RunStatus::Sleeping { .. } => "sleeping",
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
/// printed command matches the shape `salvor_runtime::validate_extension_input`
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
#[must_use]
pub fn format_ts(ts: OffsetDateTime) -> String {
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

/// Formats a span as its two coarsest units, for "overdue by" in a wake
/// report and for how much of a sleeping run's wait is left.
///
/// Two units, not one and not a full breakdown: `2d 3h` answers "how far past
/// its deadline is this timer" the way a bare `2d` cannot (a day and 23 hours
/// reads as `1d 23h`, not as the `1d` a single unit would truncate it to),
/// while `2d 3h 14m 7s` makes the reader do work the second unit already
/// finished. The finer unit is dropped, not rounded, and omitted entirely
/// when it is zero, so a round number still reads as one (`2m`, not `2m 0s`).
/// A negative span (a deadline in the future, which a due-run listing never
/// holds but a caller may still hand over) formats as `0s` rather than a
/// minus sign, since "overdue by minus an hour" is not a sentence.
#[must_use]
pub fn format_duration(span: time::Duration) -> String {
    let seconds = span.whole_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => {
            let (m, s) = (seconds / 60, seconds % 60);
            if s == 0 {
                format!("{m}m")
            } else {
                format!("{m}m {s}s")
            }
        }
        3600..=86_399 => {
            let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
            if m == 0 {
                format!("{h}h")
            } else {
                format!("{h}h {m}m")
            }
        }
        _ => {
            let (d, h) = (seconds / 86_400, (seconds % 86_400) / 3600);
            format!("{d}d {h}h")
        }
    }
}

/// Indents every line of `text` by `spaces`, for nesting a pretty JSON block
/// under a labeled heading. Shared with `crate::graph_editor`, which nests a
/// node's schemas the same way.
pub(crate) fn indent(text: &str, spaces: usize) -> String {
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
            "sleeping",
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

    /// A run on a durable timer prints `sleeping`, and that label sits in `progress`: it is
    /// waiting on an instant, not on a person, so it continues on its own and must not appear in
    /// the group that exists to be a to-do list.
    #[test]
    fn a_sleeping_run_is_in_progress_not_waiting() {
        let wake_at = OffsetDateTime::from_unix_timestamp(1_752_566_400).unwrap();
        assert_eq!(status_label(&RunStatus::Sleeping { wake_at }), "sleeping");
        assert!(
            STATUS_LABELS.contains(&"sleeping"),
            "the STATUS column prints it, so --status must accept it"
        );
        assert_eq!(status_group("sleeping"), Some(StatusGroup::Progress));
        assert_eq!(
            status_style("sleeping"),
            status_style("running"),
            "a sleeping run reads as motion, the same as a running one"
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

    // --- report wrapping ---------------------------------------------------

    use salvor_replay::{Budget, Effect, SequenceNumber, SuspensionKind};

    const UUID: &str = "00000000-0000-4000-8000-000000000000";

    fn sample_pending() -> PendingCall {
        PendingCall::Tool {
            seq: SequenceNumber::new(7),
            tool: "send_email".to_owned(),
            input: serde_json::json!({"to": "ops@example.com"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".to_owned()),
        }
    }

    fn sample_recorded_at() -> Option<OffsetDateTime> {
        Some(OffsetDateTime::from_unix_timestamp(1_752_566_400).unwrap())
    }

    /// The deadline the timer reports below are rendered against.
    fn sample_wake_at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_755_162_000).unwrap()
    }

    /// The store path threaded through the report tests below that are not
    /// themselves about the `--store` flag, standing in for whatever
    /// `cli.store` resolved to on the run that produced the report.
    fn sample_store() -> &'static Path {
        Path::new("salvor.db")
    }

    /// The refusal a `resume` of a still-sleeping run prints says the two
    /// things a reader needs, the instant and what is left of the wait, and
    /// points at the command that drives what is due rather than at a resume
    /// that would be refused again.
    #[test]
    fn the_sleeping_refusal_names_the_instant_and_the_remaining_time() {
        let report = sleeping_report(
            UUID,
            sample_wake_at(),
            time::Duration::minutes(29),
            &[PathBuf::from("agents/writer.toml")],
            None,
            Some(sample_store()),
            DEFAULT_REPORT_WIDTH,
        );
        let visible = flatten(&report);
        assert!(
            visible.contains(&format_ts(sample_wake_at())),
            "the deadline is named: {report}"
        );
        assert!(
            visible.contains("another 29m"),
            "and what is left of the wait: {report}"
        );
        assert!(
            report.contains("  salvor wake --store salvor.db --agent agents/writer.toml"),
            "the command drives what is due, on one line: {report}"
        );
        assert!(
            !report.contains("--input"),
            "a sleeping run takes no input: {report}"
        );
    }

    /// Every span in every report reads the same words at any width; this is
    /// the timer park's share of that rule, which the reports above already
    /// hold to.
    fn flatten(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Every word in `text`, in the order it appears, regardless of which
    /// line it landed on. Comparing this between two widths is how a test
    /// checks that wrapping only ever moves line breaks.
    fn words(text: &str) -> Vec<&str> {
        text.split_whitespace().collect()
    }

    /// `wrap` never drops, reorders, or splits a word, at a width so narrow
    /// that most words each get their own line.
    #[test]
    fn wrap_preserves_word_order_at_a_narrow_width() {
        let text = "the quick brown fox jumps over the lazy dog and then keeps going";
        let wrapped = wrap(text, 10, "", "");
        assert_eq!(words(&wrapped), words(text));
        for line in wrapped.lines() {
            assert!(line.len() <= 10, "line exceeds width 10: {line:?}");
        }
    }

    /// A word longer than `width` is placed on its own line rather than
    /// split, because a broken word reads worse than a long line.
    #[test]
    fn wrap_does_not_split_a_word_wider_than_the_width() {
        let wrapped = wrap("short antidisestablishmentarianism short", 10, "", "");
        assert_eq!(
            words(&wrapped),
            vec!["short", "antidisestablishmentarianism", "short"]
        );
    }

    /// `first_prefix` and `rest_prefix` both count toward `width`, and a
    /// continuation line uses `rest_prefix`, not `first_prefix`, which is how
    /// a numbered list item's wrapped text hangs under its own words instead
    /// of under the number.
    #[test]
    fn wrap_hangs_continuation_lines_under_rest_prefix() {
        let wrapped = wrap(
            "The write did not take effect and still needs to happen",
            30,
            "  2. ",
            "     ",
        );
        let lines: Vec<&str> = wrapped.lines().collect();
        assert!(lines.len() > 1, "expected the text to wrap at all");
        assert!(lines[0].starts_with("  2. "));
        for line in &lines[1..] {
            assert!(
                line.starts_with("     "),
                "continuation line does not carry the hanging indent: {line:?}"
            );
        }
    }

    /// A signal suspension is not addressed to the reader. The report says the
    /// run is awaiting a signal rather than calling it suspended, drops the
    /// instruction to go and supply the input, and still prints the resume
    /// command, which is the only way an operator can stand in for a webhook
    /// that never arrived. The gate report is unchanged beside it.
    #[test]
    fn a_signal_suspension_asks_the_reader_for_nothing() {
        let signal = parked_report(
            UUID,
            &ParkReason::Suspended {
                reason: "awaiting the payment webhook".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: Some(SuspensionKind::Signal),
            },
            Path::new("agent.toml"),
            Some(sample_store()),
            100,
        );
        assert!(
            signal.contains("awaiting a signal"),
            "a signal wait names itself:\n{signal}"
        );
        assert!(
            !signal.contains("parked: suspended"),
            "a signal wait does not read as a human gate:\n{signal}"
        );
        assert!(
            signal.contains("Nothing is waiting on you"),
            "a signal wait tells the reader they owe it nothing:\n{signal}"
        );
        assert!(
            signal.lines().any(|line| line
                .trim_start()
                .starts_with(&format!("salvor resume {UUID}"))),
            "the resume hint survives, for standing in by hand:\n{signal}"
        );

        let gate = parked_report(
            UUID,
            &ParkReason::Suspended {
                reason: "awaiting operator approval".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: None,
            },
            Path::new("agent.toml"),
            Some(sample_store()),
            100,
        );
        assert!(
            gate.contains("parked: suspended"),
            "a gate still reads as a suspension:\n{gate}"
        );
        assert!(
            gate.contains("Resume once you have the input"),
            "a gate is still addressed to the reader:\n{gate}"
        );
    }

    /// The same report, wrapped at a narrow and a wide column count, says the
    /// same thing: only line breaks may move, never words.
    #[test]
    fn same_words_at_width_40_and_width_100() {
        let narrow = reconciliation_report(UUID, Some(&sample_pending()), sample_recorded_at(), 40);
        let wide = reconciliation_report(UUID, Some(&sample_pending()), sample_recorded_at(), 100);
        assert_eq!(words(&narrow), words(&wide));

        let narrow = parked_report(
            UUID,
            &ParkReason::Suspended {
                reason: "awaiting operator approval".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: None,
            },
            Path::new("agent.toml"),
            Some(sample_store()),
            40,
        );
        let wide = parked_report(
            UUID,
            &ParkReason::Suspended {
                reason: "awaiting operator approval".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                kind: None,
            },
            Path::new("agent.toml"),
            Some(sample_store()),
            100,
        );
        assert_eq!(words(&narrow), words(&wide));

        assert_eq!(
            words(&resolved_report(
                UUID,
                &[PathBuf::from("agent.toml")],
                None,
                false,
                Some(sample_store()),
                40
            )),
            words(&resolved_report(
                UUID,
                &[PathBuf::from("agent.toml")],
                None,
                false,
                Some(sample_store()),
                100
            ))
        );
        assert_eq!(
            words(&abandoned_report(UUID, 12, Some((3, "send_email")), 40)),
            words(&abandoned_report(UUID, 12, Some((3, "send_email")), 100))
        );
    }

    /// A command a reader is meant to copy verbatim is never broken across
    /// lines, no matter how narrow the requested width is.
    #[test]
    fn command_examples_are_never_split() {
        let report = reconciliation_report(UUID, Some(&sample_pending()), sample_recorded_at(), 40);
        assert!(
            report.lines().any(|line| line
                == format!("       salvor resolve {UUID} --output '<json the tool returned>'")),
            "the resolve command must survive on one line:\n{report}"
        );

        let report = resolved_report(
            UUID,
            &[PathBuf::from("agents/writer.toml")],
            None,
            false,
            Some(sample_store()),
            40,
        );
        assert!(
            report.lines().any(|line| line
                == format!("  salvor resume {UUID} --store salvor.db --agent agents/writer.toml")),
            "the resume command must survive on one line:\n{report}"
        );

        // A `resolve` that was given no `--agent`/`--graph` still prints a
        // parseable command, with a bracketed placeholder standing in for the
        // one thing it does not know.
        let unfilled = resolved_report(UUID, &[], None, false, Some(sample_store()), 40);
        assert!(
            unfilled
                .lines()
                .any(|line| line
                    == format!("  salvor resume {UUID} --store salvor.db --agent <FILE>")),
            "the fallback resume command must survive on one line:\n{unfilled}"
        );

        // The graph-run counterpart: the operator resolved a graph run but
        // passed neither `--agent` nor `--graph`, so both are unknown. The
        // caller still knows from the run's own log that this is a graph run,
        // so the printed command hints at both placeholders rather than
        // silently dropping `--graph`.
        let unfilled_graph = resolved_report(UUID, &[], None, true, Some(sample_store()), 40);
        assert!(
            unfilled_graph.lines().any(|line| line
                == format!(
                    "  salvor resume {UUID} --store salvor.db --graph <FILE> --agent <FILE>"
                )),
            "the graph fallback resume command must survive on one line:\n{unfilled_graph}"
        );

        let report = parked_report(
            UUID,
            &ParkReason::Suspended {
                reason: "short".to_owned(),
                input_schema: serde_json::json!({}),
                kind: None,
            },
            Path::new("agents/writer.toml"),
            Some(sample_store()),
            40,
        );
        assert!(
            report.lines().any(|line| line
                == format!(
                    "  salvor resume {UUID} --store salvor.db --agent agents/writer.toml --input @resume.json"
                )),
            "the resume command must survive on one line:\n{report}"
        );
    }

    /// The resume command a resolved report prints, when `resolve` was given
    /// `--agent`/`--graph`, is not just plausible-looking text: it is a real
    /// command line the same `clap` parse tree accepts, exactly as the resume
    /// hint a graph run's own parked report prints already is. Feeding it back
    /// through `Cli::try_parse_from` is the proof.
    ///
    /// It also carries no flag the command it names would ignore. `resolve`
    /// applies only to a run parked at a dangling write, which resumes through
    /// the recover path, and recovery ignores `--input`. A copy-pasteable
    /// command that quietly drops one of its own arguments teaches the wrong
    /// thing about what resume does with input.
    #[test]
    fn resolved_report_resume_hint_parses() {
        use clap::Parser;

        let report = resolved_report(
            UUID,
            &[PathBuf::from("agents/writer.toml")],
            Some(Path::new("flow.json")),
            true,
            Some(sample_store()),
            80,
        );
        let line = report
            .lines()
            .find(|line| line.trim_start().starts_with("salvor resume"))
            .expect("resolved report prints a resume command");
        let tokens: Vec<&str> = line.split_whitespace().collect();
        crate::cli::Cli::try_parse_from(&tokens).unwrap_or_else(|error| {
            panic!("printed resume hint does not parse: {error}\nline: {line}")
        });
        assert!(
            !line.contains("--input"),
            "a resolved run recovers, and recovery ignores --input: {line}"
        );
    }

    /// The recorded-intent block keeps its label column aligned and its
    /// values untouched at any width: it is data a reader checks against the
    /// log, not prose a reader rewraps by eye.
    #[test]
    fn the_recorded_intent_block_keeps_its_alignment() {
        let block = "\n  seq:             7\n  \
             recorded at:     2025-07-15 08:00:00Z\n  \
             tool:            send_email\n  \
             effect:          Write\n  \
             idempotency key: key-123\n  \
             input:\n    {\n      \"to\": \"ops@example.com\"\n    }\n";
        for width in [40, 100] {
            let report =
                reconciliation_report(UUID, Some(&sample_pending()), sample_recorded_at(), width);
            assert!(
                report.contains(block),
                "the recorded-intent block at width {width} was reflowed:\n{report}"
            );
        }
    }

    /// No line in a report exceeds the width it was rendered at, except the
    /// spans this module deliberately never wraps: a command line, the
    /// recorded-intent block, and pretty-printed JSON.
    #[test]
    fn no_wrapped_line_exceeds_its_requested_width() {
        const WIDTH: usize = 40;
        let preserved_prefixes = [
            "  seq:",
            "  recorded at:",
            "  tool:",
            "  effect:",
            "  idempotency key:",
            "  input:",
        ];
        let is_preserved = |line: &str| {
            line.contains("salvor ")
                || preserved_prefixes.iter().any(|p| line.starts_with(p))
                || line.trim_start().starts_with('{')
                || line.trim_start().starts_with('}')
                || line.trim_start().starts_with('"')
        };

        let reports = [
            reconciliation_report(UUID, Some(&sample_pending()), sample_recorded_at(), WIDTH),
            reconciliation_report(UUID, None, None, WIDTH),
            resolved_report(
                UUID,
                &[PathBuf::from("agent.toml")],
                None,
                false,
                Some(sample_store()),
                WIDTH,
            ),
            abandoned_report(UUID, 12, Some((3, "send_email")), WIDTH),
            abandoned_report(UUID, 12, None, WIDTH),
            parked_report(
                UUID,
                &ParkReason::Suspended {
                    reason: "short".to_owned(),
                    input_schema: serde_json::json!({}),
                    kind: None,
                },
                Path::new("agent.toml"),
                Some(sample_store()),
                WIDTH,
            ),
            parked_report(
                UUID,
                &ParkReason::BudgetExceeded {
                    budget: Budget {
                        kind: BudgetKind::Tokens,
                        limit: 1000.0,
                    },
                    observed: 1200.0,
                },
                Path::new("agent.toml"),
                Some(sample_store()),
                WIDTH,
            ),
            parked_report(
                UUID,
                &ParkReason::Sleeping {
                    wake_at: sample_wake_at(),
                },
                Path::new("agent.toml"),
                Some(sample_store()),
                WIDTH,
            ),
            sleeping_report(
                UUID,
                sample_wake_at(),
                time::Duration::minutes(29),
                &[PathBuf::from("agent.toml")],
                None,
                Some(sample_store()),
                WIDTH,
            ),
        ];

        for report in reports {
            for line in report.lines() {
                if is_preserved(line) {
                    continue;
                }
                assert!(
                    line.len() <= WIDTH,
                    "line exceeds width {WIDTH}: {line:?}\nfull report:\n{report}"
                );
            }
        }
    }

    // --- the `--store` flag on a resume/wake hint --------------------------

    /// A `resume`/`wake` hint names the exact store path the command that
    /// printed it resolved, so the operator can copy the line and run it
    /// against the same store without adding `--store` by hand.
    #[test]
    fn the_hint_command_carries_the_resolved_store_path() {
        let report = sleeping_report(
            UUID,
            sample_wake_at(),
            time::Duration::minutes(29),
            &[PathBuf::from("agents/writer.toml")],
            None,
            Some(Path::new("/var/lib/salvor/salvor.db")),
            DEFAULT_REPORT_WIDTH,
        );
        assert!(
            report.contains("--store /var/lib/salvor/salvor.db"),
            "the hint names the resolved store path: {report}"
        );
    }

    /// A caller with no resolved store path to hand over, such as the wasm
    /// build with no SQLite file open, still gets a copy-pasteable-looking
    /// command: a `<STORE>` placeholder stands in, the same way `<FILE>`
    /// already stands in for a missing agent path.
    #[test]
    fn a_missing_store_path_prints_a_placeholder() {
        let report = resolved_report(
            UUID,
            &[PathBuf::from("agent.toml")],
            None,
            false,
            None,
            DEFAULT_REPORT_WIDTH,
        );
        assert!(
            report.contains("--store <STORE>"),
            "a missing store path falls back to a placeholder: {report}"
        );
    }

    // --- `format_duration` --------------------------------------------------

    /// `format_duration` prints its two coarsest units, dropping the finer one
    /// entirely when it is zero rather than printing a `0` for it.
    #[test]
    fn format_duration_prints_two_units() {
        assert_eq!(format_duration(time::Duration::seconds(76)), "1m 16s");
        assert_eq!(format_duration(time::Duration::seconds(120)), "2m");
        assert_eq!(format_duration(time::Duration::seconds(3661)), "1h 1m");
        assert_eq!(format_duration(time::Duration::seconds(90_000)), "1d 1h");
        assert_eq!(format_duration(time::Duration::seconds(-1)), "0s");
    }
}
