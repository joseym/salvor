//! Salvor CLI: `run`, `resume`, `list`, `history`, and `replay` over durable
//! agent runs.
//!
//! This library holds the CLI's logic so it can be unit-tested directly (the
//! TOML schema, the rendering) while [`main`](../salvor/index.html) stays a
//! thin shell that parses arguments, sets up tracing, and dispatches. The
//! modules split by concern:
//!
//! - [`cli`] is the `clap` parse tree.
//! - [`anchor`] is the anchor document `salvor anchor` writes and
//!   `salvor verify` reads back: a copy of every run's chain head, kept where
//!   the store cannot rewrite it along with itself.
//! - [`agent_config`] is the TOML agent-definition schema and the mapping into
//!   a live [`salvor_runtime::Agent`].
//! - [`commands`] is one handler per verb, wiring [`salvor_runtime`] and
//!   [`salvor_store`] together.
//! - [`completion`] is dynamic shell completion: the run ids and agent
//!   identities in the operator's own store, answered per Tab. It is additive
//!   to the static scripts `salvor completions <shell>` prints, which are
//!   unchanged.
//! - [`fixture`] is `salvor run --fixture <DIR>`: the offline, self-contained
//!   fixture directory (agent, input, and a recorded model conversation) and
//!   the in-process scripted model that serves it.
//! - [`graph_edit`] is the prompt loop behind `salvor graph edit`: the terminal
//!   and the filesystem around
//!   [`salvor_cli_core::graph_editor`], which is the editor itself and performs
//!   no IO.
//! - [`render`] is pure value-to-text formatting, shared by the commands.
//! - [`manifest`] walks clap's own `Command` tree into the machine-readable
//!   description checked in at `docs/cli-manifest.json`, for a consumer
//!   outside this repo that needs to know the CLI surface without
//!   reimplementing it by hand.
//! - [`serve_kill`] is the process discovery and termination behind `salvor
//!   serve --kill`, kept separate from [`commands::serve`] because it has
//!   nothing to do with actually serving.
//! - [`checkout`] is the salvor-checkout detection and login-shell subprocess
//!   helper shared by `salvor build` and `salvor serve --dev`.
//! - [`dev_server`] is the Angular dev server (`ng serve`) lifecycle behind
//!   `salvor serve --dev`'s hot reload.
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
pub mod anchor;
pub mod checkout;
pub mod commands;
pub mod completion;
#[cfg(feature = "fixture")]
pub mod demo_script;
#[cfg(feature = "fixture")]
pub mod demo_tools;
pub mod dev_server;
pub mod fixture;
pub mod graph_edit;
pub mod manifest;
pub mod render;
pub mod serve_kill;

/// The `clap` parse tree, which lives in [`salvor_cli_core`] so a browser
/// terminal can parse a command line with the real parser rather than a copy.
/// Re-exported here so `salvor_cli::cli::Cli` keeps naming it.
pub use salvor_cli_core::cli;

use anyhow::Result;

use crate::cli::{Cli, Command};

/// The filter installed when the operator has not set `RUST_LOG`: `info` for
/// every target except `rmcp`, held to `warn`. Any command that resolves an
/// agent definition can connect to an MCP server the definition declares,
/// exactly as a run would, and that handshake logs a handful of
/// `INFO serve_inner: ...` lines under the `rmcp` target that would otherwise
/// sit between the operator and whatever the command itself prints, on the
/// very first run before anyone has learned to set `RUST_LOG` at all.
const DEFAULT_LOG_DIRECTIVE: &str = "info,rmcp=warn";

/// The directive `init_tracing` filters by: `rust_log` verbatim when the
/// operator set `RUST_LOG`, [`DEFAULT_LOG_DIRECTIVE`] otherwise. Split out
/// from `init_tracing` so the precedence is checkable without installing the
/// process-global subscriber, which installs at most once.
fn log_directive(rust_log: Option<&str>) -> &str {
    rust_log.unwrap_or(DEFAULT_LOG_DIRECTIVE)
}

