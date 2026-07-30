//! [`McpTool`]: one tool on a connected MCP server, exposed as a
//! [`DynTool`](crate::DynTool).

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient};
use salvor_core::Effect;
use serde_json::Value;

use crate::DynTool;
use crate::context::ToolCtx;
use crate::error::{HandlerError, ToolError};
use crate::outcome::ToolOutcome;

/// A single MCP-server tool, dispatched through the runtime like any native
/// tool.
///
/// Where a native tool goes through [`TypedTool`](crate::TypedTool), an MCP
/// tool implements [`DynTool`](crate::DynTool) directly, because its identity
/// is known only at runtime: the name, description, and JSON schema are the
/// server's own, read off the wire when [`McpServer`](super::McpServer)
/// connects. The [`Effect`] is decided at connection time by
/// [`effect_for`](super::effect_for) and frozen here.
///
/// # It holds a client peer, not the connection
///
/// The field that lets an `McpTool` reach its server is a
/// [`Peer<RoleClient>`], a cheap, cloneable handle to the live MCP session that
/// [`McpServer`](super::McpServer) owns. The server owns the connection's
/// lifecycle; the tool only borrows the ability to send requests on it. When
/// the owning [`McpServer`](super::McpServer) is closed or dropped, the session
/// ends and this tool's calls fail rather than talk to a dead child. That split
/// is what makes respawn-on-resume clean: a fresh [`McpServer`](super::McpServer)
/// hands out fresh tools, and the old ones simply stop working.
///
/// # Never suspends
///
/// MCP has no suspension concept, so [`call_json`](DynTool::call_json) never
/// returns [`ToolOutcome::Suspend`]. It resolves to
/// [`ToolOutcome::Output`] or an error.
pub struct McpTool {
    peer: Peer<RoleClient>,
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    effect: Effect,
}

impl McpTool {
    /// Builds a tool handle. Called by [`McpServer`](super::McpServer) once per
    /// tool the server reports; not part of the public constructor surface.
    pub(super) fn new(
        peer: Peer<RoleClient>,
        name: String,
        description: String,
        input_schema: Value,
        output_schema: Option<Value>,
        effect: Effect,
    ) -> Self {
        Self {
            peer,
            name,
            description,
            input_schema,
            output_schema,
            effect,
        }
    }
}

#[async_trait]
impl DynTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn effect(&self) -> Effect {
        self.effect
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        self.output_schema.clone()
    }

    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        // `_ctx` carries the attempt's idempotency key, but MCP's call has no
        // standard idempotency-key parameter, so v0.1 does not forward it. The
        // key still governs the runtime loop's retry decision from the event log;
        // the wire call itself is unkeyed. Named `_ctx` to make that explicit.

        // Structural validation, before any network hop: MCP arguments are a
        // JSON object or absent. A null means "no arguments"; a JSON object is
        // the argument map; anything else (a bare string, array, or number)
        // could never be MCP arguments, so it is the model's malformed input
        // and is rejected here without touching the server. This is the only
        // client-side check: there is no typed Input to validate against, and
        // this module embeds no JSON Schema validator, so semantic validation
        // against the declared schema is left to the server.
        // `CallToolRequestParams` is `#[non_exhaustive]`, so it is built through
        // its constructor, not a struct literal.
        let mut params = CallToolRequestParams::new(self.name.clone());
        if !input.is_null() {
            let map = serde_json::from_value(input).map_err(|source| ToolError::InvalidInput {
                tool: self.name.clone(),
                source,
            })?;
            params = params.with_arguments(map);
        }

        // A transport or protocol failure (the request never produced a tool
        // result) folds into Handler: there is no dedicated transport variant,
        // and the runtime loop already routes Handler failures through the effect's
        // retry policy, which is the right treatment for a call that did not
        // complete.
        let result = self
            .peer
            .call_tool(params)
            .await
            .map_err(|source| ToolError::Handler {
                tool: self.name.clone(),
                source: HandlerError::new(source),
            })?;

        // A tool-reported error (`isError == true`) is the MCP way of saying
        // "the tool ran and failed." That is precisely Handler semantics: the
        // work executed and returned a failure, distinct from the model sending
        // bad arguments. The result's text content becomes the error message.
        if result.is_error == Some(true) {
            let message = error_message(&result);
            return Err(ToolError::Handler {
                tool: self.name.clone(),
                source: HandlerError::message(message),
            });
        }

        // Success: hand back the whole tool result serialized to JSON. The
        // runtime loop records this value; keeping the full CallToolResult (content
        // plus any structured content and metadata) rather than digging out one
        // field means nothing the server returned is discarded before the log.
        let value =
            serde_json::to_value(&result).map_err(|source| ToolError::OutputSerialization {
                tool: self.name.clone(),
                source,
            })?;
        Ok(ToolOutcome::Output(value))
    }
}

/// Extracts a human-readable message from a tool-reported error result, joining
/// its text content. Falls back to a generic line when the server sent an error
/// flag with no text to explain it.
fn error_message(result: &rmcp::model::CallToolResult) -> String {
    let text: Vec<String> = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect();
    if text.is_empty() {
        "the MCP tool reported an error with no message".to_owned()
    } else {
        text.join("\n")
    }
}
