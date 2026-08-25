//! Integration coverage for the `mcp` module, driven against the in-repo
//! fixture server (`src/bin/salvor-mcp-fixture.rs`).
//!
//! The whole file is gated on the `mcp` feature, so under
//! `--no-default-features` it compiles to nothing. Each test spawns the fixture
//! as a child process over stdio, exactly as the runtime spawns a real MCP
//! server, and Cargo hands us the built binary's path through
//! `CARGO_BIN_EXE_salvor-mcp-fixture`, so there is no path guessing and no
//! external dependency.

#![cfg(feature = "mcp")]

use async_trait::async_trait;
use salvor_tools::mcp::{EffectOverrides, IdempotencyKeys, McpServer};
use salvor_tools::{
    DynTool, Effect, HandlerError, SuspensionKind, Tool, ToolCtx, ToolError, ToolHandler,
    ToolOutcome, ToolSet,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::process::Command;

/// A fresh command that launches the fixture server. Each connection spawns its
/// own child, so tests do not share server state.
fn fixture_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_salvor-mcp-fixture"))
}

/// Connects to the fixture with no effect overrides.
async fn connect() -> McpServer {
    McpServer::connect(
        fixture_command(),
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await
    .expect("the fixture server connects, initializes, and lists tools")
}

/// Finds a tool by name in a slice, panicking with a clear message if absent.
fn find<'a>(server: &'a McpServer, name: &str) -> &'a dyn DynTool {
    server
        .tools()
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("the fixture exposes a `{name}` tool"))
}

// --- Criterion 1: listing and metadata ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_lists_tools_with_server_reported_metadata() {
    let server = connect().await;

    let names: Vec<&str> = server.tools().iter().map(|t| t.name()).collect();
    assert!(names.contains(&"read_note"));
    assert!(names.contains(&"append_note"));
    assert!(names.contains(&"mutate"));
    assert!(names.contains(&"explode"));

    // The description is the server's own.
    let append = find(&server, "append_note");
    assert!(
        append.description().contains("Append a line"),
        "description came from the server, got: {}",
        append.description()
    );

    // The JSON schema is the server's own: append_note takes a `line` string.
    let schema = append.input_schema();
    let properties = schema
        .get("properties")
        .expect("the schema names its properties");
    assert!(
        properties.get("line").is_some(),
        "schema exposes the `line` argument, got: {schema}"
    );

    server.close().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_with_an_advertised_output_schema_surfaces_it() {
    let server = connect().await;

    // `stamp_receipt` returns structured output, so the fixture's SDK
    // publishes an `outputSchema` for it; `DynTool::output_schema` must carry
    // that through unchanged.
    let stamp = find(&server, "stamp_receipt");
    let schema = stamp
        .output_schema()
        .expect("the server advertised an outputSchema for stamp_receipt");
    let properties = schema
        .get("properties")
        .expect("the schema names its properties");
    assert!(
        properties.get("settlement_id").is_some(),
        "schema exposes the `settlement_id` field, got: {schema}"
    );

    // A tool the server never declared an output schema for reports None,
    // not an empty schema.
    assert_eq!(
        find(&server, "append_note").output_schema(),
        None,
        "append_note declares no outputSchema"
    );

    server.close().await.expect("clean shutdown");
}

// --- Criterion 2: effect mapping and overrides ---------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn annotations_map_to_effects() {
    let server = connect().await;

    assert_eq!(
        find(&server, "read_note").effect(),
        Effect::Read,
        "readOnlyHint maps to Read"
    );
    assert_eq!(
        find(&server, "append_note").effect(),
        Effect::Idempotent,
        "idempotentHint maps to Idempotent"
    );
    assert_eq!(
        find(&server, "mutate").effect(),
        Effect::Write,
        "an unannotated tool defaults to Write"
    );

    server.close().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_override_beats_the_annotation() {
    // read_note is annotated read-only, but the operator declares it a Write.
    let overrides = EffectOverrides::new().with("read_note", Effect::Write);
    let server = McpServer::connect(fixture_command(), &overrides, &IdempotencyKeys::new())
        .await
        .expect("connect with overrides");

    assert_eq!(
        find(&server, "read_note").effect(),
        Effect::Write,
        "the connect-time override wins over the server's read-only hint"
    );

    server.close().await.expect("clean shutdown");
}

