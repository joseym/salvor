//! Integration coverage for the streamable-HTTP transport of the `mcp` module
//! ([`McpServer::connect_http`]).
//!
//! The whole file is gated on the `mcp` feature, so under
//! `--no-default-features` it compiles to nothing. Where the stdio test
//! (`mcp_integration.rs`) spawns a child process, this one serves a real MCP
//! server *in process*: it builds the same kind of `rmcp` server the child
//! fixture uses, wraps it in `rmcp`'s [`StreamableHttpService`], and mounts it
//! on an `axum` router bound to a loopback port the OS chooses (`127.0.0.1:0`).
//! That is hermetic: nothing but the test process listens, there is no network
//! and no external program, and the product path under test is exactly "reach a
//! server by URL over HTTP." In-process is cleaner than a child here because an
//! HTTP server needs a port, and binding it in the test avoids handing a chosen
//! port to a child and racing its startup.
//!
//! [`McpServer::connect_http`]: salvor_tools::mcp::McpServer::connect_http
//! [`StreamableHttpService`]: rmcp::transport::StreamableHttpService

#![cfg(feature = "mcp")]

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Meta, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use salvor_tools::mcp::{EffectOverrides, IdempotencyKeys, McpServer};
use salvor_tools::{DynTool, Effect, SuspensionKind, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinHandle;

/// The bearer token the auth-gated server accepts. The tests use it to prove
/// [`McpServer::connect_http`] sends the credential, and its absence to prove a
/// gated server refuses an unauthenticated client.
const TOKEN: &str = "secret-token";

/// The single argument `append_note` takes; echoed back so the round-trip test
/// can see its own input return through the server.
#[derive(Debug, Deserialize, JsonSchema)]
struct AppendArgs {
    /// The line to append.
    line: String,
}

/// The in-process fixture server. It is a genuine `rmcp` server, identical in
/// spirit to the stdio child fixture, exposing exactly the tools the effect
/// mapping and round-trip assertions need.
#[derive(Clone)]
struct HttpFixture {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl HttpFixture {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// Read-only: observes state, changes nothing. Must map to [`Effect::Read`].
    #[tool(
        description = "Read the note. Observes state only.",
        annotations(read_only_hint = true)
    )]
    async fn read_note(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "the note says hello",
        )]))
    }

    /// Idempotent, and echoes its argument. Must map to [`Effect::Idempotent`].
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

    /// Unannotated: the client must presume it writes. Must map to
    /// [`Effect::Write`].
    #[tool(description = "Do something with unstated effects.")]
    async fn mutate(&self) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text("mutated")]))
    }

    /// Parks the calling run until an external system reports back. The park
    /// request travels in `_meta`, which is transport-independent: the same
    /// bytes reach the client over HTTP as over a child's stdout.
    #[tool(
        description = "Park the calling run until an external system reports back.",
        annotations(read_only_hint = true)
    )]
    async fn await_signal(&self) -> Result<CallToolResult, ErrorData> {
        let mut meta = Meta::new();
        meta.insert(
            "salvor".to_owned(),
            json!({"suspend": {
                "reason": "waiting on the settlement webhook",
                "input_schema": {"type": "object", "properties": {"paid": {"type": "boolean"}}},
                "kind": "signal",
            }}),
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "waiting on the settlement webhook",
        )])
        .with_meta(Some(meta)))
    }

    /// Asks for a park with the key misspelled, which must fail the call
    /// rather than pass the result through as output.
    #[tool(description = "Return a malformed park request.")]
    async fn bad_park(&self) -> Result<CallToolResult, ErrorData> {
        let mut meta = Meta::new();
        meta.insert(
            "salvor".to_owned(),
            json!({"sleepUntil": "2026-08-14T09:00:00Z"}),
        );
        Ok(
            CallToolResult::success(vec![ContentBlock::text("asking for a park")])
                .with_meta(Some(meta)),
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HttpFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Salvor MCP HTTP integration-test fixture server.")
    }
}

