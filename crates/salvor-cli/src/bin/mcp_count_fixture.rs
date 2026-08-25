//! A hermetic counting MCP server for the CLI integration tests.
//!
//! This is test-support code, gated behind the crate's `fixture` feature (on
//! by default), not part of the `salvor` product binary. It is a real MCP
//! server built from the rmcp server SDK, spawned as a child process over
//! stdio, exactly as a real MCP server would be. The CLI's own MCP client path
//! (`salvor_tools::mcp::McpServer`) speaks to it unchanged, so the tests
//! exercise the true spawn/initialize/list/call path, including the fresh
//! respawn a `resume` performs.
//!
//! # Why it writes to a file
//!
//! It exposes one tool, `record`, that appends its `line` argument to a file
//! whose path is given as the first command-line argument. The file is the
//! durable side-effect counter the kill test asserts on: each real execution
//! of `record` adds one line, so after a kill and resume the test counts lines
//! to prove the write executed exactly once, with zero duplicates. A counter
//! held in the server's memory would not survive the respawn a resume forces,
//! which is precisely why the count lives in a file outside the process.
//!
//! `record` carries no effect annotation, so a client presumes it is a
//! [`Write`](salvor_core::Effect::Write). The agent TOML in the tests pins it to
//! `write` explicitly through `effect_overrides`, which both documents the
//! operator trust decision and makes the write-ahead semantics unambiguous.
//!
//! # The parking tools
//!
//! Three more tools ask the calling run to park, through the `_meta.salvor`
//! extension the client reads off a tool result. Each appends its own line to
//! the same count file before returning, so the file keeps being the honest
//! witness: a replayed park that executed nothing adds no line, and a test can
//! say so by counting.
//!
//! - `hold` returns `_meta.salvor.sleep_until`, its own clock plus `seconds`.
//!   It also takes a `hold_id`, which is what an agent file can declare as its
//!   idempotency key, so a test can ask what the store's claim looks like
//!   while the run sleeps.
//! - `await_settlement` returns `_meta.salvor.suspend` with `kind: "signal"`
//!   and a schema asking for `{"paid": bool}`, the webhook wait.
//! - `bad_park` returns a park request with the key misspelled
//!   (`sleepUntil`), which the client must refuse rather than pass through as
//!   an ordinary result.
//!
//! # Reachability from the tests
//!
//! Because this bin builds under the default `fixture` feature, Cargo sets
//! `CARGO_BIN_EXE_salvor-mcp-count-fixture` when it builds the crate's tests, so
//! the integration test locates it with no path guessing and no external
//! program. The agent TOML the test generates names this path as the MCP
//! server `command`, with the count-file path as its one argument.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Meta, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The single argument `record` takes: the line to append durably.
#[derive(Debug, Deserialize, JsonSchema)]
struct RecordArgs {
    /// The line appended to the count file and echoed back in the result.
    line: String,
}

/// The arguments `hold` takes: how long to park, and the identity of the hold.
/// The id exists so an agent file has something to declare as the idempotency
/// key; the server itself makes no use of it beyond echoing it.
#[derive(Debug, Deserialize, JsonSchema)]
struct HoldArgs {
    /// Seconds from now until the run may continue. Negative is allowed, and
    /// is how a test stages a deadline that is already due.
    seconds: i64,
    /// What is being held.
    hold_id: String,
}

/// The resume input a suspending tool here asks for.
fn settlement_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"paid": {"type": "boolean"}},
        "required": ["paid"],
    })
}

/// Wraps a park request in the one `_meta` namespace the client reads.
fn salvor_meta(request: Value) -> Meta {
    let mut meta = Meta::new();
    meta.insert("salvor".to_owned(), request);
    meta
}

/// The fixture server. It holds the generated tool router and the path of the
/// file `record` appends to.
#[derive(Clone)]
struct Fixture {
    tool_router: ToolRouter<Self>,
    count_file: Arc<PathBuf>,
}

#[tool_router]
impl Fixture {
    fn new(count_file: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            count_file: Arc::new(count_file),
        }
    }

    /// Records one line by appending it to the count file. Unannotated, so a
    /// client presumes it writes.
    #[tool(description = "Record one line by appending it durably to the count file.")]
    async fn record(
        &self,
        Parameters(RecordArgs { line }): Parameters<RecordArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.count_file.as_ref())
            .and_then(|mut file| writeln!(file, "{line}"));
        match appended {
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "recorded: {line}"
            ))])),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record: {error}"
            ))])),
        }
    }

    /// Parks the calling run on a durable timer. Appends a line first, so the
    /// count file records that this execution really happened.
    #[tool(description = "Hold this work and park the calling run until later.")]
    async fn hold(
        &self,
        Parameters(HoldArgs { seconds, hold_id }): Parameters<HoldArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(error) = self.append(&format!("hold {hold_id}")) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record the hold: {error}"
            ))]));
        }
        let wake_at = OffsetDateTime::now_utc() + Duration::seconds(seconds);
        let wake_at = wake_at
            .format(&Rfc3339)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(
            CallToolResult::success(vec![ContentBlock::text(format!("holding until {wake_at}"))])
                .with_meta(Some(salvor_meta(json!({"sleep_until": wake_at})))),
        )
    }

    /// Parks the calling run until a settlement webhook reports back.
    #[tool(description = "Park the calling run until the settlement webhook reports back.")]
    async fn await_settlement(&self) -> Result<CallToolResult, ErrorData> {
        if let Err(error) = self.append("await_settlement") {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record the wait: {error}"
            ))]));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "waiting on the settlement webhook",
        )])
        .with_meta(Some(salvor_meta(json!({"suspend": {
            "reason": "waiting on the settlement webhook",
            "input_schema": settlement_schema(),
            "kind": "signal",
        }})))))
    }

    /// Asks for a park with the key misspelled. The client must fail the call
    /// naming `_meta.salvor` rather than hand the content back as output.
    #[tool(description = "Ask for a park with a misspelled key.")]
    async fn bad_park(&self) -> Result<CallToolResult, ErrorData> {
        if let Err(error) = self.append("bad_park") {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record the attempt: {error}"
            ))]));
        }
        Ok(
            CallToolResult::success(vec![ContentBlock::text("asking for a park")]).with_meta(Some(
                salvor_meta(json!({"sleepUntil": "2026-08-14T09:00:00Z"})),
            )),
        )
    }

    /// Appends one line to the count file. Every tool here that executes adds
    /// a line, which is what makes the file a count of real executions.
    fn append(&self, line: &str) -> std::io::Result<()> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.count_file.as_ref())
            .and_then(|mut file| writeln!(file, "{line}"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Salvor CLI counting fixture server.")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count_file = std::env::args()
        .nth(1)
        .expect("the count-file path is required as the first argument");
    let service = Fixture::new(PathBuf::from(count_file))
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