// --- Criterion 3: round-trip call and tool-error mapping -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_call_round_trips_the_result() {
    let server = connect().await;
    let append = find(&server, "append_note");

    let outcome = append
        .call_json(&ToolCtx::new(None), json!({ "line": "echo-me" }))
        .await
        .expect("the call succeeds");

    match outcome {
        ToolOutcome::Output(value) => {
            // The server echoed the input back into its result content.
            let rendered = value.to_string();
            assert!(
                rendered.contains("echo-me"),
                "the server's result carried the input back, got: {rendered}"
            );
        }
        ToolOutcome::Suspend(_) | ToolOutcome::Sleep(_) => {
            panic!("append_note asks for no park, so its result is plain output")
        }
    }

    // Byte-for-byte what this call recorded before `_meta` meant anything: the
    // whole `CallToolResult`, with no `_meta` key on it at all. A server that
    // says nothing to salvor is not touched on the way through.
    let outcome = append
        .call_json(&ToolCtx::new(None), json!({ "line": "echo-me" }))
        .await
        .expect("the call succeeds");
    assert_eq!(
        outcome,
        ToolOutcome::Output(json!({
            "content": [{"type": "text", "text": "appended: echo-me"}],
            "isError": false,
        })),
        "a result with no `_meta.salvor` records exactly as it always did"
    );

    server.close().await.expect("clean shutdown");
}

// --- Criterion 3b: parking through `_meta.salvor` ------------------------

/// A server that cannot finish yet says when to come back, and the call
/// resolves to the same [`ToolOutcome::Sleep`] a native tool returns. The
/// instant is the server's, read once, which is what makes it replayable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_can_ask_the_run_to_sleep() {
    let server = connect().await;
    let before = OffsetDateTime::now_utc();

    let outcome = find(&server, "nap")
        .call_json(&ToolCtx::new(None), json!({ "seconds": 3600 }))
        .await
        .expect("the call succeeds");

    match outcome {
        ToolOutcome::Sleep(sleep) => {
            // The fixture asked for an hour off its own clock, so the deadline
            // is an hour ahead of the moment before the call and no more than
            // an hour ahead of the moment after it.
            assert!(
                sleep.wake_at >= before + Duration::hours(1)
                    && sleep.wake_at <= OffsetDateTime::now_utc() + Duration::hours(1),
                "the deadline is the server's own clock plus an hour, got {}",
                sleep.wake_at
            );
        }
        other => panic!("expected a sleep request, got {other:?}"),
    }

    server.close().await.expect("clean shutdown");
}

/// A server waiting on a webhook says so, and the suspension carries the
/// discriminator that keeps the run out of an approval inbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_can_ask_the_run_to_wait_for_a_signal() {
    let server = connect().await;

    let outcome = find(&server, "await_signal")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("the call succeeds");

    match outcome {
        ToolOutcome::Suspend(suspension) => {
            assert_eq!(suspension.kind, Some(SuspensionKind::Signal));
            assert_eq!(suspension.reason, "waiting on the settlement webhook");
            assert_eq!(
                suspension.input_schema["properties"]["paid"]["type"],
                json!("boolean"),
                "the schema a resume is validated against is the server's own, got: {}",
                suspension.input_schema
            );
        }
        other => panic!("expected a suspension, got {other:?}"),
    }

    // The same shape with no `kind` is the human gate, exactly as an unnamed
    // kind means everywhere else.
    let gate = find(&server, "await_person")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("the call succeeds");
    match gate {
        ToolOutcome::Suspend(suspension) => assert_eq!(suspension.kind, None),
        other => panic!("expected a suspension, got {other:?}"),
    }

    server.close().await.expect("clean shutdown");
}