/// A running fixture server: its `/mcp` URL and the task hosting it. Dropping
/// (or calling [`shutdown`](Self::shutdown)) aborts the task and frees the port.
struct TestServer {
    url: String,
    handle: JoinHandle<()>,
}

impl TestServer {
    fn shutdown(self) {
        self.handle.abort();
    }
}

/// Builds the MCP HTTP service and mounts it at `/mcp` on an `axum` router.
fn mcp_router() -> Router {
    let service = StreamableHttpService::<HttpFixture, LocalSessionManager>::new(
        || Ok(HttpFixture::new()),
        Default::default(),
        StreamableHttpServerConfig::default(),
    );
    Router::new().nest_service("/mcp", service)
}

/// Middleware that rejects any request missing `Authorization: Bearer <TOKEN>`
/// with `403 Forbidden`. A 403 (rather than 401) keeps the client's auth-retry
/// machinery out of it: the connection simply fails, which is what the
/// unauthenticated-rejection test asserts.
async fn require_bearer(
    headers: HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if presented == Some(&format!("Bearer {TOKEN}")) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "missing or wrong bearer token").into_response()
    }
}

/// Binds a loopback port, spawns the server, and returns its `/mcp` URL. The
/// port is bound before this returns, so a client may connect immediately with
/// no startup race. When `require_token`, the server is wrapped in the
/// bearer-checking middleware.
async fn spawn_server(require_token: bool) -> TestServer {
    let router = if require_token {
        mcp_router().layer(axum::middleware::from_fn(require_bearer))
    } else {
        mcp_router()
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the bound address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    TestServer {
        url: format!("http://{addr}/mcp"),
        handle,
    }
}

/// Connects to `url` over HTTP with no auth and no overrides.
async fn connect(url: &str) -> McpServer {
    McpServer::connect_http(url, None, &EffectOverrides::new(), &IdempotencyKeys::new())
        .await
        .expect("the HTTP fixture connects, initializes, and lists tools")
}

/// Finds a tool by name, panicking with a clear message if absent.
fn find<'a>(server: &'a McpServer, name: &str) -> &'a dyn DynTool {
    server
        .tools()
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("the fixture exposes a `{name}` tool"))
}

// --- Criterion 1: listing and annotation-to-effect mapping ---------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_over_http_lists_tools_and_maps_annotations_to_effects() {
    let server = spawn_server(false).await;
    let client = connect(&server.url).await;

    let names: Vec<&str> = client.tools().iter().map(|t| t.name()).collect();
    assert!(names.contains(&"read_note"));
    assert!(names.contains(&"append_note"));
    assert!(names.contains(&"mutate"));

    // The description and schema are the server's own, read off the wire.
    let append = find(&client, "append_note");
    assert!(append.description().contains("Append a line"));
    assert!(
        append
            .input_schema()
            .get("properties")
            .and_then(|p| p.get("line"))
            .is_some(),
        "the schema exposes the `line` argument"
    );

    // The annotation-to-effect mapping is identical to the stdio path.
    assert_eq!(find(&client, "read_note").effect(), Effect::Read);
    assert_eq!(find(&client, "append_note").effect(), Effect::Idempotent);
    assert_eq!(find(&client, "mutate").effect(), Effect::Write);

    client.close().await.expect("clean shutdown");
    server.shutdown();
}

// --- Criterion 2: a per-tool override beats the annotation ---------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_override_beats_the_annotation_over_http() {
    let server = spawn_server(false).await;
    // read_note is annotated read-only, but the operator declares it a Write.
    let overrides = EffectOverrides::new().with("read_note", Effect::Write);
    let client = McpServer::connect_http(&server.url, None, &overrides, &IdempotencyKeys::new())
        .await
        .expect("connect with overrides");

    assert_eq!(
        find(&client, "read_note").effect(),
        Effect::Write,
        "the connect-time override wins over the server's read-only hint"
    );

    client.close().await.expect("clean shutdown");
    server.shutdown();
}

