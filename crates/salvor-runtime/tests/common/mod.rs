//! Shared helpers for the `salvor-runtime` integration tests.
//!
//! Everything here serves hermeticity and determinism:
//!
//! - [`fixed_clock`] and [`fixed_random`] are the injected clock and random
//!   source. The clock is *constant*, so every envelope in every phase of a
//!   test carries the same timestamp and full logs compare equal across a
//!   control run and a killed-and-recovered run.
//! - [`ScriptedModel`] mounts on a wiremock server and picks its response
//!   by the number of `messages` in the request body. Selecting on request
//!   shape (rather than call order) keeps the script correct across kills
//!   and resumes, where earlier calls replay from the log and never reach
//!   the server.
//! - [`TestTool`] is a `DynTool` with a shared execution counter and a
//!   scripted behavior (echo, fail, suspend), which is how the tests count
//!   real executions across process "lifetimes".

// Each integration test binary compiles this module separately and uses a
// different subset of it, so unused-item lints would fire per binary.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use salvor_core::{Effect, Event, EventEnvelope, RunId};
use salvor_llm::Config;
use salvor_runtime::{Agent, AgentBuilder, ClockFn, RandomFn};
use salvor_tools::{DynTool, HandlerError, Suspension, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A stable run id derived from a small test-chosen byte.
pub fn fixed_run_id(tag: u8) -> RunId {
    let mut bytes = [0u8; 16];
    bytes[15] = tag;
    // Stamp version 4 / RFC variant bits so the id is a well-formed UUID.
    bytes[6] = 0x40;
    bytes[8] = 0x80;
    RunId::from_uuid(Uuid::from_bytes(bytes))
}

/// A constant clock: every envelope timestamp and every recorded `now`
/// observation is the same instant, making logs comparable byte for byte.
pub fn fixed_clock() -> ClockFn {
    Arc::new(|| datetime!(2026-07-09 12:00:00 UTC))
}

/// A constant random source.
pub fn fixed_random() -> RandomFn {
    Arc::new(|| 7)
}

/// The `kind` names of a log's events, in order, for shape assertions.
pub fn event_kinds(log: &[EventEnvelope]) -> Vec<&'static str> {
    log.iter()
        .map(|envelope| match &envelope.event {
            Event::RunStarted { .. } => "RunStarted",
            Event::ModelCallRequested { .. } => "ModelCallRequested",
            Event::ModelCallCompleted { .. } => "ModelCallCompleted",
            Event::ToolCallRequested { .. } => "ToolCallRequested",
            Event::ToolCallCompleted { .. } => "ToolCallCompleted",
            Event::NowObserved { .. } => "NowObserved",
            Event::RandomObserved { .. } => "RandomObserved",
            Event::Suspended { .. } => "Suspended",
            Event::Resumed { .. } => "Resumed",
            Event::SleepStarted { .. } => "SleepStarted",
            Event::SleepCompleted {} => "SleepCompleted",
            Event::BudgetExceeded { .. } => "BudgetExceeded",
            Event::RunCompleted { .. } => "RunCompleted",
            Event::RunFailed { .. } => "RunFailed",
            Event::RunAbandoned { .. } => "RunAbandoned",
            Event::GraphRunStarted { .. } => "GraphRunStarted",
            Event::NodeEntered { .. } => "NodeEntered",
            Event::NodeExited { .. } => "NodeExited",
            Event::NodeSkipped { .. } => "NodeSkipped",
            Event::BranchTaken { .. } => "BranchTaken",
            Event::MapFannedOut { .. } => "MapFannedOut",
            Event::MapIterationStarted { .. } => "MapIterationStarted",
            Event::MapIterationJoined { .. } => "MapIterationJoined",
            Event::FoldIterationStarted { .. } => "FoldIterationStarted",
            Event::FoldIterationJoined { .. } => "FoldIterationJoined",
            Event::FoldConverged { .. } => "FoldConverged",
        })
        .collect()
}

/// A canned text response body with fixed usage numbers.
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

/// A canned response body asking to call one tool.
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

/// One `tool_use` content block, for a response that carries several.
pub fn tool_use_block(tool_use_id: &str, tool: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": tool_use_id, "name": tool, "input": input})
}

/// A canned response body carrying the given content blocks verbatim, for the
/// turns [`tool_use_response`] cannot express: two tool calls at once, or a
/// tool call beside text.
pub fn blocks_response(
    id: &str,
    blocks: Vec<Value>,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id": format!("msg_{id}"),
        "model": "test-model",
        "role": "assistant",
        "content": blocks,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A scripted model: responds to `POST /v1/messages` by matching the number
/// of `messages` in the request body against the script. The conversation
/// grows by two messages per loop iteration, so the count uniquely names
/// the loop turn whatever mixture of replayed and live calls preceded it.
pub struct ScriptedModel {
    /// Pairs of (message count, response body).
    script: Vec<(usize, Value)>,
}

impl ScriptedModel {
    /// Mounts the script on a fresh mock server and returns it.
    pub async fn mount(script: Vec<(usize, Value)>) -> MockServer {
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
        for (expected, response) in &self.script {
            if *expected == count {
                return ResponseTemplate::new(200).set_body_json(response.clone());
            }
        }
        // An unscripted turn is a test bug; 500 makes it loud.
        ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "test_script", "message": format!("no scripted response for {count} messages")}
        }))
    }
}

/// What a [`TestTool`] does when executed.
pub enum ToolBehavior {
    /// Return `{"echo": <input>}`.
    Echo,
    /// Fail every execution with this handler-error message.
    Fail(String),
    /// Ask to suspend the run.
    Suspend(Suspension),
}

/// A scripted tool with a shared execution counter.
pub struct TestTool {
    pub name: String,
    pub effect: Effect,
    pub behavior: ToolBehavior,
    /// Incremented once per execution attempt, shared with the test body so
    /// executions can be counted across runtimes.
    pub calls: Arc<AtomicUsize>,
}

impl TestTool {
    pub fn new(name: &str, effect: Effect, behavior: ToolBehavior) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                behavior,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for TestTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a scripted test tool"
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
            ToolBehavior::Echo => Ok(ToolOutcome::Output(json!({"echo": input}))),
            ToolBehavior::Fail(message) => Err(ToolError::Handler {
                tool: self.name.clone(),
                source: HandlerError::message(message.clone()),
            }),
            ToolBehavior::Suspend(suspension) => Ok(ToolOutcome::Suspend(suspension.clone())),
        }
    }
}

/// An agent builder preconfigured for a mock server: test model id, terse
/// prompt, and a client pointed at `server_uri` with retries disabled (the
/// scripts are deterministic; a retried 500 would only slow failures down).
pub fn agent_builder(server_uri: &str) -> AgentBuilder {
    Agent::builder()
        .model(
            Config::new().with_base_url(server_uri).with_max_retries(0),
            "test-model",
        )
        .system_prompt("You are a test agent.")
}

/// Extracts the `tool_result` content strings from one recorded wiremock
/// request body, in order.
pub fn tool_result_contents(request_body: &[u8]) -> Vec<String> {
    let body: Value = serde_json::from_slice(request_body).expect("request body is JSON");
    let mut contents = Vec::new();
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && let Some(content) = block.get("content").and_then(Value::as_str)
                {
                    contents.push(content.to_owned());
                }
            }
        }
    }
    contents
}
