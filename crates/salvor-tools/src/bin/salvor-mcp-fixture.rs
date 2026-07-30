//! A hermetic MCP server used by the `mcp` integration tests.
//!
//! This is not product code and not a mock: it is a real MCP server, built
//! from the rmcp server SDK, that the integration tests spawn as a child
//! process and speak to over stdio, exactly as [`McpServer`] speaks to a real
//! server. Testing against a spawned child (rather than an in-process duplex
//! transport) is deliberate: the product path is "spawn a server process and
//! speak MCP over its stdin/stdout," and respawn-on-resume means constructing a
//! fresh child, so the test exercises the same spawn, initialize, list, call,
//! and shutdown path the runtime uses. It stays hermetic because the binary is
//! this repository's own, built behind the `mcp` feature; there is no network
//! and no external program.
//!
//! It exposes exactly the tools the effect-mapping and round-trip tests need:
//!
//! - `read_note` is annotated `readOnlyHint = true`, so it must map to
//!   [`Effect::Read`](salvor_core::Effect::Read).
//! - `append_note` is annotated `idempotentHint = true` (and not read-only), so
//!   it must map to [`Effect::Idempotent`](salvor_core::Effect::Idempotent). It
//!   takes an argument and echoes it, which drives the round-trip test.
//! - `mutate` carries no annotations, so it must fall through to the safe
//!   default [`Effect::Write`](salvor_core::Effect::Write).
//! - `explode` returns a tool-reported error result (`isError == true`), which
//!   must surface on the client as
//!   [`ToolError::Handler`](salvor_tools::ToolError::Handler).
//! - `stamp_receipt` returns structured output (`rmcp::Json<T>`), so the SDK
//!   publishes an `outputSchema` for it; this is what the
//!   `output_schema`-surfacing test connects to.
//!
//! [`McpServer`]: salvor_tools::mcp::McpServer

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The single argument `append_note` takes. Deriving `JsonSchema` lets the
/// `#[tool]` macro publish an input schema for it; deriving `Deserialize` lets
/// the server parse the client's arguments into it.
#[derive(Debug, Deserialize, JsonSchema)]
struct AppendArgs {
    /// The line to append. Echoed back in the result so the round-trip test can
    /// see its own input come through the server.
    line: String,
}

/// The structured result `stamp_receipt` returns. Deriving `JsonSchema` is
/// what makes the SDK publish an `outputSchema` for the tool that returns it.
#[derive(Debug, Serialize, JsonSchema)]
struct Receipt {
    /// A settlement id standing in for a verifiable provider reference.
    settlement_id: String,
}

/// The fixture server. It holds nothing but the generated tool router.
#[derive(Clone)]
struct Fixture {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Fixture {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// A read-only tool: it observes state and changes nothing.
    #[tool(
        description = "Read the note. Observes state only.",
        annotations(read_only_hint = true)
    )]
    async fn read_note(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "the note says hello",
        )]))
    }

    /// An idempotent tool: repeating it with the same input has no extra
    /// effect. It echoes its argument so a caller can confirm the round trip.
    #[tool(
        description = "Append a line to the note. Idempotent for a given line.",
        annotations(idempotent_hint = true)
    )]
    async fn append_note(
        &self,
        Parameters(AppendArgs { line }): Parameters<AppendArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "appended: {line}"
        ))]))
    }

    /// An unannotated tool: the fixture states no hints, so the client must
    /// presume it writes.
    #[tool(description = "Do something with unstated effects.")]
    async fn mutate(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text("mutated")]))
    }

    /// A tool that always fails at the tool level: it returns a result flagged
    /// `isError`, the MCP way of saying "the tool ran and failed."
    #[tool(description = "Always fails with a tool-reported error.")]
    async fn explode(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "boom: the explode tool always fails",
        )]))
    }

    /// A tool that returns structured output: the SDK derives its
    /// `outputSchema` from `Receipt`'s `JsonSchema` impl.
    #[tool(description = "Stamp a settlement receipt.")]
    async fn stamp_receipt(&self) -> Result<Json<Receipt>, ErrorData> {
        Ok(Json(Receipt {
            settlement_id: "settlement-123".to_owned(),
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        // Advertise the tools capability so a capability-checking client knows
        // this server offers tools. The tool list itself comes from the router
        // the `#[tool_handler]` macro wires up.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Salvor MCP integration-test fixture server.")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Serve over stdio: the client owns this process's stdin/stdout as the
    // JSON-RPC stream. `waiting` blocks until the client closes the session
    // (which it does on shutdown), at which point the process exits.
    let service = Fixture::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
