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
//! Progress streams live. `salvor-runtime` emits one info-level record at each
//! persist (see `salvor_runtime`'s progress module), carrying the run id and
//! sequence number, the instant the event becomes durable. So this module does
//! not walk the log after the drive: it just lets the runtime's records flow to
//! the subscriber on stderr as the run drives. A resumed or recovered run
//! replays its recorded prefix silently (those events are not re-persisted) and
//! streams only its genuinely new activity, which is what progress should mean.
//! The full recorded log, replayed prefix and all, is always available through
//! `salvor history`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use salvor_core::{Event, EventEnvelope, PendingCall, RunId, RunStatus, derive_state};
use salvor_engine::{
    ForkError, GraphOutcome, ToolResolver, WriteHazard, graph_hash, plan_fork, run_graph,
};
use salvor_graph::{Graph, Node};
use salvor_runtime::{
    Agent, ParkReason, RunCtx, RunOutcome, Runtime, RuntimeError, validate_labels,
};
use salvor_server::dispatch::{Disposition, classify};
use salvor_server::{
    AgentDefinition, AgentFactory, AppState, BuiltAgent, DefFormat, LlmModelExecutor, ToolRegistry,
};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use salvor_tools::mcp::McpServer;
use serde_json::Value;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::agent_config::{self, AgentConfig};
use crate::checkout;
use crate::cli::{
    AbandonArgs, AgentHashArgs, BuildArgs, CompletionsArgs, ForkArgs, GraphRunArgs,
    GraphValidateArgs, HistoryArgs, ListArgs, ReplayArgs, ResolveArgs, ResumeArgs, RunArgs,
    ServeArgs,
};
use crate::dev_server::DevServer;
use crate::render;
use crate::serve_kill;

