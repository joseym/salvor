//! Salvor CLI: `run`, `resume`, `list`, `history`, and `replay --dry-run` over
//! durable agent runs.
//!
//! This library holds the CLI's logic so it can be unit-tested directly (the
//! TOML schema, the rendering) while [`main`](../salvor/index.html) stays a
//! thin shell that parses arguments, sets up tracing, and dispatches. The
//! modules split by concern:
//!
//! - [`cli`] is the `clap` parse tree.
//! - [`agent_config`] is the TOML agent-definition schema and the mapping into
//!   a live [`salvor_runtime::Agent`].
//! - [`commands`] is one handler per verb, wiring [`salvor_runtime`] and
//!   [`salvor_store`] together.
//! - [`render`] is pure value-to-text formatting, shared by the commands.
//! - [`serve_kill`] is the process discovery and termination behind `salvor
//!   serve --kill`, kept separate from [`commands::serve`] because it has
//!   nothing to do with actually serving.
//!
//! # Where the durability comes from
//!
//! Nothing here reimplements the runtime. The CLI is a thin operator surface
//! over `salvor-runtime`: `run` maps to [`Runtime::start`](salvor_runtime::Runtime::start),
//! and `resume` maps to either [`Runtime::resume`](salvor_runtime::Runtime::resume)
//! (a parked run) or [`Runtime::recover`](salvor_runtime::Runtime::recover) (a
//! crashed one), chosen by folding the recorded log with
//! [`derive_state`](salvor_core::derive_state). The kill/resume guarantee is the
//! runtime's; the CLI only exposes it.

#![warn(missing_docs)]

pub mod agent_config;
pub mod cli;
pub mod commands;
#[cfg(feature = "fixture")]
pub mod demo_script;
pub mod render;
pub mod serve_kill;

use anyhow::Result;

use crate::cli::{Cli, Command};

/// Installs the tracing subscriber: human-readable events to stderr, filtered
/// by `RUST_LOG` (default `info`). Stderr keeps stdout clean for command
/// output. Idempotent enough for tests: a second call is a no-op rather than a
/// panic.
pub fn init_tracing() {
    use std::io::IsTerminal;

    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        // Color for a human at a terminal; none when the log is piped or
        // captured, so fields like `seq=7` stay greppable.
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

/// Runs the parsed command, returning the process exit code.
///
/// # Errors
///
/// Propagates any handler failure; the caller reports it and exits non-zero.
pub async fn dispatch(cli: Cli) -> Result<u8> {
    let store = cli.store.as_path();
    match cli.command {
        Command::Run(args) => commands::run(store, args).await,
        Command::Resume(args) => commands::resume(store, args).await,
        Command::Resolve(args) => commands::resolve(store, args).await,
        Command::List => commands::list(store).await,
        Command::History(args) => commands::history(store, args).await,
        Command::Replay(args) => commands::replay(store, args).await,
        Command::Serve(args) => commands::serve(store, args).await,
        // `build` produces the product from a checkout; it reads no store.
        Command::Build(args) => commands::build(args).await,
        // Graph document tooling reads no store and drives no run, so these
        // handlers are synchronous and ignore the store path.
        Command::Graph { command } => match command {
            crate::cli::GraphCommand::Validate(args) => commands::graph_validate(args),
            crate::cli::GraphCommand::Schema => commands::graph_schema(),
        },
    }
}
