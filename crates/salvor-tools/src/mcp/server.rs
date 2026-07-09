//! [`McpServer`]: an owned connection to one MCP server, spawned as a child
//! process and spoken to over stdio.

use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;
use tokio::process::Command;

use super::{EffectOverrides, McpTool, effect_for};

/// A live connection to one MCP server.
///
/// Constructing an `McpServer` spawns the server as a child process, runs the
/// MCP initialize handshake, and lists the server's tools, turning each into an
/// [`McpTool`]. The handle then owns that connection's lifecycle: the tools it
/// produced hold cheap clones of the client peer, so this handle must stay
/// alive for as long as those tools are dispatched. Closing or dropping it ends
/// the session and stops the child.
///
/// # Transport: stdio over a spawned child
///
/// v0.1 speaks MCP over stdio to a child process. The child's stdin and stdout
/// carry the JSON-RPC stream; its stderr
/// is inherited so a misbehaving server's diagnostics reach the operator.
/// Remote transports (HTTP) are not supported by this handle.
///
/// # Respawn on resume is safe
///
/// An MCP server child process is not itself durable: it holds no run state,
/// and it is not resumable. On resume, the Salvor runtime does not reconnect to
/// an old child; it constructs a fresh `McpServer`, which spawns a new child
/// and lists tools again. That is safe, and the reason is the replay contract,
/// not anything this handle does: a completed tool call is read from the event
/// log and never re-executed on resume, regardless of effect class. See
/// [`Effect`](salvor_core::Effect), whose documentation states that rule, and
/// the replay cursor in `salvor-core` that enforces it. So a respawned server is
/// only ever asked to run calls that had *not* completed. A crash between a
/// recorded write intent and its completion does not silently re-fire against
/// the new child; it surfaces for human reconciliation. Respawning is therefore
/// just reconstruction, with no risk of a duplicated side effect.
///
/// # Shutdown
///
/// [`close`](Self::close) ends the session and waits for the child to be torn
/// down. Dropping the handle without calling `close` still ends the session:
/// the underlying connection cancels itself on drop and the child is stopped,
/// though asynchronously and without a chance to observe errors, so `close` is
/// the tidy path.
pub struct McpServer {
    service: RunningService<RoleClient, ()>,
    tools: Vec<McpTool>,
}

impl McpServer {
    /// Spawns `command` as an MCP server, initializes the session, lists its
    /// tools, and returns the connected handle.
    ///
    /// `command` is a fully prepared [`Command`]: program, arguments, and
    /// environment are the caller's to set, so this handle stays free of any
    /// policy about where servers come from. Each listed tool's [`Effect`] is
    /// decided by [`effect_for`](super::effect_for): the server's annotation
    /// hints, unless `overrides` pins the tool's name, in which case the
    /// operator's override wins.
    ///
    /// [`Effect`]: salvor_core::Effect
    pub async fn connect(command: Command, overrides: &EffectOverrides) -> Result<Self, McpError> {
        // `TokioChildProcess` spawns the child with stdin/stdout piped (the
        // JSON-RPC stream) and stderr inherited.
        let transport = TokioChildProcess::new(command).map_err(McpError::Spawn)?;

        // `()` is the no-op client handler: this client offers the server no
        // capabilities of its own, it only calls tools. `serve` runs the
        // initialize handshake and returns once the session is live.
        let service = ().serve(transport).await.map_err(|e| McpError::Initialize(Box::new(e)))?;

        // `list_all_tools` pages through the server's tool list until the
        // cursor is exhausted, so a server that paginates is handled.
        let listed = service
            .list_all_tools()
            .await
            .map_err(McpError::ListTools)?;

        let peer = service.peer().clone();
        let tools = listed
            .into_iter()
            .map(|tool| {
                let name = tool.name.to_string();
                let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
                // The server's input schema is a JSON object; surface it as a
                // `Value` so it sits alongside a native tool's schema unchanged.
                let input_schema = Value::Object((*tool.input_schema).clone());
                let effect = effect_for(&name, tool.annotations.as_ref(), overrides);
                McpTool::new(peer.clone(), name, description, input_schema, effect)
            })
            .collect();

        Ok(Self { service, tools })
    }

    /// The tools the server reported, for inspection before registering them.
    ///
    /// Each is an [`McpTool`] carrying the server's own name, description, and
    /// JSON schema, and the [`Effect`](salvor_core::Effect) chosen at connect.
    /// [`take_tools`](Self::take_tools) moves them out for registration.
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Moves the tools out of the handle so they can be registered into a
    /// [`ToolSet`](crate::ToolSet).
    ///
    /// The handle keeps owning the connection after this: the moved tools hold
    /// live client-peer clones, so they keep working as long as this
    /// `McpServer` is not closed or dropped. Calling it a second time returns
    /// an empty vector.
    pub fn take_tools(&mut self) -> Vec<McpTool> {
        std::mem::take(&mut self.tools)
    }

    /// Ends the session and waits for the child process to be torn down.
    ///
    /// This is the tidy shutdown: it cancels the connection, closes the
    /// transport, and waits for the background task (which stops the child) to
    /// finish, so any teardown error is observable here rather than swallowed
    /// on drop. After this the tools this handle produced can no longer reach
    /// the server.
    pub async fn close(mut self) -> Result<(), McpError> {
        self.service.close().await.map_err(McpError::Shutdown)?;
        Ok(())
    }
}

/// What connecting to or driving an MCP server can fail on.
///
/// The variants track the three stages of bringing a server up plus its
/// teardown. They wrap the rmcp SDK's own error types, keeping rmcp naming out
/// of this crate's other error surfaces: only this module names MCP.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The server child process could not be spawned (bad program path,
    /// permissions, and so on).
    #[error("failed to spawn the MCP server process")]
    Spawn(#[source] std::io::Error),
    /// The child spawned but the MCP initialize handshake failed. Boxed
    /// because rmcp's initialize error is large, and boxing keeps the whole
    /// `McpError` small for the common success path that never allocates one.
    #[error("failed to initialize the MCP session")]
    Initialize(#[source] Box<rmcp::service::ClientInitializeError>),
    /// The session initialized but listing the server's tools failed.
    #[error("failed to list the MCP server's tools")]
    ListTools(#[source] rmcp::service::ServiceError),
    /// Closing the session or tearing the child process down failed.
    #[error("failed to shut the MCP session down cleanly")]
    Shutdown(#[source] tokio::task::JoinError),
}
