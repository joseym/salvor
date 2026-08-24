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
//! # The parking tools
//!
//! Four more tools exist to drive the `_meta.salvor` park contract. They are
//! real servers' behavior in miniature: a tool that cannot finish yet says
//! when to come back, and a tool waiting on somebody else says what it waits
//! for.
//!
//! - `nap` returns ordinary content plus `_meta.salvor.sleep_until`, computed
//!   as its own clock plus the `seconds` argument. Negative seconds are
//!   allowed and are how a test stages an already-due deadline without
//!   waiting on wall time.
//! - `await_signal` returns content plus `_meta.salvor.suspend` carrying
//!   `kind: "signal"`, the webhook wait.
//! - `await_person` returns the same shape with no `kind`, the human gate.
//! - `bad_park` returns whatever malformed `_meta.salvor` value its `shape`
//!   argument names, so the refusals can be driven over a real wire rather
//!   than only in the decoder's own unit tests.
//!
//! # Environment knobs
//!
//! Three environment variables, all unset by default, let the child-lifecycle
//! tests observe and misbehave. They are read once at startup and never
//! influence the MCP surface above, so every other test sees the same server it
//! always saw.
//!
//! - `SALVOR_MCP_FIXTURE_PIDFILE`: write this process's own pid to that path
//!   before serving. This is how a test learns which process to look for after
//!   the connection is gone; `McpServer` deliberately does not expose the pid.
//! - `SALVOR_MCP_FIXTURE_GRANDCHILD`: spawn a long `sleep` and write *its* pid
//!   to that path. It stands in for the process a real server starts of its
//!   own (the `node` behind an `npx` launcher, a language server's helper),
//!   which is the thing a kill aimed at one pid leaves running.
//! - `SALVOR_MCP_FIXTURE_STUBBORN`: refuse to leave. The process ignores every
//!   catchable signal a polite shutdown would use, and, when the MCP session
//!   ends, sleeps forever instead of exiting. It writes nothing to stdout after
//!   that point either, so the two ways a well-behaved server dies on its own
//!   when orphaned (EOF on stdin, `SIGPIPE` on the next write to a closed
//!   stdout) are both off the table. This is the field-reported case: a server
//!   blocked somewhere that is not its stdio, which only a real kill can end.
//!
//! [`McpServer`]: salvor_tools::mcp::McpServer

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Meta, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The resume input every suspending tool here asks for. One boolean, which is
/// enough to prove the recorded schema is the one a resume validates against.
fn signal_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"paid": {"type": "boolean"}},
        "required": ["paid"],
    })
}

/// Wraps a park request in the `_meta` namespace the client reads it from.
/// Everything a server says to salvor lives under one key, so the rest of
/// `_meta` stays available to whoever else is listening.
fn salvor_meta(request: Value) -> Meta {
    let mut meta = Meta::new();
    meta.insert("salvor".to_owned(), request);
    meta
}

/// The single argument `append_note` takes. Deriving `JsonSchema` lets the
/// `#[tool]` macro publish an input schema for it; deriving `Deserialize` lets
/// the server parse the client's arguments into it.
#[derive(Debug, Deserialize, JsonSchema)]
struct AppendArgs {
    /// The line to append. Echoed back in the result so the round-trip test can
    /// see its own input come through the server.
    line: String,
}

/// The single argument `nap` takes: how far ahead of the server's own clock
/// the wake instant should be. Signed, because a test that wants a deadline
/// already in the past should not have to wait for one.
#[derive(Debug, Deserialize, JsonSchema)]
struct NapArgs {
    /// Seconds from now until the run may continue.
    seconds: i64,
}

/// The single argument `bad_park` takes: which malformed request to return.
#[derive(Debug, Deserialize, JsonSchema)]
struct BadParkArgs {
    /// One of `not_an_object`, `both`, `unknown_key`, `bad_timestamp`,
    /// `no_reason`, or `error_and_park`.
    shape: String,
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

