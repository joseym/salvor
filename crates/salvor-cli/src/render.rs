//! Turning runtime and store values into the text the CLI prints.
//!
//! Almost all of it lives in [`salvor_cli_core::render`] and is re-exported
//! here unchanged, so every `render::` path in this crate resolves as before.
//! That module is pure: a function from a value to a `String`, with no IO, no
//! store access, and no clock, which is what lets a browser terminal format
//! salvor's output with the same code this binary does.
//!
//! What stays here is the `salvor serve --kill` table, because its input
//! describes live processes on this machine, the `agent validate` summary,
//! because it formats a built [`salvor_runtime::Agent`] and that type is kept
//! out of `salvor-cli-core` so the pure renderer stays buildable for
//! `wasm32-unknown-unknown`, and the `anchor`/`verify` reports, because they
//! format [`crate::anchor`]'s documents and those describe a store on this
//! machine.

pub use salvor_cli_core::render::*;

use std::collections::BTreeMap;
use std::path::Path;

use salvor_runtime::Agent;

use crate::serve_kill::RunningServer;

/// The success report for `salvor agent validate`: what the definition
/// declares, once it has built.
///
/// Reports the facts a person checking an agent file wants confirmed: the
/// model that will be called, whether a system prompt is set, how many tools
/// the agent ended up with and how many MCP servers were reached to collect
/// them (proof the declared servers actually started), the declared budgets,
/// the declared idempotency keys, and the content hash a graph `agent` node
/// would reference. Pure formatting of an already-built [`Agent`]; a
/// validation failure is reported by the handler, not here.
#[must_use]
pub fn agent_summary(
    agent: &Agent,
    mcp_server_count: usize,
    idempotency_keys: &BTreeMap<String, String>,
) -> String {
    let name = agent.name().unwrap_or("(none)");
    let prompt = match agent.system_prompt() {
        Some(text) => format!("set ({} chars)", text.chars().count()),
        None => "(none)".to_owned(),
    };
    format!(
        "agent ok: model {}, {} tool(s) from {} mcp server(s)\n\
         name:    {}\n\
         prompt:  {}\n\
         budgets: {}\n\
         keys:    {}\n\
         hash:    {}\n",
        agent.model(),
        agent.tools().len(),
        mcp_server_count,
        name,
        prompt,
        budgets_line(agent.budgets()),
        idempotency_line(idempotency_keys),
        agent.def_hash(),
    )
}

/// The success report for `salvor agent validate --no-connect`: fields and
/// shape checked, no MCP server contacted.
///
/// Names what was skipped rather than silently reporting fewer tools: `--
/// no-connect` never spawns or dials a declared MCP server, so `mcp_declared`
/// (the number of `[[mcp_servers]]` tables the file declared) is printed as
/// skipped, and the hash line says why it is withheld instead of printing a
/// number that would not match the hash a real (connecting) build produces.
/// `idempotency_keys` still prints: those declarations come straight out of
/// the parsed config, not out of a connected server, so `--no-connect` reports
/// them exactly as it would with a connection.
#[must_use]
pub fn agent_summary_no_connect(
    agent: &Agent,
    mcp_declared: usize,
    idempotency_keys: &BTreeMap<String, String>,
) -> String {
    let name = agent.name().unwrap_or("(none)");
    let prompt = match agent.system_prompt() {
        Some(text) => format!("set ({} chars)", text.chars().count()),
        None => "(none)".to_owned(),
    };
    let mcp_line = if mcp_declared == 0 {
        "no mcp servers declared".to_owned()
    } else {
        format!(
            "{mcp_declared} mcp server(s) declared, not connected (--no-connect): tools not verified"
        )
    };
    format!(
        "agent fields ok: model {}, {}\n\
         name:    {}\n\
         prompt:  {}\n\
         budgets: {}\n\
         keys:    {}\n\
         hash:    (not computed: depends on MCP tool schemas, which --no-connect does not collect; run without --no-connect for the real hash)\n",
        agent.model(),
        mcp_line,
        name,
        prompt,
        budgets_line(agent.budgets()),
        idempotency_line(idempotency_keys),
    )
}

