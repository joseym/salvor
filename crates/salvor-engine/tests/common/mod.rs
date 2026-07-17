//! Shared helpers for the `salvor-engine` integration tests.
//!
//! Adapted from `salvor-runtime`'s test helpers (test modules cannot be shared
//! across crates): a constant clock and random source so full logs compare byte
//! for byte across a live drive and its replay, a `ScriptedModel` that picks its
//! response by request shape (so replayed calls never reach it), and a
//! `TestTool` with a shared execution counter so a replay's zero-execution
//! claim is checkable.

// Each integration test binary compiles this module separately and uses a
// different subset, so unused-item lints would fire per binary.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use salvor_core::{Effect, Event, EventEnvelope, RunId};
use salvor_llm::Config;
use salvor_runtime::{Agent, AgentBuilder, ClockFn, RandomFn};
use salvor_tools::{DynTool, HandlerError, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A stable run id derived from a small test-chosen byte.
pub fn fixed_run_id(tag: u8) -> RunId {
    let mut bytes = [0u8; 16];
    bytes[15] = tag;
    bytes[6] = 0x40;
    bytes[8] = 0x80;
    RunId::from_uuid(Uuid::from_bytes(bytes))
}

/// A constant clock: every envelope timestamp is the same instant, making logs
/// comparable byte for byte.
pub fn fixed_clock() -> ClockFn {
    Arc::new(|| datetime!(2026-07-14 12:00:00 UTC))
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
            Event::BudgetExceeded { .. } => "BudgetExceeded",
            Event::RunCompleted { .. } => "RunCompleted",
            Event::RunFailed { .. } => "RunFailed",
            Event::GraphRunStarted { .. } => "GraphRunStarted",
            Event::NodeEntered { .. } => "NodeEntered",
            Event::NodeExited { .. } => "NodeExited",
            Event::NodeSkipped { .. } => "NodeSkipped",
            Event::BranchTaken { .. } => "BranchTaken",
            Event::MapFannedOut { .. } => "MapFannedOut",
            Event::MapIterationStarted { .. } => "MapIterationStarted",
            Event::MapIterationJoined { .. } => "MapIterationJoined",
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

/// A scripted model: responds to `POST /v1/messages` by matching the number of
/// `messages` in the request body against the script. Replayed calls never
/// reach the server, so shape-based matching stays correct across a replay.
pub struct ScriptedModel {
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
        ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "test_script", "message": format!("no scripted response for {count} messages")}
        }))
    }
}

/// A tool that echoes its input, counting each execution so a replay's
/// zero-execution claim can be checked.
pub struct EchoTool {
    pub name: String,
    pub effect: Effect,
    pub calls: Arc<AtomicUsize>,
}

impl EchoTool {
    /// A named echo tool of the given effect, plus the shared counter.
    pub fn new(name: &str, effect: Effect) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "an echo test tool"
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
        Ok(ToolOutcome::Output(json!({"published": input})))
    }
}

/// A tool that always fails, for the tool-failure path.
pub struct FailingTool {
    pub name: String,
}

#[async_trait::async_trait]
impl DynTool for FailingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a failing test tool"
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        Err(ToolError::Handler {
            tool: self.name.clone(),
            source: HandlerError::message("publish endpoint unreachable"),
        })
    }
}

/// An agent builder preconfigured for a mock server: test model id, terse
/// prompt, a client pointed at `server_uri` with retries disabled.
pub fn agent_builder(server_uri: &str) -> AgentBuilder {
    Agent::builder()
        .model(
            Config::new().with_base_url(server_uri).with_max_retries(0),
            "test-model",
        )
        .system_prompt("You are a test agent.")
}