/// Installs the tracing subscriber: human-readable events to stderr, filtered
/// by `RUST_LOG` when set, [`DEFAULT_LOG_DIRECTIVE`] otherwise. Stderr keeps
/// stdout clean for command output. Idempotent enough for tests: a second
/// call is a no-op rather than a panic.
pub fn init_tracing() {
    use std::io::IsTerminal;

    use tracing_subscriber::{EnvFilter, fmt};

    let rust_log = std::env::var("RUST_LOG").ok();
    let filter = EnvFilter::new(log_directive(rust_log.as_deref()));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        // Color for a human at a terminal; none when the log is piped or
        // captured, so fields like `seq=7` stay greppable.
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

/// The name to record as the caller on the events a verb writes: the global
/// `--caller` when given, else the operating system user this process runs as.
///
/// The account is read from the environment rather than from a system call:
/// `USER` on Unix, `USERNAME` on Windows, which is the pair every shell sets
/// and the only source available without a dependency that reaches for
/// `getpwuid`. A process started with neither set (a bare container, a daemon
/// with a scrubbed environment) resolves to `None` and records no name, which
/// is the honest answer rather than a guess. An empty value is treated the
/// same way as an unset one.
///
/// The name is a label on the events, never a credential. Anyone who can write
/// the store can write any name into it; what makes a name worth something is
/// the server verifying a token before it stamps one.
#[must_use]
pub fn caller_name(flag: Option<&str>) -> Option<String> {
    if let Some(name) = flag {
        return non_empty(name);
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .and_then(|name| non_empty(&name))
}

/// The value when it carries something, `None` when it is blank.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Runs the parsed command, returning the process exit code.
///
/// # Errors
///
/// Propagates any handler failure; the caller reports it and exits non-zero.
pub async fn dispatch(cli: Cli) -> Result<u8> {
    let store = cli.store.as_path();
    // Who to record on the events these verbs write. Resolved once, here,
    // rather than per handler, so every verb answers the question the same
    // way. See [`caller_name`].
    let caller = caller_name(cli.caller.as_deref());
    let caller = caller.as_deref();
    match cli.command {
        Command::Run(args) => commands::run(store, caller, args).await,
        Command::Resume(args) => commands::resume(store, caller, args).await,
        Command::Wake(args) => commands::wake(store, caller, args).await,
        // `fork` copies the origin's recorded envelopes verbatim under a new
        // run id and then recovers the child, so it writes no event with a
        // caller field of its own: restamping copied bytes would rewrite what
        // the origin recorded, and a graph run's head carries no such field.
        Command::Fork(args) => commands::fork(store, args).await,
        Command::Resolve(args) => commands::resolve(store, caller, args).await,
        Command::Abandon(args) => commands::abandon(store, caller, args).await,
        Command::List(args) => commands::list(store, args).await,
        Command::Completions(args) => commands::completions(args),
        Command::History(args) => commands::history(store, args).await,
        Command::Replay(args) => commands::replay(store, args).await,
        // `anchor` writes, and reads the file it would overwrite: checking a
        // store against that file is the same read `verify` does, so it awaits
        // the same way.
        Command::Anchor(args) => commands::anchor(store, args).await,
        Command::Verify(args) => commands::verify(store, args).await,
        Command::Serve(args) => commands::serve(store, args).await,
        // `build` produces the product from a checkout; it reads no store.
        Command::Build(args) => commands::build(args).await,
        // `agent hash` reads no store and starts no run: it builds the
        // definitions it is given and prints what a run would record them as.
        Command::Agent { command } => match command {
            crate::cli::AgentCommand::Hash(args) => commands::agent_hash(args).await,
            crate::cli::AgentCommand::Validate(args) => commands::agent_validate(args).await,
        },
        Command::Graph { command } => match command {
            // `edit`, `validate` and `schema` read no store and drive no run,
            // so they ignore the store path; `run` drives a graph over the
            // store, exactly as `salvor run` drives an agent run. Only `edit`
            // is awaited among the three, because resolving an agent node's
            // `--file` builds the definition and that connects to whatever MCP
            // servers it declares.
            crate::cli::GraphCommand::Edit(args) => commands::graph_edit(args).await,
            crate::cli::GraphCommand::Validate(args) => commands::graph_validate(args),
            crate::cli::GraphCommand::Schema => commands::graph_schema(),
            crate::cli::GraphCommand::Run(args) => commands::graph_run(store, args).await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rust_log_defaults_to_info_with_rmcp_held_to_warn() {
        assert_eq!(log_directive(None), DEFAULT_LOG_DIRECTIVE);
    }

    #[test]
    fn an_explicit_rust_log_wins_verbatim() {
        assert_eq!(log_directive(Some("debug,rmcp=trace")), "debug,rmcp=trace");
        assert_eq!(log_directive(Some("off")), "off");
    }
}
