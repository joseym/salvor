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
use salvor_tools::{DynTool, HandlerError, Sleep, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::OffsetDateTime;
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

/// A clock a test sets by hand, for the durable-timer scenarios: a sleeping
/// run continues when its deadline arrives, and "arrives" means the test said
/// so. Nothing sleeps in real time.
#[derive(Clone)]
pub struct TestClock {
    now: Arc<std::sync::Mutex<OffsetDateTime>>,
}

impl TestClock {
    /// A clock reading `start` until something moves it.
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            now: Arc::new(std::sync::Mutex::new(start)),
        }
    }

    /// The injected clock function: envelope timestamps and live `now`
    /// observations both read it.
    pub fn injected(&self) -> ClockFn {
        let now = self.now.clone();
        Arc::new(move || *now.lock().expect("clock is not poisoned"))
    }

    pub fn read(&self) -> OffsetDateTime {
        *self.now.lock().expect("clock is not poisoned")
    }

    pub fn set(&self, instant: OffsetDateTime) {
        *self.now.lock().expect("clock is not poisoned") = instant;
    }
}

/// A tool that parks its run until a fixed instant instead of producing an
/// output, with the same shared execution counter [`EchoTool`] carries.
///
/// The instant is fixed rather than derived from a duration for the reason the
/// outcome carries an instant at all: it is recorded, and every drive must
/// present the same one.
pub struct NappingTool {
    pub name: String,
    pub effect: Effect,
    pub wake_at: OffsetDateTime,
    pub calls: Arc<AtomicUsize>,
}

impl NappingTool {
    pub fn new(name: &str, effect: Effect, wake_at: OffsetDateTime) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                wake_at,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for NappingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a test tool that parks its run on a timer"
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
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Sleep(Sleep::until(self.wake_at)))
    }
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
            Event::RunRedriven { .. } => "RunRedriven",
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

/// A scripted model that picks its response by a NEEDLE found in the raw
/// request body, for the conversations [`ScriptedModel`]'s message count
/// cannot tell apart.
///
/// A fold's passes are exactly that case: every pass drives a fresh agent loop
/// whose conversation is one message long, so the count is 1 every time and
/// only the pass's own input distinguishes them. Matching on request content
/// keeps the script correct across replays and resumes the same way the count
/// does: it reads the request, never a call counter, so a replayed call that
/// never reaches the server changes nothing. Needles are tried in script
/// order; the first one the body contains wins.
pub struct ContentScriptedModel {
    script: Vec<(String, Value)>,
}

impl ContentScriptedModel {
    /// Mounts the script on a fresh mock server and returns it.
    pub async fn mount(script: Vec<(&str, Value)>) -> MockServer {
        let server = MockServer::start().await;
        let script = script
            .into_iter()
            .map(|(needle, response)| (needle.to_owned(), response))
            .collect();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(Self { script })
            .mount(&server)
            .await;
        server
    }
}

impl Respond for ContentScriptedModel {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = String::from_utf8_lossy(&request.body);
        for (needle, response) in &self.script {
            if body.contains(needle.as_str()) {
                return ResponseTemplate::new(200).set_body_json(response.clone());
            }
        }
        ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "test_script", "message": "no scripted response matched the request"}
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

/// A tool that ignores its input and returns a fixed JSON value, counting each
/// execution. Used to inject a structured routed value (for example a score
/// object) that a branch condition can read, while still proving a replay does
/// not re-execute it.
pub struct ConstTool {
    pub name: String,
    pub effect: Effect,
    pub value: Value,
    pub calls: Arc<AtomicUsize>,
}

impl ConstTool {
    /// A named tool of the given effect that always returns `value`, plus the
    /// shared execution counter.
    pub fn new(name: &str, effect: Effect, value: Value) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                value,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for ConstTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a constant-value test tool"
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
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Output(self.value.clone()))
    }
}

