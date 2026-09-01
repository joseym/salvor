//! The `salvor` binary: a thin shell over the [`salvor_cli`] library.
//!
//! It does four things and no more: install tracing, parse the command line,
//! dispatch on a Tokio runtime, and turn the result into a process exit code.
//! All logic worth testing lives in the library, driven either by unit tests
//! or, end to end, by integration tests that spawn this very binary.

use std::process::ExitCode;

use clap::FromArgMatches;
use salvor_cli::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    // Before tracing, before parsing, before anything reaches stdout: a
    // completion request is answered by an environment variable and argv that
    // the parser would (rightly) refuse, and its whole output is the candidate
    // list a shell reads. See `salvor_cli::completion`.
    if salvor_cli::completion::try_complete() {
        return ExitCode::SUCCESS;
    }
    salvor_cli::init_tracing();
    // Not `Cli::parse()`: the tree is built first so the two global options
    // can be hidden from the one verb that cannot act on either. Parsing is
    // otherwise unchanged, and both options still parse everywhere.
    let matches = cli::command_hiding_unusable_globals().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    match salvor_cli::dispatch(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // `{:#}` prints the whole anyhow context chain on one line.
            eprintln!("salvor: {error:#}");
            ExitCode::FAILURE
        }
    }
}
