//! The command handlers: one function per verb, each taking parsed arguments
//! and the resolved store path and returning a process exit code.
//!
//! # Exit codes
//!
//! A handler returns `Ok(0)` for success, `Ok(1)` for a deliberate refusal
//! that is not an internal error (resuming a run that needs human
//! reconciliation, or `replay` without `--dry-run`), and `Err(..)` for a
//! genuine failure the top level reports and turns into a non-zero exit.
//! Parking is **not** a failure: a run that suspends or hits a budget exits
//! `0` with a report telling the operator how to continue it.
//!
//! # Progress and stdout discipline
//!
//! Command output (the final result, the tables, the reports) goes to stdout.
//! Progress goes to the tracing log on stderr, so a caller can pipe stdout
//! cleanly. The run id is printed to stdout **first**, before the run drives,
//! so an operator can copy it and `resume` even after a `kill -9`.
//!
//! The v0.1 runtime emits no tracing of its own, and its `start`/`recover`/
//! `resume` calls drive the whole loop before returning, so this module cannot
//! observe steps mid-flight through the runtime API. It instead reads the
//! persisted log after the drive and emits one info line per recorded event,
//! carrying the run id and sequence number. That is honest progress with the
//! required correlation fields; live per-step streaming would mean
//! instrumenting the runtime, which is out of scope here.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use salvor_core::{RunId, RunStatus, derive_state};
use salvor_runtime::{RunOutcome, Runtime};
use salvor_store::{EventStore, SqliteStore};
use serde_json::Value;
use uuid::Uuid;

use crate::agent_config::{self, AgentConfig};
use crate::cli::{HistoryArgs, ReplayArgs, ResumeArgs, RunArgs};
use crate::render;

/// `salvor run`: start a fresh run, print its id, drive it, and report.
pub async fn run(store_path: &Path, args: RunArgs) -> Result<u8> {
    let config = AgentConfig::load(&args.agent)?;
    let input = parse_input(&args.input)?;
    let store = open_store(store_path)?;

    let (agent, servers) = agent_config::build_agent(&config, &args.agent).await?;
    let runtime = Runtime::new(store.clone());

    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();
    // Printed first, so a kill mid-run still leaves the operator an id to resume.
    println!("run {uuid}");
    tracing::info!(run_id = %uuid, "starting run");

    let outcome = runtime.start_with_id(&agent, run_id, input).await;
    emit_progress(&store, run_id).await;
    close_servers(servers).await;

    report_outcome(outcome?, &uuid, &args.agent)
}

/// `salvor resume`: continue an existing run, dispatching on its derived state.
///
/// The mapping (task §3): a run that needs reconciliation is refused with
/// evidence; a parked run (suspended or budget-exceeded) needs `--input` and
/// resumes; a crashed run (running or awaiting a step) recovers with no input;
/// a finished run is reported and left alone.
pub async fn resume(store_path: &Path, args: ResumeArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {uuid} in this store");
    }
    let state = derive_state(&log);

    // These branches need no agent and spawn no MCP servers, so decide them
    // before paying to build the agent.
    match &state.status {
        RunStatus::NeedsReconciliation => {
            print!(
                "{}",
                render::reconciliation_report(&uuid, state.pending_call.as_ref())
            );
            return Ok(1);
        }
        RunStatus::Completed { output } => {
            println!("run {uuid} already completed. Final output:");
            println!("{}", render::pretty_json(output));
            return Ok(0);
        }
        RunStatus::Failed { error } => {
            println!("run {uuid} already failed: {error}");
            return Ok(0);
        }
        RunStatus::NotStarted => bail!("run {uuid} has no recorded events"),
        _ => {}
    }

    let config = AgentConfig::load(&args.agent)?;
    let (agent, servers) = agent_config::build_agent(&config, &args.agent).await?;
    let runtime = Runtime::new(store.clone());

    let outcome = match &state.status {
        RunStatus::Suspended { .. } | RunStatus::BudgetExceeded { .. } => {
            let raw = args.input.as_deref().context(
                "this run is parked awaiting input; pass --input <json|@file> to resume it",
            )?;
            let input = parse_input(raw)?;
            tracing::info!(run_id = %uuid, "resuming parked run");
            runtime.resume(&agent, run_id, input).await
        }
        // Running / AwaitingModel / AwaitingTool: the process died mid-step.
        _ => {
            if args.input.is_some() {
                tracing::warn!(
                    run_id = %uuid,
                    "this run crashed mid-step; --input is ignored when recovering"
                );
            }
            tracing::info!(run_id = %uuid, "recovering crashed run");
            runtime.recover(&agent, run_id).await
        }
    };

    emit_progress(&store, run_id).await;
    close_servers(servers).await;
    report_outcome(outcome?, &uuid, &args.agent)
}

