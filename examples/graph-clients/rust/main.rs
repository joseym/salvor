//! Example: the dispute-refund graph, driven by an application that EMBEDS the
//! engine.
//!
//! The Python and TypeScript apps beside this one are HTTP clients: they post a
//! document to a control plane, start a run, answer a gate, and read the result
//! back over the wire. From Rust the runtime is a library, so this app does the
//! same story with no server in the middle. It opens a store, builds the two
//! agents itself, and calls [`salvor_engine::run_graph`] directly. There is no
//! HTTP here at all, and this process binds no port.
//!
//! What the Python and TypeScript apps do over a protocol, this one does by
//! calling the same code the server would have called. The document, the agent
//! definitions, the model script, and the ledger are identical across all
//! three.
//!
//! # What it needs from its caller
//!
//! - **The working directory must be the repository root.** Both agent TOMLs
//!   name their MCP server as `examples/graph-service/server.py`, a path
//!   relative to the process that spawns it, not to the TOML. Every other path
//!   this app reads is relative to the root too. It checks this first and
//!   refuses with a message rather than failing later inside a child process.
//! - **`SALVOR_DEMO_BASE_URL` must point at a running `salvor-demo-model`.**
//!   Both agents read their endpoint from that variable, so an unset variable
//!   would silently aim at the real Anthropic endpoint and an unreachable one
//!   would surface as a connection error three layers down. Checked up front.
//! - **`SALVOR_DISPUTES_LEDGER` and `SALVOR_DISPUTES_NOTICES`** should point at
//!   scratch paths. The MCP server reads them from the environment it inherits;
//!   this app reads the ledger back to count refunds.
//!
//! `examples/graph-clients/run.sh` supplies all of that.
//!
//! # Run it
//!
//! ```sh
//! # from the repository root, with the offline model already up:
//! cargo run --quiet -p salvor-cli --example graph_clients -- /tmp/graph-clients-rust.db
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use salvor_cli::agent_config::{self, AgentConfig};
use salvor_core::{Event, EventEnvelope, RunId, event_kind};
use salvor_engine::{GraphOutcome, ToolResolver, graph_hash, run_graph};
use salvor_graph::{Graph, Node};
use salvor_runtime::{Agent, ParkReason, RunCtx};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use salvor_tools::mcp::McpServer;
use serde_json::{Value, json};

/// The graph document all three apps drive.
const GRAPH_PATH: &str = "examples/graph-clients/dispute-refund.json";

/// The escalated dispute: above the desk's threshold, so it needs a signature.
const ESCALATED_INPUT: &str = "examples/graph-clients/input.json";

/// The small dispute: below the threshold, so it settles itself.
const AUTO_SETTLE_INPUT: &str = "examples/graph-clients/input-small.json";

/// The MCP server both agents declare, by a path relative to THIS process's
/// working directory. Checked up front so a wrong directory is a message rather
/// than a python traceback out of a child process.
const MCP_SERVER: &str = "examples/graph-service/server.py";

/// Which agent file backs which agent node. The document names the agents by
/// content hash only, so this table is the app's own claim about which file it
/// believes supplies which node, and the hash check below is what turns that
/// claim into a verified fact.
const AGENT_FILES: [(&str, &str); 2] = [
    (
        "settle_and_notify",
        "examples/graph-clients/agents/settle-and-notify.toml",
    ),
    (
        "small_claims",
        "examples/graph-clients/agents/small-claims.toml",
    ),
];

/// A [`ToolResolver`] over the built agents' own tools, exactly as the CLI does
/// it. This graph has no `tool` node, so nothing ever asks: the resolver is here
/// because `run_graph` takes one, and handing it the agents' real tools means a
/// `tool` node added to the document later would resolve.
struct AgentTools<'a>(&'a HashMap<String, Agent>);

impl ToolResolver for AgentTools<'_> {
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool> {
        self.0.values().find_map(|agent| agent.tools().get(name))
    }
}