    /// A tool that cannot finish yet and says when to come back. The wake
    /// instant is this server's own clock plus `seconds`, formatted RFC 3339,
    /// which is the arithmetic a real server does with a retry-after header or
    /// a settlement window.
    #[tool(
        description = "Park the calling run until the given number of seconds from now.",
        annotations(read_only_hint = true)
    )]
    async fn nap(
        &self,
        Parameters(NapArgs { seconds }): Parameters<NapArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let wake_at = OffsetDateTime::now_utc() + Duration::seconds(seconds);
        let wake_at = wake_at
            .format(&Rfc3339)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(
            CallToolResult::success(vec![ContentBlock::text(format!("napping until {wake_at}"))])
                .with_meta(Some(salvor_meta(json!({"sleep_until": wake_at})))),
        )
    }

    /// A tool waiting on an external system: the run parks and a webhook
    /// resumes it with a payload the schema below accepts.
    #[tool(
        description = "Park the calling run until an external system reports back.",
        annotations(read_only_hint = true)
    )]
    async fn await_signal(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "waiting on the settlement webhook",
        )])
        .with_meta(Some(salvor_meta(json!({"suspend": {
            "reason": "waiting on the settlement webhook",
            "input_schema": signal_schema(),
            "kind": "signal",
        }})))))
    }

    /// The same park with no `kind`: the human gate, which is what an unnamed
    /// kind has always meant.
    #[tool(
        description = "Park the calling run until a person answers.",
        annotations(read_only_hint = true)
    )]
    async fn await_person(&self) -> Result<CallToolResult, ErrorData> {
        Ok(
            CallToolResult::success(vec![ContentBlock::text("a person must confirm this")])
                .with_meta(Some(salvor_meta(json!({"suspend": {
                    "reason": "a person must confirm this",
                    "input_schema": signal_schema(),
                }})))),
        )
    }

    /// A tool that asks for a park it has spelled wrong. Each `shape` is one
    /// of the mistakes a server author actually makes.
    #[tool(description = "Return a malformed park request of the named shape.")]
    async fn bad_park(
        &self,
        Parameters(BadParkArgs { shape }): Parameters<BadParkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let namespace = match shape.as_str() {
            "not_an_object" => json!("suspend please"),
            "both" => json!({
                "suspend": {"reason": "either way", "input_schema": signal_schema()},
                "sleep_until": "2026-08-14T09:00:00Z",
            }),
            // The camelCase spelling of the key, which is the mistake the
            // strict decode exists to catch.
            "unknown_key" => json!({"sleepUntil": "2026-08-14T09:00:00Z"}),
            "bad_timestamp" => json!({"sleep_until": "in about an hour"}),
            "no_reason" => json!({"suspend": {"input_schema": signal_schema()}}),
            "error_and_park" => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "the settlement service refused",
                )])
                .with_meta(Some(salvor_meta(
                    json!({"sleep_until": "2026-08-14T09:00:00Z"}),
                ))));
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown bad_park shape `{other}`"),
                    None,
                ));
            }
        };
        Ok(
            CallToolResult::success(vec![ContentBlock::text("asking for a park")])
                .with_meta(Some(salvor_meta(namespace))),
        )
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

/// Writes `value` to the path named by `var`, if that variable is set.
///
/// Best effort on purpose: this is a test fixture, and a failure to record a
/// pid shows up as the waiting test's own timeout, which is a clearer report
/// than a panic inside a child whose stderr is interleaved with the run.
fn record(var: &str, value: u32) {
    if let Ok(path) = std::env::var(var) {
        let _ = std::fs::write(path, value.to_string());
    }
}

/// Makes this process ignore every catchable signal a shutdown would try
/// first, so only `SIGKILL` (which cannot be caught) ends it. Paired with
/// never exiting on session close, this is what "stubborn" means here.
///
/// `SIGPIPE` is in the list for the reason the module docs give: a reparented
/// server usually dies of it on the first write to a stdout nobody reads, and
/// the case worth testing is the one where that safety net is absent.
#[cfg(unix)]
fn ignore_polite_signals() {
    // SAFETY: `signal` with `SIG_IGN` sets a disposition and takes no pointers
    // to anything this process owns. Every constant here is a valid signal
    // number, and none of them is `SIGKILL` or `SIGSTOP`, the two the kernel
    // refuses to let a process ignore.
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGPIPE] {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stubborn = std::env::var_os("SALVOR_MCP_FIXTURE_STUBBORN").is_some();
    #[cfg(unix)]
    if stubborn {
        ignore_polite_signals();
    }

    record("SALVOR_MCP_FIXTURE_PIDFILE", std::process::id());

    // A subprocess of the server itself: the thing a kill aimed at the one pid
    // the client tracks would leave behind. It is a plain `sleep`, so it exits
    // on its own even if a test fails before it can be reaped.
    if std::env::var_os("SALVOR_MCP_FIXTURE_GRANDCHILD").is_some() {
        let child = std::process::Command::new("sleep")
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        record("SALVOR_MCP_FIXTURE_GRANDCHILD", child.id());
    }

    // Serve over stdio: the client owns this process's stdin/stdout as the
    // JSON-RPC stream. `waiting` blocks until the client closes the session
    // (which it does on shutdown), at which point the process exits.
    //
    // The result is held rather than propagated with `?` so the stubborn branch
    // below is reached even when the session ended badly. That is the case the
    // whole mode exists for: a client that vanished mid-write leaves a broken
    // pipe here, and a stubborn server is one that does not take that as its
    // cue to leave.
    let served = async {
        let service = Fixture::new().serve(stdio()).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    if stubborn {
        // The session is over and this process should be gone. It is not, and
        // it never writes to stdout again, so nothing short of a real kill will
        // end it. The loop is deliberately quiet and unbounded; the test that
        // starts this mode is the thing responsible for killing it.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    served
}
