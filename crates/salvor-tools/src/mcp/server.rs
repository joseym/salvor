//! [`McpServer`]: an owned connection to one MCP server, spawned as a child
//! process and spoken to over stdio.
//!
//! # Child-process lifecycle
//!
//! An MCP server started by [`McpServer::connect`] is a real child process of
//! this one, and every stdio server this process starts is hardened the same
//! way, in [`harden`], before rmcp ever spawns it. Three measures, each
//! covering a different way the parent can go away:
//!
//! - **A process group of its own** (Unix). The child is spawned as the leader
//!   of a fresh process group, so every kill on the controlled paths below is a
//!   `killpg` that reaches the server *and anything the server spawned*: the
//!   `node` behind an `npx` launcher, a language server's own helper, a
//!   build tool's worker pool. Killing the one pid rmcp tracks would leave
//!   those running.
//! - **Kill on drop.** The spawned handle carries Tokio's kill-on-drop flag, so
//!   a connection torn down without a chance to run its own cleanup (a runtime
//!   shut down out from under the task rmcp spawns to reap the child) still
//!   sends `SIGKILL` rather than leaking the process.
//! - **A parent-death signal** (Linux only). Between `fork` and `exec` the
//!   child asks the kernel to send it `SIGKILL` the moment its parent dies, by
//!   any means at all, including `SIGKILL` of the parent, which runs no code
//!   here. See [`arm_parent_death_signal`].
//!
//! ## What is covered, and what is not
//!
//! Controlled shutdown is covered everywhere: [`close`](McpServer::close) ends
//! the session, closes the child's stdin, waits briefly for it to exit on its
//! own, then kills the group; dropping the handle without closing does the same
//! kill asynchronously. Neither depends on the server noticing its stdin closed,
//! so a server that never reads stdin again is still reaped.
//!
//! Uncontrolled parent death is where the platforms differ, and the honest
//! statement is short:
//!
//! - On **Linux**, the parent-death signal covers it. A parent killed with
//!   `SIGKILL`, or ended by any signal it does not handle, takes the server
//!   process with it. What can still outlive the parent there is a
//!   *grandchild*: the parent-death signal is armed on the server, not on
//!   processes the server started, and once the server is gone nothing is left
//!   to signal its group.
//! - On **macOS** (and any other Unix without a parent-death signal) there is
//!   no equivalent, and no code of ours runs after `SIGKILL`, so nothing can be
//!   done from this side. What bounds the damage is the stdio design rather
//!   than anything active: the child's stdout is a pipe whose only reader was
//!   the parent, so a reparented server dies of `SIGPIPE` on its next write to
//!   stdout, and one that reads stdin sees EOF and exits. **What survives is a
//!   server that does neither**: blocked writing somewhere else (the reported
//!   case was a write to a FIFO with no reader), or looping without touching
//!   its stdio. That process keeps running, reparented to init, still holding
//!   whatever the run asked it to do. Recovering from that is an operator
//!   action: `kill -TERM -<pid>` against the server's process group, which the
//!   fresh group above makes a safe thing to type.
//!
//! One consequence of the fresh process group is worth stating rather than
//! discovering: an MCP server is no longer in this process's terminal
//! foreground group, so a Ctrl-C at the terminal reaches the parent and not the
//! server. On Linux the parent-death signal covers that case too. On macOS it
//! puts Ctrl-C in the same bucket as `SIGKILL`: a well-behaved server still
//! exits on stdin EOF, and a server that ignores its stdio survives until an
//! operator kills its group. `salvor-cli`'s `dev_server` module made the same
//! trade for `ng serve`, deliberately and for the same reason.

use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Value;
use tokio::process::Command;

#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{CommandWrap, KillOnDrop};

use super::{EffectOverrides, IdempotencyKeys, McpTool, effect_for};