/// `salvor run`: start a fresh run, print its id, drive it, and report.
///
/// `--fixture <DIR>` is the offline variant: the agent, the input, and a
/// recorded model conversation all come from one directory, and a scripted
/// model is stood up locally to serve that conversation (see [`crate::fixture`]).
/// It changes only where those three things come from — everything below the
/// resolution is the same store, the same runtime, and the same event log a
/// `--agent`/`--input` run gets.
pub async fn run(store_path: &Path, args: RunArgs) -> Result<u8> {
    // Resolve the two ways of naming a run into the one shape the rest of this
    // handler works in. clap already guarantees exactly one of them is present
    // (see `RunArgs`), so the `None` arms below are unreachable in practice;
    // they still return an actionable message rather than panicking, because a
    // handler is not the place to trust a parse tree absolutely.
    let fixture = args
        .fixture
        .as_deref()
        .map(crate::fixture::Fixture::load)
        .transpose()?;
    let (agent_path, input) = match &fixture {
        Some(fixture) => (fixture.agent_path().to_owned(), fixture.input().clone()),
        None => {
            let agent = args
                .agent
                .clone()
                .context("`salvor run` needs --agent <file>, or --fixture <dir>")?;
            let raw = args
                .input
                .as_deref()
                .context("`salvor run` needs --input <json|@file>, or --fixture <dir>")?;
            (agent, parse_input(raw)?)
        }
    };

    let config = AgentConfig::load(&agent_path)?;
    let store = open_store(store_path)?;

    // The fixture's model binds a local port and exports the agent's declared
    // `base_url_env` at it. This must happen BEFORE the agent is built: the
    // client config reads that variable once, at build time.
    let model = match &fixture {
        Some(fixture) => Some(fixture.start_model(&config).await?),
        None => None,
    };

    // From here the fixture path and the ordinary path are the same code. Any
    // failure between the model starting and the run finishing tears it down
    // first, the way `close_servers` runs regardless of outcome below.
    let (agent, servers) = match agent_config::build_agent(&config, &agent_path).await {
        Ok(built) => built,
        Err(error) => {
            shutdown_model(model).await;
            return Err(error);
        }
    };
    // The agent carries the resolved prompt-recording flag (per-agent config
    // over SALVOR_RECORD_PROMPTS over off); hand it to the runtime so the
    // RunCtx driving this run records the body only when opted in.
    let mut runtime = Runtime::new(store.clone()).with_record_prompts(agent.record_prompts());
    // Labels set on the agent definition (via the Rust builder; there is no
    // TOML surface for them yet) ride along the same way record_prompts does.
    if let Some(labels) = agent.labels() {
        runtime = runtime.with_labels(labels.clone());
    }

    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();
    // Printed first, so a kill mid-run still leaves the operator an id to resume.
    println!("run {uuid}");
    tracing::info!(run_id = %uuid, "starting run");

    // Progress streams live from the runtime as the loop drives; this call
    // returns only once the run has completed or parked. `close_servers` runs
    // regardless of the outcome, so a teardown always happens before the `?`.
    let outcome = runtime.start_with_id(&agent, run_id, input).await;
    close_servers(servers).await;
    shutdown_model(model).await;

    report_outcome(outcome?, &uuid, &agent_path)
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

    // The state-to-verb mapping is the shared `classify` the control-plane
    // server uses too, so the CLI and the HTTP resume endpoint can never
    // disagree on what a given state means. These first arms need no agent and
    // spawn no MCP servers, so decide them before paying to build the agent.
    let disposition = classify(&state);
    match disposition {
        Disposition::Reconcile(_) => {
            // The intent's timestamp is part of the evidence; find it in the
            // log by the pending call's sequence number.
            let recorded_at = match state.pending_call.as_ref() {
                Some(PendingCall::Tool { seq, .. }) => log
                    .iter()
                    .find(|envelope| envelope.seq == *seq)
                    .map(|envelope| envelope.recorded_at),
                _ => None,
            };
            print!(
                "{}",
                render::reconciliation_report(
                    &uuid,
                    state.pending_call.as_ref(),
                    recorded_at,
                    render::DEFAULT_REPORT_WIDTH,
                )
            );
            return Ok(1);
        }
        Disposition::Completed(output) => {
            println!("run {uuid} already completed. Final output:");
            println!("{}", render::pretty_json(&output));
            return Ok(0);
        }
        Disposition::Failed(error) => {
            println!("run {uuid} already failed: {error}");
            return Ok(0);
        }
        Disposition::Abandoned {
            reason,
            unresolved_write,
        } => {
            match reason {
                Some(reason) => println!("run {uuid} was abandoned: {reason}"),
                None => println!("run {uuid} was abandoned"),
            }
            if let Some(write) = unresolved_write {
                println!(
                    "  the write at seq {} ({}) was left unresolved and is recorded as such; \
                     its effect stays unknown",
                    write.seq.get(),
                    write.tool
                );
            }
            return Ok(0);
        }
        Disposition::NotStarted => bail!("run {uuid} has no recorded events"),
        Disposition::Resume(_) | Disposition::Recover => {}
    }

    // A graph run re-drives over the engine, not the built-in loop. The classify
    // above, the parked-vs-crashed decision, and the input handling are shared;
    // only the re-drive differs, because the log records the graph's hash, not
    // the document. See `resume_graph`.
    if is_graph_run(&log) {
        return resume_graph(store, run_id, &uuid, &log, &args, disposition).await;
    }

    // An agent run rebuilds its one agent. Exactly one `--agent` is expected.
    let agent_path = single_agent(&args.agents)?;
    let config = AgentConfig::load(agent_path)?;
    let (agent, servers) = agent_config::build_agent(&config, agent_path).await?;
    let mut runtime = Runtime::new(store.clone()).with_record_prompts(agent.record_prompts());
    if let Some(labels) = agent.labels() {
        runtime = runtime.with_labels(labels.clone());
    }

    let outcome = match disposition {
        Disposition::Resume(_) => {
            let raw = args.input.as_deref().context(
                "this run is parked awaiting input; pass --input <json|@file> to resume it",
            )?;
            let input = parse_input(raw)?;
            tracing::info!(run_id = %uuid, "resuming parked run");
            runtime.resume(&agent, run_id, input).await
        }
        // Recover: the process died mid-step (running or awaiting a step).
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

    close_servers(servers).await;
    report_outcome(outcome?, &uuid, agent_path)
}

/// `salvor fork`: fork a graph run from a node boundary into a NEW run, refusing
/// to re-execute a recorded write the operator has not acknowledged.
///
/// The local flavor of the server's `POST /v1/runs/{id}/fork`, mirroring how
/// `graph run` and a graph `resume` re-supply their documents: the origin's log
/// records only the graph's hash, so the document is re-supplied through
/// `--graph` (hash-checked against the recorded one, since a fork reuses the
/// origin's graph unchanged) and its `agent` nodes through `--agent`.
///
/// The fork planning is the shared, pure [`plan_fork`]; only the IO differs from
/// the server. On a hazard the operator has not acknowledged it refuses (exit 1),
/// listing exactly the writes that would re-fire; with `--acknowledge-writes`
/// covering them it writes the child's prefix (the origin's events below the fork
/// node, rewritten under a fresh id, seq-0 carrying the fork origin) and drives
/// the child onward from the fork node exactly as a recovered graph run.
pub async fn fork(store_path: &Path, args: ForkArgs) -> Result<u8> {
    let origin_id = parse_run_id(&args.run_id)?;
    let origin_uuid = origin_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    let origin_log = store.read_log(origin_id).await?;
    if origin_log.is_empty() {
        bail!("no run {origin_uuid} in this store");
    }

    // An origin parked at a dangling write must be resolved first.
    if matches!(
        derive_state(&origin_log).status,
        RunStatus::NeedsReconciliation
    ) {
        bail!(
            "origin run {origin_uuid} is parked at a dangling write; resolve it \
             (salvor resolve {origin_uuid} --output <json>) before forking, so the fork does not \
             inherit an unsettled write"
        );
    }

    // Plan the fork purely: boundary, prefix, and the write hazard set.
    let plan = plan_fork(&origin_log, &args.from_node).map_err(|error| match error {
        ForkError::NotAGraphRun => anyhow::anyhow!(
            "run {origin_uuid} is an agent run, not a graph run; only a graph run has node \
             boundaries to fork from"
        ),
        ForkError::NodeNeverEntered { node } => anyhow::anyhow!(
            "run {origin_uuid} never entered node `{node}`; fork from a node boundary the run reached"
        ),
    })?;

    // The re-supplied document must hash to the origin's recorded graph.
    let graph = load_and_validate_graph(&args.graph)?;
    let supplied_hash = graph_hash(&graph)?;
    if supplied_hash != plan.graph_hash() {
        bail!(
            "the graph in {} hashes to {supplied_hash}, but run {origin_uuid} forked from {}; a fork \
             reuses the SAME document the origin ran (submit a changed graph as a new run instead)",
            args.graph.display(),
            plan.graph_hash()
        );
    }

    // Resolve the acknowledgement: `all` covers the full hazard set, else a
    // comma-separated seq list. Then find what the acknowledgement misses.
    let hazard_seqs = plan.hazard_seqs();
    let acknowledged = parse_acknowledge_writes(args.acknowledge_writes.as_deref(), &hazard_seqs)?;
    let missing: Vec<u64> = hazard_seqs
        .iter()
        .copied()
        .filter(|seq| !acknowledged.contains(seq))
        .collect();

    // dry_run: print the preview, create nothing.
    if args.dry_run {
        print!("{}", render_fork_preview(&plan, &missing));
        return Ok(0);
    }

    // Refuse-then-record: any unacknowledged hazard refuses, listing what is
    // missing (exit 1, as a reconciliation refusal does).
    if !missing.is_empty() {
        let unacked: Vec<&WriteHazard> = plan
            .hazards()
            .iter()
            .filter(|hazard| missing.contains(&hazard.seq))
            .collect();
        print!(
            "{}",
            render_fork_refusal(&origin_uuid, &args.from_node, &unacked)
        );
        return Ok(1);
    }

    // Everything the child references must resolve, before any envelope is written.
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    // Mint the child and write its prefix; it exists, standalone, at once.
    let child_id = RunId::new();
    let child_uuid = child_id.as_uuid().to_string();
    let child_prefix = plan.build_child_prefix(child_id, hazard_seqs);
    for envelope in &child_prefix {
        if let Err(error) = store.append(envelope).await {
            close_servers(servers).await;
            return Err(error.into());
        }
    }
    // Printed first, so a kill mid-drive still leaves the operator an id to resume.
    println!(
        "run {child_uuid} (forked from {origin_uuid} at node `{}`)",
        args.from_node
    );
    tracing::info!(run_id = %child_uuid, origin = %origin_uuid, "forking graph run");

    // Continue from the fork node exactly like a recovered graph run.
    let child_log = store.read_log(child_id).await?;
    let mut ctx = RunCtx::new(store, child_id, child_log)?;
    let outcome = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools).await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, &child_uuid, &args.graph, &args.agents)
}