/// The approver's answer to the gate. In a real desk this arrives from a human
/// through whatever inbox the application offers; here it is the same literal
/// the Python and TypeScript apps post over HTTP, so all three produce the same
/// ledger line.
fn approval() -> Value {
    json!({
        "dispute_id": "DSP-4471",
        "amount_usd": 512.0,
        "approver": "j.okafor",
        "note": "Duplicate charge confirmed against INV-90210.",
    })
}

/// Refuses, with an actionable message, anything the run needs and does not
/// have: the working directory, the offline model, and the two files the agents
/// spawn and read.
///
/// Every one of these would otherwise fail deep inside a child process or a
/// provider call, where the error names none of the causes.
async fn preflight() -> Result<String> {
    let cwd = std::env::current_dir().context("reading the working directory")?;
    for required in [GRAPH_PATH, MCP_SERVER, ESCALATED_INPUT, AUTO_SETTLE_INPUT] {
        if !Path::new(required).exists() {
            bail!(
                "cannot find {required} from {}.\nThis example must run from the REPOSITORY ROOT: \
                 the agent definitions name their MCP server as `{MCP_SERVER}`, a path relative to \
                 the process that spawns it. cd to the root and re-run, or use \
                 examples/graph-clients/run.sh, which does it for you.",
                cwd.display()
            );
        }
    }
    for (_, file) in AGENT_FILES {
        if !Path::new(file).exists() {
            bail!(
                "cannot find the agent definition {file} from {}",
                cwd.display()
            );
        }
    }

    let base_url = std::env::var("SALVOR_DEMO_BASE_URL").unwrap_or_default();
    if base_url.trim().is_empty() {
        bail!(
            "SALVOR_DEMO_BASE_URL is unset or empty.\nBoth agents read their endpoint from that \
             variable, so without it they would call the public Anthropic endpoint and need a real \
             key. Start the offline model and export it:\n  target/debug/salvor-demo-model --port \
             18961 --delay-ms 20 --script examples/graph-clients/model-script.json &\n  export \
             SALVOR_DEMO_BASE_URL=http://127.0.0.1:18961\nOr just run \
             examples/graph-clients/run.sh, which does both."
        );
    }
    check_model_reachable(&base_url).await?;
    Ok(base_url)
}

/// Opens a TCP connection to the model endpoint and closes it again, so an
/// endpoint that is not listening is reported here by name instead of as a
/// provider error inside the first agent node.
async fn check_model_reachable(base_url: &str) -> Result<()> {
    let rest = base_url
        .strip_prefix("http://")
        .or_else(|| base_url.strip_prefix("https://"))
        .unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or(rest).trim_end_matches('/');
    let target = if authority.contains(':') {
        authority.to_owned()
    } else if base_url.starts_with("https://") {
        format!("{authority}:443")
    } else {
        format!("{authority}:80")
    };

    let attempt = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(&target),
    )
    .await;
    let failure = match attempt {
        Ok(Ok(_stream)) => return Ok(()),
        Ok(Err(error)) => error.to_string(),
        Err(_) => "the connection attempt timed out after 2s".to_owned(),
    };
    bail!(
        "SALVOR_DEMO_BASE_URL is {base_url}, but nothing is listening on {target}: \
         {failure}.\nStart the offline model first:\n  target/debug/salvor-demo-model --port \
         <port> --delay-ms 20 --script examples/graph-clients/model-script.json &\nOr run \
         examples/graph-clients/run.sh, which starts it and exports the variable."
    );
}

/// Moves any store left over from a previous invocation aside, so the counts
/// this app prints mean what they say.
///
/// Moved, never deleted: a run that failed halfway is still worth reading
/// afterwards, and this app should not be the thing that destroys the only copy
/// of it. `run.sh` treats the store it owns the same way.
fn clean_store(path: &Path) -> Result<()> {
    let mut moved = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", path.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}.prev", from.display()));
            std::fs::rename(&from, &to)
                .with_context(|| format!("moving {} aside to {}", from.display(), to.display()))?;
            moved.push(to);
        }
    }
    if moved.is_empty() {
        println!(
            "  store {} is fresh; nothing to move aside.",
            path.display()
        );
    } else {
        for to in &moved {
            println!("  moved a previous store aside to {}", to.display());
        }
    }
    Ok(())
}

