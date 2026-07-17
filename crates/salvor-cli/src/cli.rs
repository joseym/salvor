//! The command-line surface, as `clap` derive types.
//!
//! Keeping the parse tree in one module (separate from the handlers in
//! [`crate::commands`]) means the shape of the CLI reads top to bottom here,
//! and the handlers take already-parsed, typed arguments. The one global
//! option, `--store`, is defined once and shared by every subcommand.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Salvor: a durable execution runtime for AI agents.
#[derive(Debug, Parser)]
#[command(name = "salvor", version, about, long_about = None)]
pub struct Cli {
    /// Path to the SQLite event store.
    ///
    /// The precedence is flag, then the `SALVOR_STORE` environment variable,
    /// then the default: an explicit `--store` wins, else `SALVOR_STORE`,
    /// else `./salvor.db`.
    //
    // clap resolves that precedence itself from the `env` and `default_value`
    // attributes below; nothing in the handlers re-implements it.
    #[arg(
        long,
        global = true,
        env = "SALVOR_STORE",
        default_value = "./salvor.db",
        value_name = "PATH"
    )]
    pub store: PathBuf,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The verbs of the CLI.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a fresh run of an agent.
    Run(RunArgs),
    /// Continue an existing run: resume a parked one, or recover a crashed one.
    Resume(ResumeArgs),
    /// Fork a graph run from a node boundary into a NEW run, refusing to
    /// re-execute a recorded write the operator has not acknowledged.
    Fork(ForkArgs),
    /// Record the completion of a dangling write by hand, after verifying it.
    Resolve(ResolveArgs),
    /// List every run in the store.
    List,
    /// Print a run's event log.
    History(HistoryArgs),
    /// Re-derive a run's state from its log without executing anything.
    Replay(ReplayArgs),
    /// Run the control-plane HTTP + server-sent-events server over the store.
    Serve(ServeArgs),
    /// Build the whole product from a salvor checkout: the web dashboard, then
    /// the release binary that embeds it.
    Build(BuildArgs),
    /// Author-time graph document tools: validate a document, or print its
    /// JSON Schema. These read no store and drive no run.
    Graph {
        /// The graph subcommand to run.
        #[command(subcommand)]
        command: GraphCommand,
    },
}

/// The verbs under `salvor graph`.
#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Validate a graph document JSON file: parse it strictly and run every
    /// check, printing a summary on success or the precise node/edge errors on
    /// failure.
    Validate(GraphValidateArgs),
    /// Print the graph document JSON Schema to stdout.
    Schema,
    /// Drive a graph document locally over the store, exactly as `salvor run`
    /// drives an agent run: each `agent` node resolves to a provided `--agent`
    /// file (keyed by its computed definition hash), and each `tool` node
    /// resolves from the tools those agents carry.
    Run(GraphRunArgs),
}

/// Arguments to `graph validate`.
#[derive(Debug, Args)]
pub struct GraphValidateArgs {
    /// Path to the graph document (JSON).
    #[arg(value_name = "FILE")]
    pub path: PathBuf,
}

/// Arguments to `graph run`.
#[derive(Debug, Args)]
pub struct GraphRunArgs {
    /// Path to the graph document (JSON).
    #[arg(value_name = "FILE")]
    pub graph: PathBuf,
    /// The run input: a JSON value, or `@path` to read JSON from a file.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: String,
    /// An agent definition (TOML) an `agent` node may reference. Repeatable:
    /// each file is built and keyed by its computed definition hash, and a
    /// graph `agent_hash` that matches none of them fails with a precise
    /// message listing the hashes that were provided.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// A correlation tag `key=value`, recorded once on the run's
    /// `GraphRunStarted`. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
}

/// Arguments to `run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the agent definition (TOML).
    #[arg(long, value_name = "FILE")]
    pub agent: PathBuf,
    /// The run input: a JSON value, or `@path` to read JSON from a file.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: String,
}