/// `salvor resolve`: record the completion of a dangling write by hand.
///
/// This is the operator side of reconciliation. A run whose log ends at a
/// write intent with no completion (status `NeedsReconciliation`) cannot be
/// recovered automatically: the write may or may not have taken effect. After
/// a human has verified externally what happened, `resolve` records the
/// completion they observed, so a later `resume` replays it and never re-runs
/// the write. It needs no agent and drives nothing.
pub async fn resolve(store_path: &Path, args: ResolveArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let output = parse_input(&args.output)?;
    let store = open_store(store_path)?;
    if store.read_log(run_id).await?.is_empty() {
        bail!("no run {uuid} in this store");
    }

    let runtime = Runtime::new(store);
    match runtime.resolve(run_id, output).await {
        Ok(_) => {
            print!(
                "{}",
                render::resolved_report(&uuid, render::DEFAULT_REPORT_WIDTH)
            );
            Ok(0)
        }
        // Refusing to resolve a run that is not awaiting reconciliation is a
        // deliberate refusal, not an internal error: exit 1 with an explanation.
        Err(RuntimeError::NotReconcilable { status, .. }) => {
            eprintln!(
                "run {uuid} does not need reconciliation (status: {status}); there is no dangling write to resolve"
            );
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// `salvor abandon`: retire a run by hand, appending a terminal `RunAbandoned`.
///
/// The operator's "we do not care about this run anymore" path, for a run that
/// is dead forever or no longer worth carrying. A deliberate sibling of
/// [`resolve`]: it needs no agent and drives nothing, appending exactly one
/// terminal event. It is allowed for any non-terminal run; a run parked at a
/// dangling write is abandoned with the outstanding write recorded as
/// `unresolved_write`, so the receipt states plainly that the write stays
/// unresolved. Refuses (exit 1) a run that is already terminal.
pub async fn abandon(store_path: &Path, args: AbandonArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    if store.read_log(run_id).await?.is_empty() {
        bail!("no run {uuid} in this store");
    }

    let runtime = Runtime::new(store.clone());
    match runtime.abandon(run_id, args.reason).await {
        Ok(_) => {
            // Re-read to report the appended position and the recorded
            // unresolved-write evidence, straight off the terminal event.
            let log = store.read_log(run_id).await?;
            let appended_seq = log.last().map_or(0, |env| env.seq.get());
            let unresolved = match log.last().map(|env| &env.event) {
                Some(Event::RunAbandoned {
                    unresolved_write: Some(write),
                    ..
                }) => Some((write.seq.get(), write.tool.as_str())),
                _ => None,
            };
            print!(
                "{}",
                render::abandoned_report(
                    &uuid,
                    appended_seq,
                    unresolved,
                    render::DEFAULT_REPORT_WIDTH,
                )
            );
            Ok(0)
        }
        // Refusing an already-terminal run is a deliberate refusal, not an
        // internal error: exit 1 with an explanation.
        Err(RuntimeError::AlreadyTerminal { status, .. }) => {
            eprintln!(
                "run {uuid} is already terminal (status: {status}); there is nothing left to abandon"
            );
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// `salvor completions <shell>`: print a completion script on stdout.
///
/// Generated from the same `clap::Command` the parser is built from, not a hand-written list, so a
/// new subcommand or flag completes the moment it exists and cannot drift out of date. That is the
/// whole reason to generate rather than ship a static script.
///
/// It takes no store and reads nothing: shells source this at startup, so it must not touch a
/// database, and the exit code is always 0 unless writing to stdout fails.
pub fn completions(args: CompletionsArgs) -> Result<u8> {
    let mut command = <crate::cli::Cli as clap::CommandFactory>::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
    Ok(0)
}

/// `salvor list`: one row per run, with status folded from each log, narrowed by whatever filters
/// the caller gave.
///
/// Filtering happens after the fold because status is a replay-time projection, not a stored
/// column: there is nothing to filter on until each log has been read. That also means a filter
/// saves screen space, not work.
pub async fn list(store_path: &Path, args: ListArgs) -> Result<u8> {
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
        let state = derive_state(&log);
        let status = render::status_label(&state.status).to_owned();

        if !args.status.is_empty() && !args.status.iter().any(|s| s == &status) {
            continue;
        }
        if let Some(group) = &args.group
            && render::status_group(&status).map(render::StatusGroup::as_str)
                != Some(group.as_str())
        {
            continue;
        }
        if let Some(needle) = &args.agent {
            let identity = agent_identity(&log);
            if !identity.to_lowercase().contains(&needle.to_lowercase()) {
                continue;
            }
        }
        rows.push((summary, status));
    }

    // The tail, not the head: a limit exists to show the runs that matter now, and the newest sit
    // at the end of an ascending list. The surviving rows keep that order, so the shape of the
    // table never changes with the flag.
    if let Some(limit) = args.limit
        && rows.len() > limit
    {
        rows.drain(..rows.len() - limit);
    }

    if rows.is_empty() {
        println!("no runs matched");
        return Ok(0);
    }
    // anstream, not print!: it strips the table's styling when stdout is a pipe or a file, and
    // honours NO_COLOR and CLICOLOR, so redirected output stays plain text.
    anstream::print!("{}", render::list_table(&rows));
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

/// `salvor serve`: run the control-plane HTTP + server-sent-events server.
///
/// The server owns the same store every other command uses and drives runs
/// through the same runtime, so durability is identical to the local verbs.
/// The one piece the server does not own is the agent-definition format: this
/// command supplies the factory that parses a submitted definition (TOML or
/// JSON) with the CLI's own [`AgentConfig`] and builds it, so the schema keeps
/// its single home here. A submitted definition's relative paths (a prompt
/// file, a wasm component) resolve against the server's working directory.
///
/// `--kill` short-circuits this handler before the store opens or a port
/// binds: it never serves in the same invocation. That is a plain early
/// return rather than a clap `conflicts_with`, because `--store` lives on the
/// top-level [`crate::cli::Cli`] as a global flag, not on [`ServeArgs`], so
/// there is no single sibling argument for clap to name; a handler-level
/// check covers `--store` and `--bind` alike with one line, and keeps the
/// process-discovery flow ([`serve_kill`]) unit-testable on its own.
///
/// `--dev` adds a second process to the same invocation: the Angular dev
/// server (`ng serve`) for `bridge/`, hot module reloading included, proxying
/// `/v1` to the API this same command just bound. The API itself binds and
/// serves exactly as plain `serve` does; `--dev` only decides whether a
/// second process joins it and whether this handler waits on a shutdown
/// signal afterward to tear that second process down (see
/// [`DevServer::shutdown`]). The checkout it needs is found through
/// [`checkout::find_repo_root`], the same walk-up `salvor build` uses, so a
/// `--dev` outside a checkout fails before anything binds or spawns.
pub async fn serve(store_path: &Path, args: ServeArgs) -> Result<u8> {
    if let Some(target) = &args.kill {
        // `--kill` with no value arrives as `Some("")` (clap's
        // `default_missing_value`); that is the "no target, discover and
        // maybe prompt" case, not a literal empty target.
        let target = (!target.is_empty()).then_some(target.as_str());
        return serve_kill::run(target).await;
    }

    // Found and validated before the store opens or a port binds, so `--dev`
    // outside a checkout fails fast and honestly, before any other work.
    let bridge_dir = if args.dev {
        Some(
            checkout::find_repo_root()
                .context(
                    "--dev needs a salvor checkout with bridge/; the installed dashboard is \
                     embedded and does not hot-reload",
                )?
                .join("bridge"),
        )
    } else {
        None
    };

    let store = open_store(store_path)?;

    let factory: AgentFactory = Arc::new(|definition: AgentDefinition| {
        Box::pin(async move { build_from_definition(definition).await })
    });

    // The general model executor client-driven runs perform their model step
    // through, wired from the CLI's own client-construction path. The config is
    // built explicitly: the key keeps `Config::from_env`'s semantics
    // (`ANTHROPIC_API_KEY`, absent for a local endpoint, which sends no auth
    // header), and `SALVOR_MODEL_BASE_URL`, when set and non-empty, points the
    // executor at a local or offline endpoint speaking the same wire protocol
    // instead of the public one. Another host injects its own executor; this is
    // the out-of-the-box default, mirroring the agent factory.
    let mut model_config = salvor_llm::Config::from_env();
    if let Ok(base_url) = std::env::var("SALVOR_MODEL_BASE_URL")
        && !base_url.is_empty()
    {
        model_config = model_config.with_base_url(base_url);
    }
    let model_client = salvor_llm::Client::new(model_config)
        .context("building the model client for the client-driven model step")?;

    // The tool registry: EMPTY by default (the mechanism is wired, but
    // salvor serve ships no tools of its own — a tool-step or a graph `tool`
    // node for any name is a clean `unknown_tool` until a host registers
    // one, mirroring how the model executor is wired), or the deterministic
    // demo set when `--demo-tools` opts in. This is the one place that flag
    // is read; every other line of `serve` is unchanged whether it is
    // passed or not, so the stock, no-flag path stays byte-identical.
    let tool_registry = if args.demo_tools {
        #[cfg(feature = "fixture")]
        {
            tracing::info!(
                "demo tools registered: lookup_invoice (read), issue_refund (write), send_email \
                 (idempotent) — see salvor_cli::demo_tools"
            );
            crate::demo_tools::registry()
        }
        #[cfg(not(feature = "fixture"))]
        {
            bail!(
                "--demo-tools requires the `fixture` feature (this binary was built with \
                 --no-default-features); rebuild with the default features to use it"
            );
        }
    } else {
        ToolRegistry::new()
    };
    let mut state = AppState::new(store, factory)
        .with_model_executor(Arc::new(LlmModelExecutor::new(model_client)))
        .with_tool_registry(Arc::new(tool_registry));
    // The client-driven-run lease TTL: how long a client run reports an attached
    // driver after its last guarded operation. Default 60s (set in AppState);
    // `SALVOR_CLIENT_LEASE_TTL_SECS`, when a positive integer, shortens it so a
    // driverless client run becomes observable quickly (the stalled-run seed uses
    // this). A missing, empty, zero, or unparseable value leaves the default.
    if let Ok(raw) = std::env::var("SALVOR_CLIENT_LEASE_TTL_SECS")
        && let Ok(secs) = raw.parse::<u64>()
        && secs > 0
    {
        state = state.with_client_lease_ttl(std::time::Duration::from_secs(secs));
        tracing::info!(
            secs,
            "client-driven run lease TTL set from SALVOR_CLIENT_LEASE_TTL_SECS"
        );
    }
    if let Some(env_name) = &args.auth_token {
        match std::env::var(env_name) {
            Ok(token) if !token.is_empty() => {
                state = state.with_auth_token(token);
                tracing::info!("bearer auth required (token read from ${env_name})");
            }
            _ => tracing::warn!(
                "--auth-token names ${env_name}, but it is unset or empty; serving without auth"
            ),
        }
    }

    let listener = TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    println!("salvor control plane listening on http://{addr}");
    tracing::info!(%addr, "serving the control plane");

    let Some(bridge_dir) = bridge_dir else {
        // The plain path, unchanged from before `--dev` existed: no signal
        // handler installed here, so Ctrl-C or a `--kill` SIGTERM stops this
        // process the ordinary way, at the OS's default disposition.
        salvor_server::serve(listener, state)
            .await
            .context("serving the control plane")?;
        return Ok(0);
    };

    let dev = DevServer::start(&bridge_dir, addr).await?;
    println!(
        "dev UI (hot reload): http://localhost:{}/  <- open this one",
        dev.port()
    );
    println!("the API above stays reachable directly, e.g. for curl or an SDK");

    // A signal handler is installed only on this path: `--dev` is the one
    // case with a second process that must not outlive this one, so this is
    // the one case that needs to intercept the shutdown signal instead of
    // letting the OS's default disposition end the process immediately.
    // Whichever finishes first wins the race; the server future is simply
    // dropped on the signal branch (no in-flight request survives a plain
    // kill today either, `--dev` or not), then the dev server is torn down
    // before this returns, so `--kill` reliably reaps both.
    tokio::select! {
        result = salvor_server::serve(listener, state) => {
            result.context("serving the control plane")?;
        }
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received, stopping the dev server");
        }
    }
    dev.shutdown().await;
    Ok(0)
}

/// Waits for Ctrl-C (`SIGINT`) or, on Unix, `SIGTERM` (what `salvor serve
/// --kill` sends). Only [`serve`]'s `--dev` path awaits this: it is the one
/// case that needs to run cleanup code before the process exits, rather than
/// letting the OS's default signal disposition end it immediately.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("installing a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `salvor build`: build the whole product from a checkout.
///
/// It builds the web dashboard (the Bridge's production output) and then the
/// release binary, in that order, so the release binary embeds the dashboard
/// just produced. With `--install` it then installs that binary onto the PATH,
/// so the `salvor` a shell resolves carries the fresh dashboard.
///
/// The dashboard and the binary are built through a login shell (`bash -lc`) so
/// a node toolchain managed by nvm, and cargo under `~/.cargo/bin`, resolve the
/// same way they do at an interactive prompt. Every subprocess inherits this
/// process's streams, so the token gate and the compiler report scroll through
/// live.
pub async fn build(args: BuildArgs) -> Result<u8> {
    let root = checkout::find_repo_root()?;
    println!("salvor build: repo root at {}", root.display());

    let bridge = root.join("bridge");
    checkout::ensure_node_modules(&bridge).await?;
    println!("building the dashboard (npm run build)");
    checkout::run_shell(&bridge, "npm run build").await?;

    println!("building the release binary (cargo build --release -p salvor-cli)");
    checkout::run_shell(&root, "cargo build --release -p salvor-cli").await?;

    if args.install {
        println!("installing salvor onto the PATH (cargo install --path crates/salvor-cli)");
        checkout::run_shell(&root, "cargo install --path crates/salvor-cli").await?;
        println!(
            "salvor installed at {}",
            install_dir().join("salvor").display()
        );
    } else {
        println!("built {}", root.join("target/release/salvor").display());
        println!("run `salvor build --install` to put it on your PATH");
    }
    Ok(0)
}

/// The directory `cargo install` writes binaries to: `$CARGO_HOME/bin`, else
/// `$HOME/.cargo/bin`.
fn install_dir() -> PathBuf {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        PathBuf::from(cargo_home).join("bin")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cargo").join("bin")
    } else {
        PathBuf::from("~/.cargo/bin")
    }
}

/// `salvor agent hash <FILE>...`: print each agent definition's content hash.
///
/// The value printed is [`Agent::def_hash`], the same string a run records in
/// `RunStarted` and the same key [`graph_run`] resolves an `agent` node
/// against. It is produced by BUILDING each definition through
/// [`build_agents`], the way every other verb that takes `--agent` does, rather
/// than by hashing the file's bytes: tool schemas are part of the definition
/// and an MCP server supplies its own, so a byte hash of the TOML would be a
/// number no graph node could ever resolve.
///
/// One file prints the bare hash and nothing else, so `$(salvor agent hash
/// a.toml)` is usable as a value. Several print `<path>: <hash>` a line, in the
/// order given, since the question several files ask is which hash belongs to
/// which file.
///
/// This reads no store and starts no run. The MCP sessions the build opened are
/// closed before anything is printed, so stdout carries the hashes alone.
pub async fn agent_hash(args: AgentHashArgs) -> Result<u8> {
    let (agents, servers) = build_agents(&args.agents).await?;
    close_servers(servers).await;

    let bare = agents.len() == 1;
    for (path, agent) in args.agents.iter().zip(&agents) {
        if bare {
            println!("{}", agent.def_hash());
        } else {
            println!("{}: {}", path.display(), agent.def_hash());
        }
    }
    Ok(0)
}

/// `salvor graph edit`: fold typed lines into a graph document.
///
/// The editor itself is [`salvor_cli_core::graph_editor`], which performs no IO;
/// everything host-shaped about a session (the prompt, the streams, the three
/// lines that name a file) is [`crate::graph_edit`]. This handler is the seam
/// between the parse tree and that loop, and it takes no store, because an
/// author building a document has not started a run.
pub async fn graph_edit(args: crate::cli::GraphEditArgs) -> Result<u8> {
    crate::graph_edit::edit(args).await
}

/// `salvor graph validate <path>`: parse a graph document strictly and run
/// every validation check.
///
/// The exit-code contract matches the rest of the CLI: `Ok(0)` on a valid
/// document, `Ok(1)` for a document the tool deliberately refuses (unreadable
/// file, non-JSON, or a validation failure). Every failure prints a clear
/// message to stderr and never panics. On success the summary (node/edge counts
/// and entry/terminal nodes) prints to stdout.
///
/// This handler reads no store and drives no run: a graph document is
/// validated in isolation.
pub fn graph_validate(args: GraphValidateArgs) -> Result<u8> {
    let path = args.path.display().to_string();

    let text = match std::fs::read_to_string(&args.path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("cannot read graph file {path}: {error}");
            return Ok(1);
        }
    };

    // Strict parse: a stray field is a rejection, not a warning, because a graph
    // is a control document.
    let graph: salvor_graph::Graph = match serde_json::from_str(&text) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("{path} is not a valid graph document: {error}");
            return Ok(1);
        }
    };

    match salvor_graph::validate(&graph) {
        Ok(summary) => {
            print!("{}", render::graph_summary(&summary));
            Ok(0)
        }
        Err(errors) => {
            eprintln!("{path}: {} validation error(s):", errors.len());
            for error in &errors {
                eprintln!("  - {error}");
            }
            Ok(1)
        }
    }
}

/// `salvor graph schema`: print the graph document JSON Schema to stdout.
///
/// This is the single source of truth for the document format, so it is emitted
/// verbatim from the [`salvor_graph`] types with no store and no run involved.
pub fn graph_schema() -> Result<u8> {
    let schema = salvor_graph::graph_schema();
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(0)
}

/// `salvor graph run`: drive a graph document locally over the store, the graph
/// counterpart of [`run`].
///
/// It mirrors `salvor run`: strict-parse and validate the document (refusing an
/// invalid one before any run head is written), build every `--agent` file and
/// key it by its computed definition hash, and drive the engine over a fresh
/// `RunCtx`. The run id is printed first so a `kill -9` mid-run still leaves an
/// id to resume. On a park (a gate, a budget crossing) it prints how to continue
/// with `salvor resume ... --graph`.
///
/// # Resolution, and how it matches the server
///
/// An `agent` node resolves to a provided `--agent` file by that file's
/// definition hash; a hash matching none of them is a precise error listing what
/// was provided. A `tool` node resolves from the tools the provided agents
/// carry — the local counterpart of the server's tool registry, keeping one
/// honest story: a tool no provided agent carries is refused, named, before the
/// walk reaches it (as [`run_graph`] does through the resolver).
pub async fn graph_run(store_path: &Path, args: GraphRunArgs) -> Result<u8> {
    let graph = load_and_validate_graph(&args.graph)?;
    let input = parse_input(&args.input)?;
    let labels = parse_label_args(&args.labels)?;
    let store = open_store(store_path)?;
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    // Resolve everything the document references up front, before any run head is
    // written, so an unresolvable agent or tool is a precise refusal rather than
    // a run stranded at the offending node. This mirrors the server, which
    // resolves synchronously at graph-run submit.
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();
    // Printed first, so a kill mid-run still leaves the operator an id to resume.
    println!("run {uuid}");
    tracing::info!(run_id = %uuid, "starting graph run");

    let mut ctx = RunCtx::new(store.clone(), run_id, vec![])?;
    if let Some(labels) = labels {
        ctx = ctx.with_labels(labels);
    }
    let outcome = run_graph(&mut ctx, &graph, &input, &agents, &tools).await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, &uuid, &args.graph, &args.agents)
}