/// Reads the document and validates it, refusing an invalid one with every
/// error the validator found rather than the first.
fn load_and_validate_graph(path: &str) -> Result<Graph> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading graph document {path}"))?;
    let graph: Graph = serde_json::from_str(&text)
        .with_context(|| format!("{path} is not a valid graph document"))?;
    match salvor_graph::validate(&graph) {
        Ok(summary) => {
            println!(
                "  graph ok: {} node(s), {} edge(s)",
                summary.node_count, summary.edge_count
            );
            println!("  entry:    {}", summary.entry_nodes.join(", "));
            println!("  terminal: {}", summary.terminal_nodes.join(", "));
            Ok(graph)
        }
        Err(errors) => {
            let mut message = format!("{path}: {} validation error(s):", errors.len());
            for error in &errors {
                message.push_str(&format!("\n  - {error}"));
            }
            bail!(message)
        }
    }
}

/// The `agent_hash` the document names for one agent node.
fn declared_agent_hash<'a>(graph: &'a Graph, node_id: &str) -> Result<&'a str> {
    graph
        .nodes
        .iter()
        .find_map(|node| match node {
            Node::Agent(agent) if agent.id == node_id => Some(agent.agent_hash.as_str()),
            _ => None,
        })
        .with_context(|| format!("{GRAPH_PATH} has no agent node `{node_id}`"))
}

/// Builds both agents from their TOMLs and verifies each computed definition
/// hash against the hash the document names for that node.
///
/// This is the same desync guard `run.sh` performs, and it matters more here
/// than it does over HTTP: a control plane resolves an agent hash at submit and
/// refuses up front, but an app that built the wrong definition and never
/// checked would only find out when the walk reached the node, with a run head
/// already written. Checking here keeps a drifted TOML a startup failure.
///
/// The returned servers are the live MCP sessions the tools hold peers into.
/// The caller keeps them in scope for as long as the agents are used and closes
/// them afterwards.
async fn build_and_verify_agents(
    graph: &Graph,
) -> Result<(HashMap<String, Agent>, Vec<McpServer>)> {
    let mut agents: HashMap<String, Agent> = HashMap::new();
    let mut servers: Vec<McpServer> = Vec::new();

    for (node_id, file) in AGENT_FILES {
        let path = Path::new(file);
        let config = AgentConfig::load(path)?;
        let (agent, agent_servers) = agent_config::build_agent(&config, path).await?;
        servers.extend(agent_servers);

        let declared = declared_agent_hash(graph, node_id)?;
        let computed = agent.def_hash();
        if declared != computed {
            // Loud, and naming both numbers and the file, because the fix
            // depends on which side moved: either the TOML changed and the
            // document needs the new hash, or the edit was not meant.
            bail!(
                "HASH MISMATCH for agent node `{node_id}`:\n  {GRAPH_PATH} claims: \
                 {declared}\n  {file} hashes:  {computed}\nThe document and the definition have \
                 drifted apart. Update the agent_hash in {GRAPH_PATH}, or revert the edit to \
                 {file}."
            );
        }
        println!("  {node_id} -> {computed} (matches {file})");
        agents.insert(computed.to_owned(), agent);
    }

    Ok((agents, servers))
}

/// Closes every MCP session tidily. A teardown hiccup is reported, never fatal:
/// whatever the run recorded is already durable.
async fn close_servers(servers: Vec<McpServer>) {
    for server in servers {
        if let Err(error) = server.close().await {
            println!("  (an MCP server did not shut down cleanly: {error})");
        }
    }
}

/// Reads a dispute record from one of the checked-in input files.
fn read_input(path: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading input {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("{path} is not valid JSON"))
}

/// Prints the recorded walk: one line per event, sequence number and kind, the
/// same reduction the README shows for the HTTP apps' event stream.
fn print_walk(log: &[EventEnvelope]) {
    for envelope in log {
        println!(
            "  {:>2}  {}",
            envelope.seq.get(),
            event_kind(&envelope.event)
        );
    }
}