/// Arguments to `resume`.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// The run id (a UUID) to continue.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Path to an agent definition (TOML), needed to rebuild the agent.
    /// Repeatable: an agent run needs exactly one; a graph run needs the files
    /// its `agent` nodes reference (zero or more).
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Path to the graph document (JSON), needed to re-drive a GRAPH run. The
    /// run's log records only the graph's hash, not the document, exactly as it
    /// records an agent by hash and not its definition; so a graph run's resume
    /// re-supplies the document here, the same way an agent run re-supplies its
    /// definition through `--agent`. Its hash must match the one the run
    /// recorded. Omit for an ordinary agent run.
    #[arg(long, value_name = "FILE")]
    pub graph: Option<PathBuf>,
    /// The resume input, required for a parked run: a JSON value, or `@path`.
    /// Ignored (with a warning) when recovering a crashed run.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: Option<String>,
}

/// Arguments to `fork`.
#[derive(Debug, Args)]
pub struct ForkArgs {
    /// The origin run id (a UUID) to fork.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// The node boundary to restart the fork from: the fork re-walks from this
    /// node, carrying the origin's events below it as an identical prefix.
    #[arg(long = "from-node", value_name = "NODE")]
    pub from_node: String,
    /// Path to the graph document (JSON) the origin ran. Re-supplied the same way
    /// a graph resume re-supplies it (the log records only the hash); its hash
    /// must match the recorded one, since a fork reuses the origin's graph
    /// unchanged.
    #[arg(long, value_name = "FILE")]
    pub graph: PathBuf,
    /// An agent definition (TOML) the graph's `agent` nodes reference.
    /// Repeatable, exactly as `graph run` and a graph `resume` take them.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Acknowledge the writes the re-walked segment would re-fire: a
    /// comma-separated list of origin log positions (`4,7`), or `all` to
    /// acknowledge the full hazard set. Recorded permanently into the child's
    /// fork origin. Omit when the fork boundary sits before any write.
    #[arg(long = "acknowledge-writes", value_name = "SEQ,SEQ|all")]
    pub acknowledge_writes: Option<String>,
    /// Print what the fork WOULD do (the hazard list and the would-be prefix
    /// summary) without creating a run.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `resolve`.
#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// The run id (a UUID) that needs reconciliation.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// The output to record for the dangling write, after verifying externally
    /// what it did: a JSON value, or `@path` to read JSON from a file. It is
    /// recorded verbatim as the tool's output, so replay never re-runs the
    /// write.
    #[arg(long, value_name = "JSON|@FILE")]
    pub output: String,
}

/// Arguments to `history`.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// The run id (a UUID) whose log to print.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Print the raw event envelopes as JSON instead of the pretty log.
    #[arg(long)]
    pub json: bool,
}

/// Arguments to `replay`.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The run id (a UUID) to re-derive state for.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Re-derive state from the log without executing anything. Required in
    /// this version: live replay is not yet available.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// After building, install the release binary onto the PATH with
    /// `cargo install --path crates/salvor-cli`, so the `salvor` you run from
    /// anywhere carries the dashboard just built.
    #[arg(long)]
    pub install: bool,
}

/// Arguments to `serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The address to bind, host and port.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: String,
    /// The NAME of an environment variable holding a shared-secret bearer
    /// token. When set (and the variable is non-empty), every request must
    /// carry `Authorization: Bearer <that value>`. When omitted, the server
    /// runs without auth, trusting a reverse proxy to guard it. Never the
    /// token itself, matching how agent files name key variables.
    #[arg(long, value_name = "ENV_VAR")]
    pub auth_token: Option<String>,
    /// Kill the running `salvor serve` instead of serving. With no value,
    /// discovers every running `salvor serve` (by inspecting the process
    /// table, since there are no pid files): zero found is reported and this
    /// exits; exactly one is killed; multiple print a numbered table and
    /// prompt for a choice. Given a value (a pid or a listening port), kills
    /// that one directly with no prompt. When present at all, this
    /// short-circuits before `--bind` or `--store` are acted on: the process
    /// never binds a port.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "PID|PORT")]
    pub kill: Option<String>,
    /// Also start the Angular dev server (`ng serve`) for `bridge/`, hot
    /// module reloading included, with `/v1` proxied to this API so a
    /// browser at the dev server's own URL calls straight through. This
    /// process's own bind/store handling is otherwise unchanged: the API
    /// binds and serves exactly as plain `serve` does. Requires a salvor
    /// checkout with a `bridge/` directory alongside it; the dashboard an
    /// installed `salvor` embeds is prebuilt and does not hot-reload.
    #[arg(long)]
    pub dev: bool,
}