/// A live connection to one MCP server.
///
/// Constructing an `McpServer` runs the MCP initialize handshake and lists the
/// server's tools, turning each into an [`McpTool`]. The handle then owns that
/// connection's lifecycle: the tools it produced hold cheap clones of the
/// client peer, so this handle must stay alive for as long as those tools are
/// dispatched. Closing or dropping it ends the session.
///
/// # Two transports, one handle
///
/// There are two constructors, one per transport, and the handle is identical
/// afterward because both resolve to the same `rmcp` running-service type:
///
/// - [`connect`](Self::connect) spawns the server as a child process and speaks
///   MCP over its stdio: the child's stdin and stdout
///   carry the JSON-RPC stream, its stderr is inherited so a misbehaving
///   server's diagnostics reach the operator.
/// - [`connect_http`](Self::connect_http) reaches a *remote* server by URL over
///   the streamable-HTTP transport: no child process, just HTTP
///   requests to an already-running server, with optional bearer-token auth.
///
/// Once connected, listing, dispatch, effect mapping, overrides, and shutdown
/// are the same for both; the transport is chosen only at construction.
///
/// # Reconnect on resume is safe
///
/// A stdio child process is not durable (it holds no run state) and a remote
/// HTTP endpoint is out of Salvor's control entirely, so neither is resumed. On
/// resume the runtime does not reattach to an old session; it constructs a
/// fresh `McpServer`, which for stdio spawns a new child and for HTTP opens a
/// new connection, then lists tools again. Reconnecting is therefore just
/// reconstruction: same constructor, same arguments, a new handle.
///
/// That is safe, and the reason is the replay contract, not anything this
/// handle does: a completed tool call is read from the event log and never
/// re-executed on resume, regardless of effect class. See
/// [`Effect`](salvor_core::Effect), whose documentation states that rule, and
/// the replay cursor in `salvor-core` that enforces it. So a reconnected server
/// is only ever asked to run calls that had *not* completed. A crash between a
/// recorded write intent and its completion does not silently re-fire against
/// the new session; it surfaces for human reconciliation. Reconnecting is
/// reconstruction, with no risk of a duplicated side effect.
///
/// # Shutdown
///
/// [`close`](Self::close) ends the session and waits for the child to be torn
/// down. Dropping the handle without calling `close` still ends the session:
/// the underlying connection cancels itself on drop and the child is stopped,
/// though asynchronously and without a chance to observe errors, so `close` is
/// the tidy path.
///
/// Either way the child is *killed*, not merely asked to leave: teardown closes
/// its stdin, waits a few seconds for it to exit on its own, then signals its
/// whole process group. A server that never reads its stdin again, or that
/// started subprocesses of its own, is reaped along with the rest. What that
/// does and does not cover when this process dies *without* running any of
/// this, and how the two supported platforms differ there, is the subject of
/// the [module docs](self).
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
    /// `keys` carries the other operator declaration a server cannot make for
    /// itself: which input field identifies a call, per tool. A tool named
    /// there declares an idempotency key and can be deduplicated across runs; a
    /// tool absent from it is keyless, exactly as every MCP tool was before.
    /// See [`IdempotencyKeys`].
    ///
    /// [`Effect`]: salvor_core::Effect
    pub async fn connect(
        command: Command,
        overrides: &EffectOverrides,
        keys: &IdempotencyKeys,
    ) -> Result<Self, McpError> {
        // `TokioChildProcess` spawns the child with stdin/stdout piped (the
        // JSON-RPC stream) and stderr inherited. It is handed the *hardened*
        // command, not the caller's, so every stdio server this process starts
        // gets the same lifecycle guarantees no matter which caller built it;
        // see the module docs for what those are and what they do not cover.
        let transport = TokioChildProcess::new(harden(command)).map_err(McpError::Spawn)?;

        // `()` is the no-op client handler: this client offers the server no
        // capabilities of its own, it only calls tools. `serve` runs the
        // initialize handshake and returns once the session is live.
        let service = ().serve(transport).await.map_err(|e| McpError::Initialize(Box::new(e)))?;

        Self::from_service(service, overrides, keys).await
    }

    /// Connects to a *remote* MCP server at `url` over the streamable-HTTP
    /// transport, initializes the session, lists its tools, and returns the
    /// connected handle.
    ///
    /// This is the remote counterpart to [`connect`](Self::connect): there is
    /// no child process, only HTTP requests to a server the operator is already
    /// running. Everything after connection is identical to the stdio path,
    /// including the effect mapping: each listed tool's [`Effect`] is decided by
    /// [`effect_for`](super::effect_for) from the server's annotation hints,
    /// unless `overrides` pins the tool's name.
    ///
    /// `bearer_token`, when `Some`, is sent as an `Authorization: Bearer <token>`
    /// header on every request (the value is the raw token, without the
    /// `Bearer ` prefix). Pass `None` for a server that needs no auth. This is
    /// the only auth knob v0.1 exposes; richer schemes (OAuth flows, arbitrary
    /// custom headers) are `rmcp` capabilities not wired up here.
    ///
    /// A `url` that names an unreachable or non-MCP endpoint is not diagnosed
    /// here: the transport is constructed eagerly but does not probe the
    /// network, so the failure surfaces from the initialize handshake as
    /// [`McpError::Initialize`].
    ///
    /// [`Effect`]: salvor_core::Effect
    pub async fn connect_http(
        url: &str,
        bearer_token: Option<&str>,
        overrides: &EffectOverrides,
        keys: &IdempotencyKeys,
    ) -> Result<Self, McpError> {
        let config = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
        let config = match bearer_token {
            Some(token) => config.auth_header(token.to_owned()),
            None => config,
        };
        // `from_config` builds the transport on rmcp's bundled reqwest client.
        // It does not touch the network yet; the first request is the
        // initialize handshake `serve` drives below.
        let transport = StreamableHttpClientTransport::from_config(config);

        // `()` is the same no-op client handler the stdio path uses, and
        // `serve` produces the same `RunningService<RoleClient, ()>`, which is
        // why the rest of this handle does not care which transport it rode in
        // on.
        let service = ().serve(transport).await.map_err(|e| McpError::Initialize(Box::new(e)))?;

        Self::from_service(service, overrides, keys).await
    }

    /// Lists a live session's tools and wraps each as an [`McpTool`], the step
    /// shared by every transport. Both constructors hand it an initialized
    /// [`RunningService`]; from here on the transport is invisible.
    async fn from_service(
        service: RunningService<RoleClient, ()>,
        overrides: &EffectOverrides,
        keys: &IdempotencyKeys,
    ) -> Result<Self, McpError> {
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
                // The server's output schema is optional on the wire (most
                // tools declare none); converted the same way as the input
                // schema when present.
                let output_schema = tool
                    .output_schema
                    .map(|schema| Value::Object((*schema).clone()));
                let effect = effect_for(&name, tool.annotations.as_ref(), overrides);
                // The operator's key declaration for this tool, if there is
                // one. Cloned per tool so each `McpTool` owns what it needs and
                // the caller's declaration set stays borrowed only here.
                let idempotency_key = keys.get(&name).cloned();
                McpTool::new(
                    peer.clone(),
                    name,
                    description,
                    input_schema,
                    output_schema,
                    effect,
                    idempotency_key,
                )
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

/// Wraps a caller's [`Command`] in the child-lifecycle measures every stdio MCP
/// server gets, and returns the wrapper rmcp's transport spawns.
///
/// Nothing about the caller's command is changed: program, arguments,
/// environment, and working directory are untouched. What is added is how the
/// resulting process is *held*, which is not a decision a caller should have to
/// remember to make. The three measures and what each is for are laid out in
/// the [module docs](self); this function is where they are applied, in one
/// place, so no spawn site can miss one.
///
/// The order matters in one respect only: the parent-death signal is armed on
/// the raw [`Command`] first, because it runs in the child between `fork` and
/// `exec` and must be in place before the process group wrapper's own spawn
/// hook is layered on top.
fn harden(#[allow(unused_mut)] mut command: Command) -> CommandWrap {
    #[cfg(target_os = "linux")]
    arm_parent_death_signal(&mut command);

    let mut wrapped = CommandWrap::from(command);
    // Tokio's kill-on-drop. rmcp reaps the child from a task it spawns on drop,
    // which needs a live runtime to run; this is the layer underneath that, so
    // a runtime torn down before that task is polled still kills the process
    // when the handle itself is dropped.
    wrapped.wrap(KillOnDrop);
    #[cfg(unix)]
    {
        // A fresh process group with the child as leader, so every kill below
        // is a `killpg` reaching the server and whatever the server spawned.
        // Read the module docs before removing this: it is also what takes the
        // server out of this process's terminal foreground group.
        wrapped.wrap(ProcessGroup::leader());
    }
    wrapped
}

/// Arms `PR_SET_PDEATHSIG` on the child so the kernel sends it `SIGKILL` the
/// moment this process dies, by any means, including a `SIGKILL` that runs no
/// code here at all. Linux only; there is no macOS equivalent.
///
/// The work happens in a `pre_exec` hook, which runs in the forked child after
/// `fork` and before `exec`. Two details make that placement correct rather
/// than merely convenient. First, the setting survives `exec`, so arming it
/// before the server binary is even loaded still covers the server binary.
/// Second, it closes the obvious race the other way round: the parent could
/// already have died between `fork` and this hook, in which case the
/// parent-death signal would never fire because the death already happened.
/// The hook therefore re-reads its parent's pid and exits immediately if it is
/// no longer the process that spawned it.
///
/// One honest caveat about the kernel's semantics: the parent-death signal
/// fires when the *thread* that spawned the child exits, not necessarily when
/// the whole parent process does. Every spawn here happens on a Tokio worker
/// thread of a runtime that lives as long as the process, so the two coincide
/// in practice; a caller that spawned an MCP server from a short-lived thread
/// of its own would see the child die with that thread.
#[cfg(target_os = "linux")]
fn arm_parent_death_signal(command: &mut Command) {
    // `pre_exec` here is Tokio's own inherent method on its `Command`, not the
    // std extension trait of the same name, so nothing needs importing.

    // Read on this side of the fork, so the child compares against the pid it
    // was actually spawned by rather than re-deriving it after the fact.
    let parent = std::process::id();

    // SAFETY: the closure runs in the forked child between `fork` and `exec`,
    // where only async-signal-safe work is allowed. It calls `prctl`,
    // `getppid`, and `_exit`, all of which are on that list, allocates nothing,
    // and touches no memory shared with the parent beyond the copied `parent`
    // integer.
    unsafe {
        command.pre_exec(move || {
            request_parent_death_signal()?;
            // Lost the race: the parent died before the line above ran, so the
            // signal it would have delivered is already in the past. Leave now
            // rather than exec a server nothing will ever reap.
            if libc::getppid() != parent as libc::pid_t {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// Asks the kernel for `SIGKILL` on the death of the calling thread's parent,
/// the `PR_SET_PDEATHSIG` half of [`arm_parent_death_signal`] on its own.
///
/// Split out because it is the half that can be observed: it changes a setting
/// the same process can read back, so a test can prove the request lands rather
/// than inferring it from a child's fate. The other half of the hook (the
/// `getppid` race check, which ends the process when it loses) cannot be
/// exercised in-process without ending the test, so it is proved from the
/// outside instead, by `tests/mcp_child_lifecycle.rs`.
#[cfg(target_os = "linux")]
fn request_parent_death_signal() -> std::io::Result<()> {
    // SAFETY: `prctl` with this option takes an integer, writes nothing through
    // a pointer, and affects only the calling thread's parent-death setting.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod parent_death_signal_tests {
    use super::*;

    /// The request the child makes for itself between `fork` and `exec`
    /// actually lands: after it, the kernel reports `SIGKILL` as the signal it
    /// will deliver when this thread's parent dies.
    ///
    /// Linux only, so it neither compiles nor runs on macOS, which is the
    /// honest shape of the feature: there is nothing to assert there. CI builds
    /// and runs the workspace on `ubuntu-latest`, so this is covered on every
    /// push, and the musl release build compiles it too.
    ///
    /// The setting is made on this thread and then cleared again. Arming it for
    /// real would be harmless (the test binary's parent is Cargo, which outlives
    /// it), but a test that leaves a process-wide-looking setting behind it is a
    /// bad neighbour to the tests that share the binary.
    #[test]
    fn requesting_the_parent_death_signal_sets_sigkill() {
        let mut before: libc::c_int = -1;
        // SAFETY: `PR_GET_PDEATHSIG` writes one `int` through the pointer it is
        // given, and it is given a live, correctly typed local.
        let read_back = |slot: &mut libc::c_int| unsafe {
            libc::prctl(libc::PR_GET_PDEATHSIG, slot as *mut libc::c_int)
        };
        assert_eq!(read_back(&mut before), 0, "reading the current setting");

        request_parent_death_signal().expect("the kernel accepts the request");

        let mut after: libc::c_int = -1;
        assert_eq!(read_back(&mut after), 0, "reading the setting back");
        assert_eq!(
            after,
            libc::SIGKILL,
            "the parent-death signal should be SIGKILL, the one a stuck server cannot ignore"
        );

        // SAFETY: same call, restoring whatever was set before this test ran
        // (zero, meaning no parent-death signal, in every ordinary case).
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, before);
        }
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