/// Prints every `BranchTaken` in the log: the sole recorded authority for which
/// way a branch went, never inferred from which siblings were skipped.
fn print_branches(log: &[EventEnvelope]) {
    for envelope in log {
        if let Event::BranchTaken { node, case } = &envelope.event {
            println!(
                "  seq {}: BranchTaken node=`{node}` case=`{case}`",
                envelope.seq.get()
            );
        }
    }
}

/// Prints every `NodeSkipped` in the log, with the reason the engine recorded.
/// A skipped node WAS reached on the walk; a node that appears in no event was
/// never reached at all, which is a different fact.
fn print_skips(log: &[EventEnvelope]) {
    let mut any = false;
    for envelope in log {
        if let Event::NodeSkipped { node, reason } = &envelope.event {
            any = true;
            println!(
                "  seq {}: NodeSkipped node=`{node}` reason: {reason}",
                envelope.seq.get()
            );
        }
    }
    if !any {
        println!("  (none: every node on the walk executed)");
    }
}

/// Where the MCP server appends its refund lines. The same default the server
/// itself falls back to, so the count below is read from the file the tool
/// actually wrote.
fn ledger_path() -> String {
    std::env::var("SALVOR_DISPUTES_LEDGER").unwrap_or_else(|_| "refund-ledger.txt".to_owned())
}

/// Reads the ledger's non-blank lines, in order.
fn read_ledger_lines() -> Vec<String> {
    std::fs::read_to_string(ledger_path())
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_owned())
        .collect()
}