// --- Criterion 3: a round-trip tool call ---------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_call_round_trips_over_http() {
    let server = spawn_server(false).await;
    let client = connect(&server.url).await;

    let outcome = find(&client, "append_note")
        .call_json(&ToolCtx::new(None), json!({ "line": "echo-me" }))
        .await
        .expect("the call succeeds");

    match outcome {
        ToolOutcome::Output(value) => assert!(
            value.to_string().contains("echo-me"),
            "the server echoed the input back, got: {value}"
        ),
        ToolOutcome::Suspend(_) | ToolOutcome::Sleep(_) => {
            panic!("an MCP tool never parks its run: the protocol has no way to ask")
        }
    }

    client.close().await.expect("clean shutdown");
    server.shutdown();
}

// --- Criterion 4: reconnect (drop the client, connect again, call) -------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnecting_over_http_works() {
    let server = spawn_server(false).await;

    // First connection: call, then drop it without closing.
    {
        let first = connect(&server.url).await;
        find(&first, "read_note")
            .call_json(&ToolCtx::new(None), json!({}))
            .await
            .expect("call on the first connection");
    }

    // A fresh connection to the same URL, constructed after the first was
    // dropped, works and can call. This is reconnect-on-resume in miniature:
    // reconstructing the handle with the same argument, nothing carried over.
    let second = connect(&server.url).await;
    let outcome = find(&second, "read_note")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("call on the reconnected client");
    assert!(matches!(outcome, ToolOutcome::Output(_)));

    second.close().await.expect("clean shutdown");
    server.shutdown();
}

// --- Bearer-token auth ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bearer_token_is_sent_and_accepted() {
    // The server refuses any request without the token; connecting with it
    // proves connect_http puts the credential on the wire.
    let server = spawn_server(true).await;
    let client = McpServer::connect_http(
        &server.url,
        Some(TOKEN),
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await
    .expect("connecting with the bearer token succeeds");

    let outcome = find(&client, "read_note")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("an authenticated call succeeds");
    assert!(matches!(outcome, ToolOutcome::Output(_)));

    client.close().await.expect("clean shutdown");
    server.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unauthenticated_client_is_rejected() {
    // The same gated server, but the client presents no token: the initialize
    // handshake gets a 403 and connecting fails.
    let server = spawn_server(true).await;
    // `McpServer` is not `Debug`, so match rather than `expect_err`. It fails at
    // the handshake; the exact rmcp error text is not pinned, only that
    // connecting did not succeed.
    let result = McpServer::connect_http(
        &server.url,
        None,
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await;
    assert!(
        result.is_err(),
        "a gated server rejects an unauthenticated client"
    );

    server.shutdown();
}

// --- Parking through `_meta.salvor`, over HTTP ---------------------------

/// The park contract is a property of the result, not of the transport: a
/// server reached by URL parks its caller the same way a spawned child does,
/// and gets the same refusal when it asks wrongly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_park_request_crosses_http_unchanged() {
    let server = spawn_server(false).await;
    let client = connect(&server.url).await;

    let outcome = find(&client, "await_signal")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect("the call succeeds");
    match outcome {
        ToolOutcome::Suspend(suspension) => {
            assert_eq!(suspension.kind, Some(SuspensionKind::Signal));
            assert_eq!(suspension.reason, "waiting on the settlement webhook");
        }
        other => panic!("expected a suspension, got {other:?}"),
    }

    let error = find(&client, "bad_park")
        .call_json(&ToolCtx::new(None), json!({}))
        .await
        .expect_err("a misspelled park key fails the call");
    let ToolError::MalformedResult { detail, .. } = error else {
        panic!("a malformed park is an unreadable result, on this transport as on the other");
    };
    assert!(
        detail.contains("`_meta.salvor"),
        "the refusal names the key, got: {detail}"
    );

    client.close().await.expect("clean shutdown");
    server.shutdown();
}
