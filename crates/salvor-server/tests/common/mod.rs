//! Shared harness for the `salvor-server` integration tests.
//!
//! The tests drive the real router over loopback HTTP: a [`TestServer`] binds
//! `127.0.0.1:0`, and [`reqwest`] talks to it. Hermeticity comes from two
//! pieces with nothing on the network:
//!
//! - [`ScriptedModel`] is a wiremock server that answers `POST /v1/messages`
//!   by the number of `messages` in the request, so replay across a resume
//!   picks the right turn.
//! - [`CountingTool`] is an in-process `DynTool` with a shared execution
//!   counter, which is how a test proves a write ran exactly once across a
//!   kill and a recovery.
//!
//! The [`agent_factory`] wires those into the [`AgentFactory`] the server
//! builds agents through, so the definition a test submits is opaque (the
//! factory always builds the same agent) yet the agent's hash is stable.

#![allow(dead_code)]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use salvor_core::Effect;
use salvor_llm::{Client, Config};
use salvor_runtime::{Agent, ClockFn, RandomFn};
use salvor_server::{AgentFactory, AppState, BuiltAgent, LlmModelExecutor, ModelExecutor};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::{DynTool, HandlerError, Suspension, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::macros::datetime;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A constant clock, so recorded logs compare equal across a control run and a
/// recovered one.
pub fn fixed_clock() -> ClockFn {
    Arc::new(|| datetime!(2026-07-10 12:00:00 UTC))
}

/// A constant random source.
pub fn fixed_random() -> RandomFn {
    Arc::new(|| 11)
}

/// Server state over `store`, driving agents through `factory`, with the fixed
/// hooks and a short stream poll so tests finish quickly.
pub fn app_state(store: Arc<dyn EventStore>, factory: AgentFactory) -> AppState {
    AppState::new(store, factory)
        .with_hooks(fixed_clock(), fixed_random())
        .with_poll_interval(Duration::from_millis(10))
}

/// A bound, serving control plane, plus the state and task handle the test
/// keeps for aborting runs or standing a fresh server on the same store.
pub struct TestServer {
    /// The base URL, `http://127.0.0.1:<port>`.
    pub base: String,
    /// The state the router shares, for direct store reads and run aborts.
    pub state: AppState,
    /// The serving task.
    pub handle: JoinHandle<()>,
}

impl TestServer {
    /// Binds a loopback port and serves `state` on it.
    pub async fn spawn(state: AppState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("read the bound address");
        let serve_state = state.clone();
        let handle = tokio::spawn(async move {
            let _ = salvor_server::serve(listener, serve_state).await;
        });
        Self {
            base: format!("http://{addr}"),
            state,
            handle,
        }
    }
}

/// What a [`CountingTool`] does when executed.
#[derive(Clone)]
pub enum CountBehavior {
    /// Increment the counter and return `{"recorded": <input>}`.
    Record,
    /// Fail every execution with this message.
    Fail(String),
    /// Suspend the run with this schema.
    Suspend(Value),
    /// Increment the counter, then never return. The intent is recorded before
    /// the tool runs, so aborting the task while this hangs leaves a dangling
    /// write intent, which is exactly what needs-reconciliation is.
    Hang,
}

/// An in-process tool with a shared execution counter.
pub struct CountingTool {
    name: String,
    effect: Effect,
    behavior: CountBehavior,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DynTool for CountingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a counting test tool"
    }
    fn effect(&self) -> Effect {
        self.effect
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            CountBehavior::Record => Ok(ToolOutcome::Output(json!({"recorded": input}))),
            CountBehavior::Fail(message) => Err(ToolError::Handler {
                tool: self.name.clone(),
                source: HandlerError::message(message.clone()),
            }),
            CountBehavior::Suspend(schema) => Ok(ToolOutcome::Suspend(Suspension::new(
                "awaiting approval",
                schema.clone(),
            ))),
            CountBehavior::Hang => std::future::pending().await,
        }
    }
}