/// Prints the refund ledger, numbered, and returns how many lines it holds.
///
/// This is the SHARED ledger: `run.sh` drives the Python, TypeScript, and this
/// Rust app in sequence against one ledger file, so by the time this app runs,
/// the file may already carry lines neither of its two runs wrote. The callers
/// below therefore check deltas rather than this count.
fn print_ledger() -> usize {
    let path = ledger_path();
    let lines = read_ledger_lines();
    println!(
        "  ledger {path} ({} line(s) total, from every app that has run):",
        lines.len()
    );
    if lines.is_empty() {
        println!("    (empty: no refund has been written)");
    } else {
        for (index, line) in lines.iter().enumerate() {
            println!("    {}  {line}", index + 1);
        }
    }
    lines.len()
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(store_path) = args.next() else {
        bail!(
            "usage: graph_clients <store path>\nThe Rust app embeds the engine rather than talking \
             to a control plane, so it takes a STORE PATH where the other two apps take a base \
             URL. Run it through examples/graph-clients/run.sh, or by hand from the repository \
             root:\n  cargo run --quiet -p salvor-cli --example graph_clients -- \
             /tmp/graph-clients-rust.db"
        );
    };
    let store_path = PathBuf::from(store_path);

    println!("== checking what this app needs before it starts anything ==");
    let base_url = preflight().await?;
    println!(
        "  working directory: {}",
        std::env::current_dir()?.display()
    );
    println!("  offline model:     {base_url} (reachable)");
    println!("  refund ledger:     {}", ledger_path());
    println!(
        "  notices:           {}",
        std::env::var("SALVOR_DISPUTES_NOTICES").unwrap_or_else(|_| "customer-notices.json".into())
    );

    println!();
    println!(
        "== starting from a clean store at {} ==",
        store_path.display()
    );
    clean_store(&store_path)?;

    println!();
    println!("== validating {GRAPH_PATH} ==");
    let graph = load_and_validate_graph(GRAPH_PATH)?;
    let hash = graph_hash(&graph)?;
    println!("  graph hash: {hash}");

    println!();
    println!("== building both agents and verifying the hashes the document claims ==");
    let (agents, servers) = build_and_verify_agents(&graph).await?;

    // `run.sh` drives the Python, TypeScript, and this Rust app in sequence
    // against ONE shared ledger file, so this ledger may already carry lines
    // neither of this app's two runs wrote. Snapshotting the count now,
    // immediately before this app's own first run, is what lets the checks
    // below claim only what this app itself did.
    let ledger_before_escalated = read_ledger_lines().len();

    // ------------------------------------------------------------------
    // The escalated dispute: above the threshold, so it parks for a human.
    // ------------------------------------------------------------------
    println!();
    println!("== run 1: the ESCALATED dispute ==");
    let escalated = read_input(ESCALATED_INPUT)?;
    println!("  input: {escalated}");

    let store: Arc<dyn EventStore> = Arc::new(
        SqliteStore::open(&store_path)
            .with_context(|| format!("opening store at {}", store_path.display()))?,
    );
    let escalated_run = RunId::new();
    // Printed before the drive, exactly as `salvor graph run` prints it: a
    // `kill -9` mid-run still leaves an id to resume with.
    println!("  run {}", escalated_run.as_uuid());

    let mut ctx = RunCtx::new(store.clone(), escalated_run, vec![])?;
    let tools = AgentTools(&agents);
    let outcome = run_graph(&mut ctx, &graph, &escalated, &agents, &tools).await?;

    let GraphOutcome::Parked { node, reason } = outcome else {
        bail!(
            "the escalated dispute was expected to park at the gate, but the graph ran straight \
             through. Check that {ESCALATED_INPUT} still carries an amount_usd at or above the \
             document's threshold."
        );
    };
    let ParkReason::Suspended {
        reason,
        input_schema,
    } = reason
    else {
        bail!("the escalated run parked at `{node}`, but on a budget rather than the gate");
    };
    println!("  parked at node `{node}`.");
    println!("  the gate's prompt, as the log recorded it:");
    println!("    {reason}");
    println!("  the shape of the answer it wants:");
    println!("{}", serde_json::to_string_pretty(&input_schema)?);
    print_branches(&store.read_log(escalated_run).await?);

    // ------------------------------------------------------------------
    // Drop everything, then resume from the store alone.
    //
    // What this demonstrates: nothing about the park is carried across in
    // memory. Below, the MCP child processes are closed, the agents are
    // dropped, the RunCtx is dropped, and the store handle is dropped. The
    // second drive then opens the SQLite file by path, reads the log back out
    // of it, and rebuilds the context from those bytes and the run id alone.
    // The only thing that survives the tear-down is the id, which is exactly
    // what a human answering an approval would be handed.
    //
    // What it does NOT demonstrate: that the park survives the PROCESS dying.
    // This is still one process, so a leaked in-memory cache would not be
    // caught here. `examples/graph-service/run.sh` does prove that version: it
    // kills the driver outright and resumes from a new one. Read that for the
    // stronger claim.
    // ------------------------------------------------------------------
    println!();
    println!("== tearing the runtime down before answering the gate ==");
    drop(ctx);
    drop(agents);
    close_servers(servers).await;
    drop(store);
    println!("  closed the MCP sessions, dropped the agents, the context, and the store handle.");
    println!("  all that is left is the run id and the file on disk.");
    println!("  (still one process: this shows the park is not held in memory, NOT that it");
    println!("   survives the process dying. examples/graph-service/run.sh kills the driver");
    println!("   outright and resumes from a new one, which is the stronger proof.)");

    println!();
    println!("== run 1, second drive: answering the gate from the store alone ==");
    let store: Arc<dyn EventStore> = Arc::new(
        SqliteStore::open(&store_path)
            .with_context(|| format!("reopening store at {}", store_path.display()))?,
    );
    let (agents, servers) = build_and_verify_agents(&graph).await?;
    let log = store.read_log(escalated_run).await?;
    println!(
        "  read {} recorded event(s) back for run {}.",
        log.len(),
        escalated_run.as_uuid()
    );
    let mut ctx = RunCtx::new(store.clone(), escalated_run, log)?;
    // The approver's answer. A gate passes its resume input through as its
    // output, so this IS the refund instruction the next node executes.
    ctx.set_resume_input(approval());
    println!("  approver's answer: {}", approval());
    let tools = AgentTools(&agents);
    // The engine reads the graph input back out of the log on replay, so a
    // bare null is all this call needs.
    let outcome = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools).await?;
    let GraphOutcome::Completed {
        output: escalated_output,
    } = outcome
    else {
        bail!("the escalated run parked a second time instead of completing");
    };
    println!("  completed.");

    let log = store.read_log(escalated_run).await?;
    println!();
    println!("== run 1: the recorded walk ==");
    print_walk(&log);
    println!("  which case the branch fired:");
    print_branches(&log);
    println!("  which nodes the walk reached and skipped:");
    print_skips(&log);

    println!();
    println!("== run 1: the refund ==");
    let after_escalated = print_ledger();
    // An application should make claims about its own effects, not about
    // global file state it does not own: this ledger is shared with whichever
    // apps ran before this one, so its ABSOLUTE size is none of this app's
    // business. What this run DID own is one approval, executed exactly once,
    // so the only honest assertion is that the ledger grew by exactly one line
    // relative to the snapshot taken before this app touched it.
    let escalated_delta = after_escalated as i64 - ledger_before_escalated as i64;
    if escalated_delta != 1 {
        bail!(
            "expected the ledger to grow by exactly ONE line for the escalated run's approval, \
             but it grew by {escalated_delta} (from {ledger_before_escalated} to \
             {after_escalated}). The approval ran more or fewer times than once, or something \
             else wrote to {} between this app's snapshot and now.",
            ledger_path()
        );
    }
    println!(
        "  ledger grew by exactly 1 line (this app's own line): the approval was executed once, \
         not replayed. ({ledger_before_escalated} line(s) belonged to other apps or earlier runs \
         before this one started.)"
    );

    // ------------------------------------------------------------------
    // The small dispute: below the threshold, so it never suspends.
    // ------------------------------------------------------------------
    println!();
    println!("== run 2: the AUTO_SETTLE dispute ==");
    // Snapshotted again, immediately before this app's second run, for the
    // same reason as above: the delta this run owns is measured from here, not
    // from zero and not from run 1's count (which itself included whatever
    // other apps had already written).
    let ledger_before_small = read_ledger_lines().len();
    let small = read_input(AUTO_SETTLE_INPUT)?;
    println!("  input: {small}");
    let small_run = RunId::new();
    println!("  run {}", small_run.as_uuid());
    let mut ctx = RunCtx::new(store.clone(), small_run, vec![])?;
    let tools = AgentTools(&agents);
    let outcome = run_graph(&mut ctx, &graph, &small, &agents, &tools).await?;
    let GraphOutcome::Completed {
        output: small_output,
    } = outcome
    else {
        bail!(
            "the auto-settle dispute parked, which it must never do: it is below the threshold, so \
             the gate is on the arm the branch did not take"
        );
    };
    println!("  completed in one drive: it never suspended, so nothing had to answer it.");

    let small_log = store.read_log(small_run).await?;
    println!();
    println!("== run 2: the recorded walk ==");
    print_walk(&small_log);
    println!("  which case the branch fired:");
    print_branches(&small_log);
    println!("  the evidence it never suspended: the two escalated nodes were skipped,");
    println!("  and no Suspended event was ever recorded.");
    print_skips(&small_log);
    let suspensions = small_log
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::Suspended { .. }))
        .count();
    println!("  Suspended events in run 2: {suspensions}");
    if suspensions != 0 {
        bail!("the auto-settle run suspended {suspensions} time(s); it must never suspend");
    }

    println!();
    println!("== the ledger, after both runs ==");
    let after_small = print_ledger();
    let small_delta = after_small as i64 - ledger_before_small as i64;
    if small_delta != 1 {
        bail!(
            "expected the ledger to grow by exactly ONE line for the auto-settle run's refund, but \
             it grew by {small_delta} (from {ledger_before_small} to {after_small}): one refund per \
             dispute, no more, no fewer."
        );
    }
    println!(
        "  ledger grew by exactly 1 line (this app's own line): one refund per dispute, each \
         written once. Across both of this app's runs it wrote exactly 2 lines of its own into a \
         ledger that, by now, also holds every other app's lines."
    );

    println!();
    println!("== the final output of both runs ==");
    println!("  escalated (DSP-4471):   {escalated_output}");
    println!("  auto-settle (DSP-3312): {small_output}");

    close_servers(servers).await;
    Ok(())
}