/// Re-drives a parked or crashed GRAPH run, for [`resume`]'s graph branch.
///
/// The log records only the graph's hash, so the document is re-supplied through
/// `--graph`; its hash must match the recorded one (a different document could
/// route the same log differently, which would be a silent divergence rather
/// than an honest refusal). The agent nodes' definitions are re-supplied through
/// `--agent`, exactly as an agent run re-supplies its one definition. A parked
/// run consumes `--input` at its suspension; a crashed run recovers with none.
async fn resume_graph(
    store: Arc<dyn EventStore>,
    run_id: RunId,
    uuid: &str,
    log: &[EventEnvelope],
    args: &ResumeArgs,
    disposition: Disposition,
) -> Result<u8> {
    let graph_path = args.graph.as_deref().context(
        "this is a graph run; pass --graph <graph.json> (its hash must match the recorded run) to \
         re-drive it, alongside the --agent files its agent nodes reference",
    )?;
    let graph = load_and_validate_graph(graph_path)?;
    let hash = graph_hash(&graph)?;
    let recorded =
        recorded_graph_hash(log).context("this graph run's log has no GraphRunStarted event")?;
    if hash != recorded {
        bail!(
            "the graph in {} hashes to {hash}, but run {uuid} recorded {recorded}; resume needs the \
             SAME document the run started with (submit the changed graph as a new run instead)",
            graph_path.display()
        );
    }
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    let mut ctx = RunCtx::new(store, run_id, log.to_vec())?;
    match disposition {
        Disposition::Resume(_) => {
            let raw = args.input.as_deref().context(
                "this run is parked awaiting input; pass --input <json|@file> to resume it",
            )?;
            ctx.set_resume_input(parse_input(raw)?);
            tracing::info!(run_id = %uuid, "resuming parked graph run");
        }
        _ => {
            if args.input.is_some() {
                tracing::warn!(
                    run_id = %uuid,
                    "this graph run crashed mid-step; --input is ignored when recovering"
                );
            }
            tracing::info!(run_id = %uuid, "recovering crashed graph run");
        }
    }
    // The recorded input wins on replay, so a bare null is fine here.
    let outcome = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools).await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, uuid, graph_path, &args.agents)
}