/// An agent factory that builds one agent: a mock model plus one counting
/// tool. The submitted definition is ignored, so the agent (and its hash) is
/// the same on every build, which is what a resume on a fresh server needs.
pub fn agent_factory(
    model_uri: String,
    tool_name: &str,
    effect: Effect,
    behavior: CountBehavior,
    calls: Arc<AtomicUsize>,
) -> AgentFactory {
    let tool_name = tool_name.to_owned();
    Arc::new(move |_definition| {
        let model_uri = model_uri.clone();
        let tool_name = tool_name.clone();
        let behavior = behavior.clone();
        let calls = calls.clone();
        Box::pin(async move {
            let tool = CountingTool {
                name: tool_name,
                effect,
                behavior,
                calls,
            };
            let agent = Agent::builder()
                .model(
                    Config::new().with_base_url(&model_uri).with_max_retries(0),
                    "test-model",
                )
                .system_prompt("You are a test agent.")
                .tool_dyn(Box::new(tool))
                .build()
                .map_err(|error| error.to_string())?;
            Ok(BuiltAgent {
                agent,
                servers: vec![],
            })
        })
    })
}

/// A fresh execution counter.
pub fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// The default model executor pointed at a mock provider `uri`, for the
/// server-performed model step. Retries are off (the scripts are
/// deterministic). Wraps the general `salvor_llm::Client`, exactly as
/// `salvor serve` wires its own out of the box.
pub fn model_executor(uri: &str) -> Arc<dyn ModelExecutor> {
    let client = Client::new(Config::new().with_base_url(uri).with_max_retries(0))
        .expect("model client builds");
    Arc::new(LlmModelExecutor::new(client))
}

/// A canned text response with fixed usage.
pub fn text_response(text: &str, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "id": format!("msg_text_{input_tokens}_{output_tokens}"),
        "model": "test-model",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A canned response asking to call one tool.
pub fn tool_use_response(
    tool_use_id: &str,
    tool: &str,
    input: Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id": format!("msg_tool_{tool_use_id}"),
        "model": "test-model",
        "role": "assistant",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": tool, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A scripted model, matching on the number of `messages` in the request. Each
/// entry is `(message_count, response, optional_delay)`.
pub struct ScriptedModel {
    script: Vec<(usize, Value, Option<Duration>)>,
}

impl ScriptedModel {
    /// Mounts the script on a fresh mock server and returns it.
    pub async fn mount(script: Vec<(usize, Value, Option<Duration>)>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(Self { script })
            .mount(&server)
            .await;
        server
    }
}

impl Respond for ScriptedModel {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => return ResponseTemplate::new(400),
        };
        let count = body
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (expected, response, delay) in &self.script {
            if *expected == count {
                let mut template = ResponseTemplate::new(200).set_body_json(response.clone());
                if let Some(delay) = delay {
                    template = template.set_delay(*delay);
                }
                return template;
            }
        }
        ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "test_script", "message": format!("no response for {count} messages")}
        }))
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers over the bound server.
// ---------------------------------------------------------------------------

/// A GET returning the status and decoded JSON body.
pub async fn get_json(
    client: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    let mut request = client.get(url);
    if let Some(token) = auth {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("GET sends");
    decode(response).await
}

/// A POST with a body and content type, returning the status and JSON body.
pub async fn post(
    client: &reqwest::Client,
    url: &str,
    content_type: &str,
    body: String,
    auth: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body);
    if let Some(token) = auth {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("POST sends");
    decode(response).await
}

/// A JSON POST.
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
    auth: Option<&str>,
) -> (reqwest::StatusCode, Value) {
    post(client, url, "application/json", body.to_string(), auth).await
}

async fn decode(response: reqwest::Response) -> (reqwest::StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, body)
}

/// Registers a TOML definition, returning its assigned agent hash.
pub async fn register_agent(
    client: &reqwest::Client,
    base: &str,
    toml: &str,
    auth: Option<&str>,
) -> String {
    let (status, body) = post(
        client,
        &format!("{base}/v1/agents"),
        "application/toml",
        toml.to_owned(),
        auth,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CREATED, "register: {body}");
    body["agent"].as_str().expect("agent hash").to_owned()
}