/// `salvor list`: one row per run, with status folded from each log.
pub async fn list(store_path: &Path) -> Result<u8> {
    let store = open_store(store_path)?;
    let mut summaries = store.list_runs().await?;
    if summaries.is_empty() {
        println!("no runs in {}", store_path.display());
        return Ok(0);
    }
    summaries.sort_by_key(|summary| summary.first_recorded_at);

    let mut rows = Vec::with_capacity(summaries.len());
    for summary in summaries {
        // Status is a replay-time projection, not a stored column, so fold the
        // log to get it. See RunSummary's docs for why status stays out of the
        // store.
        let log = store.read_log(summary.run_id).await?;
        let status = render::status_label(&derive_state(&log).status).to_owned();
        rows.push((summary, status));
    }
    print!("{}", render::list_table(&rows));
    Ok(0)
}

/// `salvor history`: the pretty event log, or raw JSON envelopes with `--json`.
pub async fn history(store_path: &Path, args: HistoryArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let store = open_store(store_path)?;
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {} in this store", run_id.as_uuid());
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&log)?);
    } else {
        for envelope in &log {
            println!("{}", render::history_line(envelope));
        }
    }
    Ok(0)
}

/// `salvor replay --dry-run`: re-derive state from the log, execute nothing.
pub async fn replay(store_path: &Path, args: ReplayArgs) -> Result<u8> {
    if !args.dry_run {
        eprintln!(
            "salvor replay only supports --dry-run in this version: it re-derives state from the log without executing anything. Live replay (re-running from a chosen point) arrives in a later version."
        );
        return Ok(1);
    }
    let run_id = parse_run_id(&args.run_id)?;
    let store = open_store(store_path)?;
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {} in this store", run_id.as_uuid());
    }
    let state = derive_state(&log);
    print!("{}", render::replay_summary(&state));
    Ok(0)
}

/// Prints the final result of a completed run, or the parked report of a
/// suspended one. Both are exit code 0.
fn report_outcome(outcome: RunOutcome, uuid: &str, agent_path: &Path) -> Result<u8> {
    match outcome {
        RunOutcome::Completed { output, .. } => {
            println!("{}", render::pretty_json(&output));
            Ok(0)
        }
        RunOutcome::Parked { reason, .. } => {
            print!("{}", render::parked_report(uuid, &reason, agent_path));
            Ok(0)
        }
    }
}

/// Reads the persisted log and emits one info-level line per event, carrying
/// the run id and sequence number. Best-effort: a read failure here does not
/// change the run's outcome, so it is logged and swallowed rather than
/// propagated.
async fn emit_progress(store: &Arc<dyn EventStore>, run_id: RunId) {
    let uuid = run_id.as_uuid().to_string();
    match store.read_log(run_id).await {
        Ok(log) => {
            for envelope in &log {
                tracing::info!(
                    run_id = %uuid,
                    seq = envelope.seq.get(),
                    "{} {}",
                    render::event_kind(&envelope.event),
                    render::event_detail(&envelope.event),
                );
            }
        }
        Err(error) => tracing::warn!(run_id = %uuid, %error, "could not read log for progress"),
    }
}

/// Closes every MCP server session tidily. Errors are logged, not propagated:
/// the run already finished, so a teardown hiccup must not fail the command.
async fn close_servers(servers: Vec<salvor_tools::mcp::McpServer>) {
    for server in servers {
        if let Err(error) = server.close().await {
            tracing::warn!(%error, "MCP server did not shut down cleanly");
        }
    }
}

/// Opens the SQLite store, wrapped as the trait object the runtime holds.
fn open_store(path: &Path) -> Result<Arc<dyn EventStore>> {
    let store =
        SqliteStore::open(path).with_context(|| format!("opening store at {}", path.display()))?;
    Ok(Arc::new(store))
}

/// Parses a `--input` value: `@path` reads JSON from a file, anything else is
/// parsed as a JSON literal.
pub fn parse_input(raw: &str) -> Result<Value> {
    let text = if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("reading input file {path}"))?
    } else {
        raw.to_owned()
    };
    serde_json::from_str(&text).context("parsing --input as JSON (wrap a bare string in quotes)")
}

/// Parses a run id from its UUID string.
fn parse_run_id(text: &str) -> Result<RunId> {
    let uuid = Uuid::parse_str(text)
        .with_context(|| format!("`{text}` is not a valid run id (expected a UUID)"))?;
    Ok(RunId::from_uuid(uuid))
}
