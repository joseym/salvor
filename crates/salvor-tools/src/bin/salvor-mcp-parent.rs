//! A stand-in parent process for the child-reaping tests. Not product code.
//!
//! The interesting failure this crate guards against is what happens to an MCP
//! server when the process that started it dies *without running any code*: a
//! `SIGKILL`, or on the terminal a signal it never handled. A test cannot prove
//! that about itself, because proving it means killing the process doing the
//! asserting. So the test kills this instead.
//!
//! What it does is deliberately thin, and all of it goes through the real
//! product path: build a [`Command`] for the MCP fixture server, hand it to
//! [`McpServer::connect`], and then hold the connection open forever. There is
//! no lifecycle handling here at all, which is the point. Everything that
//! happens to the child when this process is killed is what
//! `salvor_tools::mcp` arranged at spawn time, not something this file
//! contributed.
//!
//! Usage, from a test:
//!
//! ```text
//! salvor-mcp-parent <path-to-salvor-mcp-fixture>
//! ```
//!
//! Environment variables prefixed `SALVOR_MCP_FIXTURE_` are forwarded to the
//! server, so the test names the pid files and asks for the stubborn mode the
//! same way it does when it spawns the fixture directly. This process prints
//! `ready` on a line of its own once the session is live, so a test can wait
//! for the connection rather than for a duration.
//!
//! [`Command`]: tokio::process::Command
//! [`McpServer::connect`]: salvor_tools::mcp::McpServer

use std::io::Write;

use salvor_tools::mcp::{EffectOverrides, IdempotencyKeys, McpServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = std::env::args()
        .nth(1)
        .ok_or("usage: salvor-mcp-parent <path-to-salvor-mcp-fixture>")?;

    let mut command = tokio::process::Command::new(fixture);
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SALVOR_MCP_FIXTURE_") {
            command.env(key, value);
        }
    }

    // The product constructor, unmodified and unassisted. Whatever the child
    // process ends up carrying (its process group, kill-on-drop, a
    // parent-death signal on the platforms that have one) it got from here.
    // Bound, never read, and never dropped before this process is killed: a
    // dropped handle would tear the child down through the controlled path,
    // which is the other test's subject, not this one's.
    let _server = McpServer::connect(command, &EffectOverrides::new(), &IdempotencyKeys::new())
        .await
        .map_err(|error| format!("connecting to the fixture server: {error}"))?;

    println!("ready");
    std::io::stdout().flush()?;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