/// A [`ToolResolver`] over the provided agents' own tools: a graph `tool` node
/// resolves to the first provided agent that carries a tool of that name. This
/// is the local counterpart of the server's tool registry — the CLI has no
/// standalone tool inventory, so the tools come from the real agent definitions
/// the operator supplied.
struct AgentTools<'a>(&'a HashMap<String, Agent>);

impl ToolResolver for AgentTools<'_> {
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool> {
        self.0.values().find_map(|agent| agent.tools().get(name))
    }
}

/// Reads and strictly validates a graph document, refusing an invalid one with a
/// precise, node/edge-level error message (the same checks `graph validate`
/// runs).
fn load_and_validate_graph(path: &Path) -> Result<Graph> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading graph document {}", path.display()))?;
    let graph: Graph = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid graph document", path.display()))?;
    match salvor_graph::validate(&graph) {
        Ok(_) => Ok(graph),
        Err(errors) => {
            let mut message = format!("{}: {} validation error(s):", path.display(), errors.len());
            for error in &errors {
                message.push_str(&format!("\n  - {error}"));
            }
            bail!(message)
        }
    }
}

/// Builds every provided agent file, in the order given, and collects their MCP
/// sessions for the caller to keep alive and close.
///
/// The one place a `--agent` path becomes a live [`Agent`], so what
/// [`agent_hash`] prints, what [`build_graph_agents`] keys a graph node
/// against, and what `graph edit` resolves an `add agent --file` line to cannot
/// be three different numbers.
pub(crate) async fn build_agents(paths: &[PathBuf]) -> Result<(Vec<Agent>, Vec<McpServer>)> {
    let mut agents: Vec<Agent> = Vec::with_capacity(paths.len());
    let mut servers: Vec<McpServer> = Vec::new();
    for path in paths {
        let config = AgentConfig::load(path)?;
        let (agent, agent_servers) = agent_config::build_agent(&config, path).await?;
        agents.push(agent);
        servers.extend(agent_servers);
    }
    Ok((agents, servers))
}

