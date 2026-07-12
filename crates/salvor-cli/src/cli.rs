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
}

/// Arguments to `graph validate`.
#[derive(Debug, Args)]
pub struct GraphValidateArgs {
    /// Path to the graph document (JSON).
    #[arg(value_name = "FILE")]
    pub path: PathBuf,
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
    /// Path to the agent definition (TOML), needed to rebuild the agent.
    #[arg(long, value_name = "FILE")]
    pub agent: PathBuf,
    /// The resume input, required for a parked run: a JSON value, or `@path`.
    /// Ignored (with a warning) when recovering a crashed run.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: Option<String>,
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
}
