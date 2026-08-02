//! Turning runtime and store values into the text the CLI prints.
//!
//! Almost all of it lives in [`salvor_cli_core::render`] and is re-exported
//! here unchanged, so every `render::` path in this crate resolves as before.
//! That module is pure: a function from a value to a `String`, with no IO, no
//! store access, and no clock, which is what lets a browser terminal format
//! salvor's output with the same code this binary does.
//!
//! What stays here is the `salvor serve --kill` table, because its input
//! describes live processes on this machine, and the `agent validate` summary,
//! because it formats a built [`salvor_runtime::Agent`] and that type is kept
//! out of `salvor-cli-core` so the pure renderer stays buildable for
//! `wasm32-unknown-unknown`.

pub use salvor_cli_core::render::*;

use std::collections::BTreeMap;

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