/// Builds every provided agent file, keyed by its computed definition hash, and
/// collects their MCP sessions for the caller to keep alive and close.
async fn build_graph_agents(paths: &[PathBuf]) -> Result<(HashMap<String, Agent>, Vec<McpServer>)> {
    let (built, servers) = build_agents(paths).await?;
    let agents = built
        .into_iter()
        .map(|agent| (agent.def_hash().to_owned(), agent))
        .collect();
    Ok((agents, servers))
}

/// Checks every agent and tool the graph references resolves from what was
/// provided, before any run head is written. An `agent` node (or a
/// model-decision `branch`) whose hash matches no provided `--agent` fails with
/// the list of hashes that WERE provided; a `tool` node no provided agent
/// carries fails naming the node and tool.
fn check_graph_resolvable(graph: &Graph, agents: &HashMap<String, Agent>) -> Result<()> {
    for node in &graph.nodes {
        let referenced = match node {
            Node::Agent(agent) => Some((agent.id.as_str(), agent.agent_hash.as_str())),
            Node::Branch(branch) => branch
                .agent_hash
                .as_deref()
                .map(|hash| (branch.id.as_str(), hash)),
            _ => None,
        };
        if let Some((node_id, hash)) = referenced
            && !agents.contains_key(hash)
        {
            let provided: Vec<&str> = agents.keys().map(String::as_str).collect();
            let provided = if provided.is_empty() {
                "none".to_owned()
            } else {
                provided.join(", ")
            };
            bail!(
                "node `{node_id}` references agent `{hash}`, which none of the provided --agent \
                 files supply (provided: {provided})"
            );
        }
    }
    for node in &graph.nodes {
        if let Node::Tool(tool) = node
            && !agents
                .values()
                .any(|agent| agent.tools().get(&tool.tool).is_some())
        {
            bail!(
                "tool node `{}` names tool `{}`, which none of the provided agents carry",
                tool.id,
                tool.tool
            );
        }
    }
    Ok(())
}

