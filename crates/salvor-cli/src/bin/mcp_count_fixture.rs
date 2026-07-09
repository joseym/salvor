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
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The single argument `record` takes: the line to append durably.
#[derive(Debug, Deserialize, JsonSchema)]
struct RecordArgs {
    /// The line appended to the count file and echoed back in the result.
    line: String,
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
