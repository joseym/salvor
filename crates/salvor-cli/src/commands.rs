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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use salvor_core::{PendingCall, RunId, derive_state};
use salvor_runtime::{RunOutcome, Runtime, RuntimeError};
use salvor_server::dispatch::{Disposition, classify};
use salvor_server::{
    AgentDefinition, AgentFactory, AppState, BuiltAgent, DefFormat, LlmModelExecutor, ToolRegistry,
};
use salvor_store::{EventStore, SqliteStore};
use serde_json::Value;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::agent_config::{self, AgentConfig};
use crate::cli::{
    BuildArgs, GraphValidateArgs, HistoryArgs, ReplayArgs, ResolveArgs, ResumeArgs, RunArgs,
    ServeArgs,
};
use crate::render;
use crate::serve_kill;

/// `salvor run`: start a fresh run, print its id, drive it, and report.
pub async fn run(store_path: &Path, args: RunArgs) -> Result<u8> {
    let config = AgentConfig::load(&args.agent)?;
    let input = parse_input(&args.input)?;
    let store = open_store(store_path)?;

    let (agent, servers) = agent_config::build_agent(&config, &args.agent).await?;
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
                render::reconciliation_report(&uuid, state.pending_call.as_ref(), recorded_at)
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
        Disposition::NotStarted => bail!("run {uuid} has no recorded events"),
        Disposition::Resume(_) | Disposition::Recover => {}
    }

    let config = AgentConfig::load(&args.agent)?;
    let (agent, servers) = agent_config::build_agent(&config, &args.agent).await?;
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
    report_outcome(outcome?, &uuid, &args.agent)
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
            print!("{}", render::resolved_report(&uuid));
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
pub async fn serve(store_path: &Path, args: ServeArgs) -> Result<u8> {
    if let Some(target) = &args.kill {
        // `--kill` with no value arrives as `Some("")` (clap's
        // `default_missing_value`); that is the "no target, discover and
        // maybe prompt" case, not a literal empty target.
        let target = (!target.is_empty()).then_some(target.as_str());
        return serve_kill::run(target).await;
    }

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

    // An empty tool registry: the mechanism is wired, but salvor serve ships no
    // tools of its own. A tool-step for any name is a clean `unknown_tool` until
    // a host registers one, mirroring how the model executor is wired.
    let mut state = AppState::new(store, factory)
        .with_model_executor(Arc::new(LlmModelExecutor::new(model_client)))
        .with_tool_registry(Arc::new(ToolRegistry::new()));
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
    salvor_server::serve(listener, state)
        .await
        .context("serving the control plane")?;
    Ok(0)
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
    let root = find_repo_root()?;
    println!("salvor build: repo root at {}", root.display());

    let bridge = root.join("bridge");
    if !bridge.join("node_modules").is_dir() {
        println!("installing dashboard dependencies (npm ci)");
        run_shell(&bridge, "npm ci").await?;
    }
    println!("building the dashboard (npm run build)");
    run_shell(&bridge, "npm run build").await?;

    println!("building the release binary (cargo build --release -p salvor-cli)");
    run_shell(&root, "cargo build --release -p salvor-cli").await?;

    if args.install {
        println!("installing salvor onto the PATH (cargo install --path crates/salvor-cli)");
        run_shell(&root, "cargo install --path crates/salvor-cli").await?;
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

/// Walks up from the current directory for the workspace root: a directory with
/// a `Cargo.toml` that declares the salvor workspace and a sibling `bridge/`
/// tree. Names what it looked for when the search runs out.
fn find_repo_root() -> Result<PathBuf> {
    let start = std::env::current_dir().context("reading the current directory")?;
    let mut dir = start.as_path();
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file()
            && dir.join("bridge").is_dir()
            && std::fs::read_to_string(&cargo)
                .map(|text| text.contains("[workspace]") && text.contains("crates/salvor-cli"))
                .unwrap_or(false)
        {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "not inside a salvor checkout: walked up from {} and found no workspace \
                 Cargo.toml declaring the salvor members alongside a bridge/ directory",
                start.display()
            ),
        }
    }
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

/// Runs one build step in `dir` through a login shell, inheriting this
/// process's streams. Bails, naming the step, on a non-zero exit.
async fn run_shell(dir: &Path, line: &str) -> Result<()> {
    let status = tokio::process::Command::new("bash")
        .arg("-lc")
        .arg(line)
        .current_dir(dir)
        .status()
        .await
        .with_context(|| format!("spawning `{line}`"))?;
    if !status.success() {
        bail!("`{line}` failed ({status})");
    }
    Ok(())
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
            print!("{}", render::parked_report(uuid, &reason, agent_path));
            Ok(0)
        }
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