/// What `--agent` matches against: the agent definition hash for an ordinary run, or the literal
/// `graph run` for a graph, which has no single agent to name.
///
/// The same distinction the web UI's agent column draws, and for the same reason: a graph run
/// genuinely has no one agent, so naming one would be a convenient lie.
///
/// Shared with [`crate::completion`], which offers these same identities as Tab candidates for the
/// flag: what completion offers and what the filter matches are one definition, so they cannot
/// drift into offering a value the filter would never match.
pub(crate) fn agent_identity(log: &[EventEnvelope]) -> String {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
            Event::GraphRunStarted { .. } => Some("graph run".to_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether a run's log is a graph run: its first event is `GraphRunStarted`.
fn is_graph_run(log: &[EventEnvelope]) -> bool {
    matches!(
        log.first().map(|envelope| &envelope.event),
        Some(Event::GraphRunStarted { .. })
    )
}

/// The `graph_hash` recorded in a graph run's `GraphRunStarted` head.
fn recorded_graph_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::GraphRunStarted { graph_hash, .. } => Some(graph_hash.clone()),
        _ => None,
    })
}

/// The single agent path an agent-run resume needs, or a clear refusal when
/// none or several were passed.
fn single_agent(agents: &[PathBuf]) -> Result<&Path> {
    match agents {
        [one] => Ok(one),
        [] => bail!("resuming an agent run needs its definition; pass --agent <file>"),
        _ => bail!(
            "an agent run resumes under exactly one --agent; pass --graph to resume a graph run \
             with multiple agents"
        ),
    }
}

/// Parses `key=value` label arguments into the map the runtime stamps, checking
/// the same bounds the server does. `None` when no labels were passed.
fn parse_label_args(labels: &[String]) -> Result<Option<BTreeMap<String, String>>> {
    if labels.is_empty() {
        return Ok(None);
    }
    let mut map = BTreeMap::new();
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .with_context(|| format!("label `{label}` must be key=value"))?;
        map.insert(key.to_owned(), value.to_owned());
    }
    validate_labels(&map).map_err(anyhow::Error::msg)?;
    Ok(Some(map))
}