/// Every way a park request can be wrong fails the call with a message naming
/// `_meta.salvor`. None of them is quietly handed back as output, which is the
/// bug this contract exists to make impossible.
///
/// The variant matters as much as the failure. `MalformedResult` is the one
/// the runtime loop never retries, so a park the server spelled wrong costs
/// one execution on a `Read` exactly as it does on a `Write`. A `Handler`
/// here would buy two more calls to the same server and the same misspelling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_park_request_fails_the_call() {
    let server = connect().await;
    let bad = find(&server, "bad_park");

    for shape in [
        "not_an_object",
        "both",
        "unknown_key",
        "bad_timestamp",
        "no_reason",
        "error_and_park",
    ] {
        let error = bad
            .call_json(&ToolCtx::new(None), json!({ "shape": shape }))
            .await
            .expect_err("a malformed park request never resolves to an outcome");
        // Read before the destructure, because this is what the runtime
        // records: the error's own text, with the refusal inside it.
        let rendered = error.to_string();
        let ToolError::MalformedResult { tool, detail } = error else {
            panic!(
                "a malformed `{shape}` park is an unreadable result, never a retryable handler failure"
            );
        };
        assert_eq!(tool, "bad_park");
        assert!(
            detail.contains("`_meta.salvor"),
            "the `{shape}` refusal names the key, got: {detail}"
        );
        assert!(
            rendered.contains(&detail),
            "the refusal reaches the log verbatim, got: {rendered}"
        );
    }

    server.close().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_tool_error_surfaces_as_handler() {
    let server = connect().await;
    let explode = find(&server, "explode");

    let error = explode
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect_err("the explode tool reports an error");

    match error {
        ToolError::Handler { tool, source } => {
            assert_eq!(tool, "explode");
            assert!(
                source.to_string().contains("boom"),
                "the tool's own message came through, got: {source}"
            );
        }
        other => panic!("expected ToolError::Handler, got {other:?}"),
    }

    server.close().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_arguments_are_rejected_before_the_wire() {
    let server = connect().await;
    let append = find(&server, "append_note");

    // A bare array is not an MCP argument object, so it is rejected locally.
    let error = append
        .call_json(&ToolCtx::new(None), json!([1, 2, 3]))
        .await
        .expect_err("non-object arguments are invalid input");

    assert!(
        matches!(error, ToolError::InvalidInput { .. }),
        "a non-object argument is InvalidInput, got {error:?}"
    );

    server.close().await.expect("clean shutdown");
}

// --- Criterion 4: MCP and native tools in one registry -------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct GreetArgs {
    who: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct Greeting {
    text: String,
}

#[derive(Tool)]
#[tool(effect = "read", description = "Greet someone")]
struct Greet;

#[async_trait]
impl ToolHandler for Greet {
    type Input = GreetArgs;
    type Output = Greeting;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: GreetArgs,
    ) -> Result<ToolOutcome<Greeting>, HandlerError> {
        Ok(ToolOutcome::Output(Greeting {
            text: format!("hello, {}", input.who),
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_and_native_tools_dispatch_through_one_registry() {
    let mut server = connect().await;

    let mut tools = ToolSet::new();
    // A native derived tool.
    tools.register(Greet).expect("register native tool");
    // The MCP tools, moved out of the handle and registered erased. The handle
    // stays alive below so the tools' client-peer clones keep working.
    for tool in server.take_tools() {
        tools
            .register_dyn(Box::new(tool))
            .expect("register MCP tool");
    }

    // Both kinds are looked up and dispatched through the same registry.
    let native = tools.get("greet").expect("native tool is registered");
    let native_out = native
        .call_json(&ToolCtx::new(None), json!({ "who": "world" }))
        .await
        .expect("native dispatch");
    assert_eq!(
        native_out,
        ToolOutcome::Output(json!({ "text": "hello, world" }))
    );

    let mcp = tools.get("read_note").expect("MCP tool is registered");
    let mcp_out = mcp
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("MCP dispatch");
    assert!(matches!(mcp_out, ToolOutcome::Output(_)));

    server.close().await.expect("clean shutdown");
}

// --- Criterion 5: respawn ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_after_close_and_after_drop() {
    // First connection: call, then close explicitly.
    let first = connect().await;
    find(&first, "read_note")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("call on the first connection");
    first.close().await.expect("explicit close");

    // Second connection: call, then drop it without closing (the connection
    // cancels itself on drop and the child is stopped).
    {
        let second = connect().await;
        find(&second, "read_note")
            .call_json(&ToolCtx::new(None), json!({}))
            .await
            .expect("call on the second connection");
    }

    // Third connection, constructed fresh after both teardown paths, works.
    // This is respawn-on-resume in miniature: a new handle, a new child, a
    // successful call, with nothing carried over from the closed sessions.
    let third = connect().await;
    let outcome = find(&third, "read_note")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("call on the respawned connection");
    assert!(matches!(outcome, ToolOutcome::Output(_)));
    third.close().await.expect("clean shutdown");
}