/// A trivial agent TOML. The factory ignores it, but registration parses it,
/// so it is real.
pub fn sample_toml() -> &'static str {
    "model = \"test-model\"\nsystem_prompt = \"You are a test agent.\"\n"
}

// ---------------------------------------------------------------------------
// A small server-sent-events reader.
// ---------------------------------------------------------------------------

/// One parsed server-sent-event frame.
#[derive(Debug)]
pub struct Frame {
    /// The `id:` field, parsed as a sequence number.
    pub id: Option<u64>,
    /// The `event:` field, when named.
    pub event: Option<String>,
    /// The `data:` payload.
    pub data: String,
}

impl Frame {
    /// Whether this is the terminal `end` frame.
    pub fn is_end(&self) -> bool {
        self.event.as_deref() == Some("end")
    }

    /// The frame's data decoded as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or(Value::Null)
    }
}

/// Reads server-sent-event frames from a running stream.
pub struct SseReader {
    stream: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    buf: String,
}

impl SseReader {
    /// Opens the event stream. `from_seq` sets the `?from_seq=` cursor;
    /// `last_event_id` sets the `Last-Event-ID` header.
    pub async fn open(
        client: &reqwest::Client,
        base: &str,
        run_id: &str,
        from_seq: Option<u64>,
        last_event_id: Option<u64>,
        auth: Option<&str>,
    ) -> Self {
        let mut url = format!("{base}/v1/runs/{run_id}/events");
        if let Some(seq) = from_seq {
            url.push_str(&format!("?from_seq={seq}"));
        }
        let mut request = client.get(&url);
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id.to_string());
        }
        if let Some(token) = auth {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.expect("stream connects");
        assert!(response.status().is_success(), "stream status");
        Self {
            stream: Box::pin(response.bytes_stream()),
            buf: String::new(),
        }
    }

    /// The next frame, or `None` when the stream ends.
    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(index) = self.buf.find("\n\n") {
                let raw = self.buf[..index].to_string();
                self.buf.drain(..index + 2);
                if let Some(frame) = parse_frame(&raw) {
                    return Some(frame);
                }
                continue;
            }
            match self.stream.next().await {
                Some(Ok(bytes)) => self.buf.push_str(&String::from_utf8_lossy(&bytes)),
                _ => return None,
            }
        }
    }

    /// Reads frames until (and including) the terminal `end` frame.
    pub async fn read_to_end(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            let end = frame.is_end();
            frames.push(frame);
            if end {
                break;
            }
        }
        frames
    }

    /// Reads exactly `count` non-terminal frames (for a mid-stream disconnect).
    pub async fn read_frames(&mut self, count: usize) -> Vec<Frame> {
        let mut frames = Vec::new();
        while frames.len() < count {
            match self.next().await {
                Some(frame) => frames.push(frame),
                None => break,
            }
        }
        frames
    }
}

/// Parses one raw frame block into a [`Frame`], or `None` if it is only a
/// keep-alive comment.
fn parse_frame(raw: &str) -> Option<Frame> {
    let mut id = None;
    let mut event = None;
    let mut data = String::new();
    let mut has_field = false;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("id:") {
            id = value.trim().parse().ok();
            has_field = true;
        } else if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
            has_field = true;
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            has_field = true;
        }
        // A line starting with ':' is a comment (keep-alive); ignore it.
    }
    has_field.then_some(Frame { id, event, data })
}

/// Reads a run's whole log directly from a store (a test convenience).
pub async fn read_log(
    store: &Arc<dyn EventStore>,
    run_id: salvor_core::RunId,
) -> Vec<salvor_core::EventEnvelope> {
    store.read_log(run_id).await.expect("read log")
}

/// Opens a file-backed store at `path` as the trait object the state holds.
pub fn open_store(path: &std::path::Path) -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::open(path).expect("open store"))
}

/// A private in-memory store.
pub fn memory_store() -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::in_memory().expect("open in-memory store"))
}
