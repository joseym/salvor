//! The research MCP server behind the `demo/` reference agent.
//!
//! Like `mcp_count_fixture`, this is demo-support code behind the crate's
//! default-on `fixture` feature, not part of the `salvor` product binary. It
//! is a real MCP server built from the rmcp server SDK, spawned over stdio
//! by `salvor run --agent demo/agent.toml`, so the demo exercises the true
//! spawn/initialize/list/call path, including the fresh respawn a resume
//! performs after a `kill -9`.
//!
//! # The tools
//!
//! - `search_notes` (annotated `readOnlyHint = true`, so the client maps it
//!   to `Effect::Read`): returns a canned research snippet keyed by the
//!   query. Deterministic and hermetic; there is no network anywhere.
//! - `save_finding` (no read-only or idempotent hint, so the client's
//!   conservative mapping lands on `Effect::Write`): appends one line to
//!   the findings file. This file is the demo's duplicate-detection
//!   witness: each real execution adds exactly one line, so counting lines
//!   before the kill and after the resume proves zero duplicate writes.
//! - `get_finding_count` (`readOnlyHint = true`, `Effect::Read`): reports
//!   how many findings the file holds.
//!
//! # The findings file
//!
//! The side-effect ledger must live outside the process because a resume
//! respawns this server; an in-memory counter would reset and prove
//! nothing. The path resolves in this order: the first command-line
//! argument, then the `SALVOR_DEMO_FINDINGS` environment variable, then
//! `findings.txt` under the working directory. The environment hook is what
//! lets the demo integration test point the same unmodified `agent.toml` at
//! a temporary file: the variable travels from the test through `salvor` to
//! this child process by ordinary environment inheritance.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The canned research library: one snippet per subtopic the demo agent is
/// prompted to investigate. Static data keeps the server hermetic and every
/// run of the demo byte-for-byte repeatable.
const NOTES: &[(&str, &str)] = &[
    (
        "event sourcing",
        "Event sourcing stores every state change as an immutable, append-only \
         event; current state is a fold over the log, so history is never lost.",
    ),
    (
        "write-ahead logging",
        "Write-ahead logging records the intent to change something before the \
         change happens, which makes a crash between intent and effect detectable.",
    ),
    (
        "idempotency keys",
        "An idempotency key lets a retried request collapse into the original \
         one, so at-least-once delivery behaves like exactly-once.",
    ),
    (
        "crash recovery",
        "Crash recovery replays durable history to rebuild in-flight state; \
         completed work is read from the log and never redone.",
    ),
    (
        "replay determinism",
        "Replay only works when orchestration code is deterministic: clocks, \
         randomness, and IO must be recorded once and read back on replay.",
    ),
    (
        "suspension and approval",
        "A workflow that needs a human parks durably and resumes when input \
         arrives; being nothing but rows in a store, it can wait for weeks.",
    ),
    (
        "budget enforcement",
        "Budgets checked between steps from recorded usage stop runaway loops \
         without killing the run; a human can extend the limit and resume.",
    ),
    (
        "side-effect classification",
        "Classifying effects as read, idempotent, or write lets a runtime retry \
         freely, retry with a key, or refuse to guess, respectively.",
    ),
    (
        "process supervision",
        "Supervisors restart crashed processes, but restarts alone lose \
         in-flight work; durable state is what turns a restart into a resume.",
    ),
];

/// The argument `search_notes` takes.
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// The subtopic to look up in the archived research notes.
    query: String,
}

/// The argument `save_finding` takes.
#[derive(Debug, Deserialize, JsonSchema)]
struct SaveArgs {
    /// One sentence to append durably to the findings file.
    finding: String,
}

/// The demo research server: the generated tool router plus the findings
/// file path resolved at startup.
#[derive(Clone)]
struct Research {
    tool_router: ToolRouter<Self>,
    findings_file: Arc<PathBuf>,
}

#[tool_router]
impl Research {
    fn new(findings_file: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            findings_file: Arc::new(findings_file),
        }
    }

    /// Read-only lookup into the canned library. The match is a
    /// case-insensitive substring check so a query like "notes on event
    /// sourcing" still finds its snippet.
    #[tool(
        description = "Search the archived research notes for a subtopic and return the matching snippet.",
        annotations(read_only_hint = true)
    )]
    async fn search_notes(
        &self,
        Parameters(SearchArgs { query }): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let needle = query.to_lowercase();
        for (topic, snippet) in NOTES {
            if needle.contains(topic) {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "[{topic}] {snippet}"
                ))]));
            }
        }
        let topics: Vec<&str> = NOTES.iter().map(|(topic, _)| *topic).collect();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "No archived notes match \"{query}\". Known subtopics: {}.",
            topics.join(", ")
        ))]))
    }

    /// The write: appends one line to the findings file. Deliberately
    /// carries no read-only or idempotent hint, so a client following the
    /// conservative annotation mapping treats it as a Write and records
    /// its intent before executing it.
    #[tool(description = "Save one finding by appending it durably to the findings file.")]
    async fn save_finding(
        &self,
        Parameters(SaveArgs { finding }): Parameters<SaveArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.findings_file.as_ref())
            .and_then(|mut file| writeln!(file, "{finding}"));
        match appended {
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "saved: {finding}"
            ))])),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to save finding: {error}"
            ))])),
        }
    }

    /// Read-only count of the findings saved so far, straight from the
    /// file so it survives server respawns.
    #[tool(
        description = "Report how many findings have been saved so far.",
        annotations(read_only_hint = true)
    )]
    async fn get_finding_count(&self) -> Result<CallToolResult, ErrorData> {
        let count = std::fs::read_to_string(self.findings_file.as_ref())
            .map(|text| text.lines().count())
            .unwrap_or(0);
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "findings saved so far: {count}"
        ))]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Research {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Salvor demo research server: canned notes plus a durable findings file.",
        )
    }
}

/// Resolves the findings file path: first command-line argument, then the
/// `SALVOR_DEMO_FINDINGS` environment variable, then `findings.txt` in the
/// working directory.
fn findings_path() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    if let Ok(from_env) = std::env::var("SALVOR_DEMO_FINDINGS")
        && !from_env.is_empty()
    {
        return PathBuf::from(from_env);
    }
    PathBuf::from("findings.txt")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Serve over stdio: the salvor process owns this child's stdin/stdout as
    // the JSON-RPC stream and closes the session when the run finishes.
    let service = Research::new(findings_path()).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