/// A `fold` body tool whose output is a PURE function of its input, so a fold's
/// passes are a deterministic sequence a kill and its resume reproduce exactly.
///
/// It reads the zero-based pass count at `pass` (absent reads as 0, which is how
/// a graph input enters pass 0) and returns `{"pass": pass + 1, "score": ...}`,
/// taking the score from the scripted list at that position. A position past the
/// end of the list, or a scripted `null`, is returned without a usable score, so
/// a test can script a pass the `best_by` join cannot choose. Because the fold
/// threads each pass's output into the next, `pass` counts itself up and the
/// scripted sequence plays out without the tool holding any state of its own.
pub struct PassTool {
    pub name: String,
    pub effect: Effect,
    pub scores: Vec<Value>,
    pub calls: Arc<AtomicUsize>,
}

impl PassTool {
    /// A named pass tool of the given effect, scripted with one score per pass,
    /// plus the shared execution counter.
    pub fn new(name: &str, effect: Effect, scores: Vec<Value>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                scores,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for PassTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a scripted fold-pass test tool"
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
        let pass = input.get("pass").and_then(Value::as_u64).unwrap_or(0);
        let mut output = json!({"pass": pass + 1});
        if let Some(score) = self.scores.get(pass as usize)
            && !score.is_null()
        {
            output["score"] = score.clone();
        }
        Ok(ToolOutcome::Output(output))
    }
}

/// A [`PassTool`] that answers the way an MCP tool does: the pass value rides
/// inside a `{"content": [...], "structuredContent": {...}}` envelope.
///
/// A tool reached over MCP returns a `CallToolResult`, and the engine records
/// that whole result as the call's output. The value the fold is actually
/// folding is the `structuredContent` payload, so this exists to prove the
/// engine unwraps it: the pass value it computes is identical to `PassTool`'s,
/// only wrapped. `envelopes` says which passes wrap, one flag per pass (a pass
/// past the end of the list wraps), so one tool can script a run where pass 0
/// answers with an envelope and a later pass answers bare, the way a graph
/// mixing an MCP tool and a native one would.
///
/// It reads `pass` at the BARE path, never `structuredContent.pass`, which is
/// the point: if the engine did not unwrap, the second pass would read no
/// `pass` at all and the sequence would stall at 1.
pub struct EnvelopePassTool {
    pub name: String,
    pub effect: Effect,
    pub scores: Vec<Value>,
    pub envelopes: Vec<bool>,
    pub calls: Arc<AtomicUsize>,
}

impl EnvelopePassTool {
    /// A named envelope tool of the given effect, scripted with one score and
    /// one wrap flag per pass, plus the shared execution counter.
    pub fn new(
        name: &str,
        effect: Effect,
        scores: Vec<Value>,
        envelopes: Vec<bool>,
    ) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                effect,
                scores,
                envelopes,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for EnvelopePassTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a scripted fold-pass test tool answering in an MCP result envelope"
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
        let pass = input.get("pass").and_then(Value::as_u64).unwrap_or(0);
        let mut payload = json!({"pass": pass + 1});
        if let Some(score) = self.scores.get(pass as usize)
            && !score.is_null()
        {
            payload["score"] = score.clone();
        }
        let wrapped = self.envelopes.get(pass as usize).copied().unwrap_or(true);
        if !wrapped {
            return Ok(ToolOutcome::Output(payload));
        }
        Ok(ToolOutcome::Output(json!({
            "content": [{"type": "text", "text": payload.to_string()}],
            "structuredContent": payload,
        })))
    }
}

/// A tool that always suspends, asking for the described input. What it asks for
/// is a constant, so a replayed call re-derives the identical suspension. Used to
/// park a run inside a `fold` pass, where the resume input becomes that pass's
/// output.
pub struct SuspendingTool {
    pub name: String,
    pub reason: String,
    pub input_schema: Value,
    pub calls: Arc<AtomicUsize>,
}

impl SuspendingTool {
    /// A named suspending tool asking for `input_schema` under `reason`, plus
    /// the shared execution counter.
    pub fn new(name: &str, reason: &str, input_schema: Value) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name: name.to_owned(),
                reason: reason.to_owned(),
                input_schema,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait::async_trait]
impl DynTool for SuspendingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "a test tool that always suspends"
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Suspend(salvor_tools::Suspension::new(
            self.reason.clone(),
            self.input_schema.clone(),
        )))
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