/// The declared idempotency keys, one clause per tool in the report's own
/// voice (`<tool> key from <path>`), comma-joined the same way
/// [`budgets_line`] joins its dimensions, or `(none)` when the file declares
/// none. Tool order follows the map's own key order (by tool name), so the
/// line is stable across runs of the same file.
fn idempotency_line(keys: &BTreeMap<String, String>) -> String {
    if keys.is_empty() {
        return "(none)".to_owned();
    }
    keys.iter()
        .map(|(tool, path)| format!("{tool} key from {path}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The declared budgets, one dimension at a time in the fixed order (steps,
/// tokens, cost, wall time), or `(none)` when nothing is declared.
fn budgets_line(budgets: &salvor_runtime::Budgets) -> String {
    let mut parts = Vec::new();
    if let Some(steps) = budgets.max_steps {
        parts.push(format!("steps {steps}"));
    }
    if let Some(tokens) = budgets.max_tokens {
        parts.push(format!("tokens {tokens}"));
    }
    if let Some(cost) = budgets.max_cost_usd {
        parts.push(format!("cost_usd {cost}"));
    }
    if let Some(wall_time) = budgets.max_wall_time {
        parts.push(format!("wall_time_seconds {}", wall_time.as_secs_f64()));
    }
    if parts.is_empty() {
        "(none)".to_owned()
    } else {
        parts.join(", ")
    }
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

/// The line `salvor anchor` prints on stderr once the anchor is written: how
/// many runs it covers and where it went.
///
/// On stderr, not stdout, because with no `--out` the anchor itself is on
/// stdout and a caller redirecting it into a file must get the file and
/// nothing else.
#[must_use]
pub fn anchored_line(runs: usize, out: Option<&Path>) -> String {
    let where_to = match out {
        Some(path) => format!("written to {}", path.display()),
        None => "printed on stdout".to_owned(),
    };
    if runs == 0 {
        return format!(
            "anchored 0 runs ({where_to}). This store holds no runs, so the anchor commits to \
             nothing; take one over a store that holds runs."
        );
    }
    format!(
        "anchored {runs} run(s) ({where_to}). Keep it somewhere this store cannot reach: an \
         anchor kept beside the database it describes is rewritten by whoever rewrites the \
         database."
    )
}

/// The report `salvor verify` prints: one line per run, then the summary, then
/// what to do about it when something does not match.
///
/// Every run is named, including the ones that are fine, because the question
/// this command answers is "does this store still hold what it held", and an
/// answer that lists only trouble cannot tell "nothing is wrong" from "nothing
/// was checked".
#[must_use]
pub fn verify_report(result: &crate::anchor::Verification) -> String {
    use crate::anchor::Finding;

    let mut out = String::new();
    for entry in &result.runs {
        let run = &entry.run;
        match &entry.finding {
            Finding::Intact {
                anchored_len,
                events_since,
                ..
            } => {
                out.push_str(&format!("run {run}: intact at {anchored_len} event(s)"));
                if *events_since > 0 {
                    out.push_str(&format!(", {events_since} recorded since the anchor"));
                }
                out.push('\n');
            }
            Finding::New { len } => {
                out.push_str(&format!(
                    "run {run}: new since the anchor, {len} event(s). Not covered by this \
                     anchor; the next one covers it.\n"
                ));
            }
            Finding::Missing { anchored_len } => {
                out.push_str(&format!(
                    "run {run}: missing. The anchor recorded {anchored_len} event(s); this store \
                     holds none.\n"
                ));
            }
            Finding::Shortened { anchored_len, len } => {
                out.push_str(&format!(
                    "run {run}: shortened. The anchor recorded {anchored_len} event(s); this \
                     store holds {len}.\n"
                ));
            }
            Finding::Rewritten {
                anchored_len,
                anchored_hash,
                found_hash,
            } => {
                out.push_str(&format!(
                    "run {run}: rewritten at event {anchored_len}. The events this anchor \
                     covered are not the events this store now holds.\n"
                ));
                out.push_str(&format!("  the anchor recorded {anchored_hash}\n"));
                match found_hash {
                    Some(found) => out.push_str(&format!("  this store holds  {found}\n")),
                    None => out.push_str("  this store holds no event at that position\n"),
                }
            }
            Finding::Broken { seq, detail } => {
                out.push_str(&format!(
                    "run {run}: broken. This store refuses its own log at seq {seq}: {detail}.\n"
                ));
            }
        }
    }

    out.push_str(&format!(
        "{} run(s) anchored, {} intact, {} new since the anchor\n",
        result.anchored, result.intact, result.new
    ));

    if !result.ok {
        out.push_str(
            "\nThis store no longer holds what the anchor says it held. Do not re-anchor it: a \
             fresh anchor\nover a rewritten store records the rewrite. Go back to a backup that \
             reads clean and verifies\nagainst this anchor. See docs/OPERATIONS.md, Anchoring \
             the chain.\n",
        );
    }
    out
}