/// Resolves the `--acknowledge-writes` argument to the set of origin log
/// positions the operator acknowledged: `all` expands to the full hazard set,
/// an omitted argument is the empty set, and anything else is a comma-separated
/// list of `u64` seqs. A non-numeric entry is a precise error.
fn parse_acknowledge_writes(arg: Option<&str>, hazard_seqs: &[u64]) -> Result<HashSet<u64>> {
    match arg.map(str::trim) {
        None | Some("") => Ok(HashSet::new()),
        Some("all") => Ok(hazard_seqs.iter().copied().collect()),
        Some(list) => list
            .split(',')
            .map(|item| {
                let item = item.trim();
                item.parse::<u64>().with_context(|| {
                    format!("`{item}` is not a valid log position; use `4,7` or `all`")
                })
            })
            .collect(),
    }
}

/// Renders the dry-run preview: what the fork would do, creating nothing.
fn render_fork_preview(plan: &salvor_engine::ForkPlan, missing: &[u64]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "fork of run {} from node `{}` (dry run):\n",
        plan.origin_run().as_uuid(),
        plan.from_node()
    ));
    out.push_str(&format!(
        "  prefix: {} event(s), through seq {}\n",
        plan.prefix_len(),
        plan.through_seq().get()
    ));
    if plan.hazards().is_empty() {
        out.push_str("  no writes in the re-walked segment; nothing to acknowledge.\n");
    } else {
        out.push_str(&format!(
            "  {} write(s) the re-walked segment would re-execute:\n",
            plan.hazards().len()
        ));
        for hazard in plan.hazards() {
            out.push_str(&render_hazard_line(hazard));
        }
        if missing.is_empty() {
            out.push_str("  all acknowledged; the fork would proceed.\n");
        } else {
            out.push_str(&format!(
                "  would refuse: seq(s) {} still need acknowledgement.\n",
                seq_list(missing)
            ));
        }
    }
    out
}

/// Renders the refusal report for an unacknowledged fork: the writes that would
/// re-fire, and how to acknowledge them. Mirrors the reconciliation report's
/// posture (show the intent, then how to proceed).
fn render_fork_refusal(origin: &str, from_node: &str, unacked: &[&WriteHazard]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "refused: forking run {origin} from node `{from_node}` would re-execute {} recorded \
         write(s) the segment re-walks:\n",
        unacked.len()
    ));
    for hazard in unacked {
        out.push_str(&render_hazard_line(hazard));
    }
    let seqs: Vec<u64> = unacked.iter().map(|hazard| hazard.seq).collect();
    out.push_str(&format!(
        "acknowledge that they may re-fire, then fork:\n  salvor fork {origin} --from-node \
         {from_node} --graph <graph.json> --agent <file>... --acknowledge-writes {}\n",
        seq_list(&seqs)
    ));
    out
}

/// One write-hazard line: its seq, tool, and recorded input.
fn render_hazard_line(hazard: &WriteHazard) -> String {
    format!(
        "    - seq {} `{}` input {}\n",
        hazard.seq,
        hazard.tool,
        render::pretty_json(&hazard.input).trim_end()
    )
}

/// A comma-separated seq list for a command hint.
fn seq_list(seqs: &[u64]) -> String {
    seqs.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Prints the result of a graph drive: the final output on completion, or a
/// parked report telling the operator how to continue with `salvor resume
/// --graph`. Both are exit code 0, exactly as an agent run's park is.
fn report_graph_outcome(
    outcome: GraphOutcome,
    uuid: &str,
    graph_path: &Path,
    agents: &[PathBuf],
) -> Result<u8> {
    match outcome {
        GraphOutcome::Completed { output } => {
            println!("{}", render::pretty_json(&output));
            Ok(0)
        }
        GraphOutcome::Parked { node, reason } => {
            println!("graph run {uuid} parked at node `{node}`.");
            match &reason {
                ParkReason::Suspended { reason, .. } => println!("  reason: {reason}"),
                ParkReason::BudgetExceeded { budget, observed } => {
                    println!("  budget crossed: {budget:?} (observed {observed})");
                }
            }
            let agent_flags: String = agents
                .iter()
                .map(|path| format!(" --agent {}", path.display()))
                .collect();
            println!(
                "resume it with:\n  salvor resume {uuid} --graph {}{agent_flags} --input <json>",
                graph_path.display()
            );
            Ok(0)
        }
    }
}

/// Builds a live agent from a submitted definition, for the `serve` factory.
/// Turns any failure into a human message the server maps to a `400`.
async fn build_from_definition(definition: AgentDefinition) -> Result<BuiltAgent, String> {
    let text = String::from_utf8(definition.body)
        .map_err(|_| "agent definition is not valid UTF-8".to_owned())?;
    let config = match definition.format {
        DefFormat::Toml => AgentConfig::from_toml_str(&text),
        DefFormat::Json => AgentConfig::from_json_str(&text),
    }
    .map_err(|error| format!("{error:#}"))?;

    // Relative paths in a submitted definition resolve against the server's
    // working directory; the pseudo path's parent is that directory.
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pseudo_path = base.join("agent-definition");
    let (agent, servers) = agent_config::build_agent(&config, &pseudo_path)
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(BuiltAgent { agent, servers })
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
            print!(
                "{}",
                render::parked_report(uuid, &reason, agent_path, render::DEFAULT_REPORT_WIDTH)
            );
            Ok(0)
        }
    }
}

/// Stops the fixture's scripted model, if there was one. A `None` (the
/// ordinary `--agent`/`--input` run) is a no-op, so the call site stays one
/// unconditional line next to `close_servers`.
async fn shutdown_model(model: Option<crate::fixture::FixtureModel>) {
    if let Some(model) = model {
        model.shutdown().await;
    }
}

/// Closes every MCP server session tidily. Errors are logged, not propagated:
/// the run already finished, so a teardown hiccup must not fail the command.
pub(crate) async fn close_servers(servers: Vec<salvor_tools::mcp::McpServer>) {
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
