//! The server-performed tool step and the client-driven resolve over real HTTP:
//! the write-ahead intent/dispatch/completion flow, the effect table (retry,
//! dangling `Idempotent` re-execution under the recorded key, dangling `Write`
//! to reconciliation), the operator-declared effect, the unknown-tool refusal,
//! and the end-to-end model-plus-tool replay proof.
//!
//! The tools are in-process scripted `DynTool`s with shared execution counters,
//! injected through a `ToolRegistry`, so a test proves a tool ran exactly once
//! (or never) across a retry or a reconciliation. Nothing touches the network:
//! the model provider the replay proof needs is a `wiremock` server.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{
    CountBehavior, TestServer, agent_factory, app_state, counter, get_json, memory_store,
    model_executor, post_json, text_response,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, Outcome, ReplayCursor, RunId, SequenceNumber};
use salvor_runtime::hash_value;
use salvor_tools::{DynTool, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A fixed recorded timestamp for hand-seeded envelopes.
fn ts() -> time::OffsetDateTime {
    datetime!(2026-07-11 12:00:00 UTC)
}

/// An in-process tool with a shared execution counter and a record of every
/// idempotency key it was dispatched under. Returns `{ "echo": <input> }`.
struct ScriptedTool {
    name: String,
    effect: Effect,
    calls: Arc<AtomicUsize>,
    seen_keys: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait::async_trait]
impl DynTool for ScriptedTool {
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
        json!({ "type": "object" })
    }
    async fn call_json(
        &self,
        ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_keys
            .lock()
            .expect("seen keys lock")
            .push(ctx.idempotency_key().map(ToOwned::to_owned));
        Ok(ToolOutcome::Output(json!({ "echo": input })))
    }
}

/// The shared handles a test watches a scripted tool through.
struct ToolProbe {
    tool: Arc<dyn DynTool>,
    calls: Arc<AtomicUsize>,
    seen_keys: Arc<Mutex<Vec<Option<String>>>>,
}

/// Builds a scripted tool plus the probe that watches it.
fn scripted_tool(name: &str, effect: Effect) -> ToolProbe {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_keys = Arc::new(Mutex::new(Vec::new()));
    let tool = Arc::new(ScriptedTool {
        name: name.to_owned(),
        effect,
        calls: calls.clone(),
        seen_keys: seen_keys.clone(),
    });
    ToolProbe {
        tool,
        calls,
        seen_keys,
    }
}

/// A server whose tool registry holds `tools` and whose model executor answers
/// `POST /v1/messages` from `mock` (used only by the replay proof).
async fn tool_server(mock: &MockServer, tools: Vec<Arc<dyn DynTool>>) -> TestServer {
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let mut registry = salvor_server::ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    let state = app_state(memory_store(), factory)
        .with_model_executor(model_executor(&mock.uri()))
        .with_tool_registry(Arc::new(registry));
    TestServer::spawn(state).await
}

/// A bare mock provider with nothing mounted (the tool tests never call it).
async fn no_provider() -> MockServer {
    MockServer::start().await
}

/// A provider that returns one text response, for the replay proof.
async fn json_provider() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("the plan", 10, 5)))
        .mount(&mock)
        .await;
    mock
}

/// Opens a fresh client-driven run.
async fn open_run(client: &reqwest::Client, base: &str) -> (String, String) {
    let (status, body) =
        post_json(client, &format!("{base}/v1/client-runs"), json!({}), None).await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    (
        body["run"].as_str().expect("run id").to_owned(),
        body["drive_token"].as_str().expect("token").to_owned(),
    )
}

/// A generic guarded append carrying the drive token.
async fn append(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: &str,
    events: Vec<Value>,
) -> StatusCode {
    client
        .post(format!("{base}/v1/client-runs/{run}/events"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(json!({ "events": events }).to_string())
        .send()
        .await
        .expect("append sends")
        .status()
}

/// A tool step with a free-form JSON body.
async fn tool_step(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base}/v1/client-runs/{run}/tool-step"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(body.to_string())
        .send()
        .await
        .expect("tool-step sends");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A client-driven resolve with the recorded output.
async fn resolve(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: &str,
    output: Value,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base}/v1/client-runs/{run}/resolve"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(json!({ "output": output }).to_string())
        .send()
        .await
        .expect("resolve sends");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Reads a client-driven run's log back as decoded envelopes.
async fn read_log(client: &reqwest::Client, base: &str, run: &str) -> Vec<EventEnvelope> {
    let (status, body) = get_json(client, &format!("{base}/v1/client-runs/{run}/log"), None).await;
    assert_eq!(status, StatusCode::OK, "log read: {body}");
    serde_json::from_value(body["log"].clone()).expect("decode log")
}

/// How many `POST /v1/messages` requests the mock has seen.
async fn provider_hits(mock: &MockServer) -> usize {
    mock.received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .filter(|request| request.url.path() == "/v1/messages")
        .count()
}

/// A `RunStarted` envelope value for `run` at seq 0.
fn run_started_env(run: &str) -> Value {
    env_value(
        run,
        0,
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({ "topic": "otters" }),
            labels: None,
            driven_by: None,
            caller: None,
        },
    )
}

/// The wire JSON of an envelope for `run` at `seq`.
fn env_value(run: &str, seq: u64, event: Event) -> Value {
    let run_id = RunId::from_uuid(Uuid::parse_str(run).expect("run id"));
    let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq), ts(), event);
    serde_json::to_value(envelope).expect("serialize envelope")
}

/// Seeds an envelope straight through the store (bypassing the guarded append),
/// standing in for a crash that left a dangling intent.
async fn seed(server: &TestServer, run: &str, seq: u64, event: Event) {
    let run_id = RunId::from_uuid(Uuid::parse_str(run).expect("run id"));
    let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq), ts(), event);
    server
        .state
        .store()
        .append(&envelope)
        .await
        .expect("seed envelope");
}

/// Test 1: a fresh tool step appends the intent and completion, returns the
/// output, and dispatches the tool exactly once.
#[tokio::test]
async fn tool_step_records_intent_and_completion_dispatching_once() {
    let mock = no_provider().await;
    let probe = scripted_tool("echo", Effect::Read);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    assert_eq!(
        append(
            &client,
            &server.base,
            &run,
            &token,
            vec![run_started_env(&run)]
        )
        .await,
        StatusCode::OK
    );

    let (status, body) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "seq": 1, "tool": "echo", "input": { "n": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tool-step: {body}");
    assert_eq!(body["output"], json!({ "echo": { "n": 1 } }));

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, completion");
    assert!(matches!(log[1].event, Event::ToolCallRequested { .. }));
    assert!(matches!(log[2].event, Event::ToolCallCompleted { .. }));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "dispatched once");
}

/// Test 2: retrying the same step returns the recorded output, the tool is not
/// dispatched again, and the log does not grow.
#[tokio::test]
async fn retry_returns_recorded_output_without_re_dispatching() {
    let mock = no_provider().await;
    let probe = scripted_tool("echo", Effect::Read);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let body = json!({ "seq": 1, "tool": "echo", "input": { "n": 1 } });
    let (status, first) = tool_step(&client, &server.base, &run, &token, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

    let (status, second) = tool_step(&client, &server.base, &run, &token, body).await;
    assert_eq!(status, StatusCode::OK, "retry: {second}");
    assert_eq!(second, first, "the recorded output comes back verbatim");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "no re-dispatch");
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        3,
        "no growth"
    );
}

/// Test 3: a dangling `Idempotent` intent re-executes on a matching tool step,
/// and the tool is dispatched under the RECORDED idempotency key.
#[tokio::test]
async fn dangling_idempotent_reexecutes_under_recorded_key() {
    let mock = no_provider().await;
    let probe = scripted_tool("idem", Effect::Idempotent);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    // Seed a dangling idempotent intent carrying a specific recorded key.
    seed(
        &server,
        &run,
        1,
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "idem".into(),
            input: json!({ "n": 7 }),
            effect: Effect::Idempotent,
            idempotency_key: Some("recorded-key-123".into()),
            performed_by: None,
        },
    )
    .await;

    // The retry must present the matching key (a mismatch would diverge). The
    // tool then re-executes under the recorded key.
    let (status, body) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({
            "seq": 1, "tool": "idem", "input": { "n": 7 },
            "idempotency_key": "recorded-key-123"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-issue: {body}");
    assert_eq!(body["output"], json!({ "echo": { "n": 7 } }));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "dispatched once");
    assert_eq!(
        *probe.seen_keys.lock().unwrap(),
        vec![Some("recorded-key-123".to_owned())],
        "the tool saw the recorded key"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "the seeded intent gained its completion");
    let Event::ToolCallCompleted { seq: corr, .. } = &log[2].event else {
        panic!("seq 2 is the completion");
    };
    assert_eq!(
        corr.get(),
        1,
        "the completion correlates to the seeded intent"
    );
}

/// Test 4: a dangling `Write` intent is a `needs_reconciliation` 409 carrying
/// the intent, the tool is NOT dispatched, and a client-driven resolve records
/// the completion and unsticks the run.
#[tokio::test]
async fn dangling_write_reconciles_then_resolve_unsticks() {
    let mock = no_provider().await;
    let probe = scripted_tool("render", Effect::Write);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    // Seed a dangling write intent (a render that may or may not have landed).
    seed(
        &server,
        &run,
        1,
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "render".into(),
            input: json!({ "doc": "a.typ" }),
            effect: Effect::Write,
            idempotency_key: None,
            performed_by: None,
        },
    )
    .await;

    let (status, body) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "seq": 1, "tool": "render", "input": { "doc": "a.typ" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "reconciliation: {body}");
    assert_eq!(body["error"]["code"], "needs_reconciliation");
    assert_eq!(body["error"]["details"]["intent"]["tool"], "render");
    assert_eq!(body["error"]["details"]["intent"]["effect"], "write");
    assert_eq!(body["error"]["details"]["intent"]["seq"], 1);
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "a dangling write is never dispatched"
    );

    // Resolve with the observed output records the missing completion.
    let (status, body) = resolve(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "pdf": "a.pdf" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");
    assert_eq!(body["resolved"], true);

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "the intent gained its resolved completion");
    let Event::ToolCallCompleted {
        seq: corr, output, ..
    } = &log[2].event
    else {
        panic!("seq 2 is the completion");
    };
    assert_eq!(corr.get(), 1, "correlates to the dangling intent");
    assert_eq!(
        output,
        &json!({ "pdf": "a.pdf" }),
        "carries the resolved output"
    );

    // The run is unstuck: a subsequent generic append proceeds.
    assert_eq!(
        append(
            &client,
            &server.base,
            &run,
            &token,
            vec![env_value(&run, 3, Event::NowObserved { now: ts() })],
        )
        .await,
        StatusCode::OK,
        "the run drives again after resolve"
    );
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "resolve dispatched nothing"
    );
}

/// Test 5: a client-declared effect is ignored; the recorded effect is the
/// registry's operator declaration.
#[tokio::test]
async fn client_declared_effect_is_ignored() {
    let mock = no_provider().await;
    // The registry declares this tool a Write.
    let probe = scripted_tool("w", Effect::Write);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    // The client tries to downgrade to "read" on a fresh call.
    let (status, body) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "seq": 1, "tool": "w", "input": { "x": 1 }, "effect": "read" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fresh write executes: {body}");

    let log = read_log(&client, &server.base, &run).await;
    let Event::ToolCallRequested { effect, .. } = &log[1].event else {
        panic!("seq 1 is the intent");
    };
    assert_eq!(
        *effect,
        Effect::Write,
        "the recorded effect is the registry's"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
}

/// Test 6: an unknown tool is a clean error and the log is untouched.
#[tokio::test]
async fn unknown_tool_is_an_error_with_nothing_written() {
    let mock = no_provider().await;
    let probe = scripted_tool("render", Effect::Write);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let (status, body) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "seq": 1, "tool": "missing", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown tool: {body}");
    assert_eq!(body["error"]["code"], "unknown_tool");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 1, "only the RunStarted; nothing was written");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

/// Test 7: a full client loop with a model step AND a tool step, then a fresh
/// cursor rebuilt from the fetched log re-drives with both replayed and zero
/// live executions.
#[tokio::test]
async fn full_loop_with_tool_then_replay_makes_no_live_call() {
    let mock = json_provider().await;
    let probe = scripted_tool("render", Effect::Read);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    // open -> RunStarted -> model-step -> tool-step -> RunCompleted.
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;
    let request = json!({
        "model": "test-model",
        "max_tokens": 256,
        "messages": [{ "role": "user", "content": "draft a plan" }]
    });
    let (status, _) = client
        .post(format!("{}/v1/client-runs/{run}/model-step", server.base))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", &token)
        .body(json!({ "seq": 1, "request": request }).to_string())
        .send()
        .await
        .map(|r| (r.status(), ()))
        .expect("model-step sends");
    assert_eq!(status, StatusCode::OK);

    let tool_input = json!({ "doc": "plan.typ" });
    let (status, _) = tool_step(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "seq": 3, "tool": "render", "input": tool_input }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        append(
            &client,
            &server.base,
            &run,
            &token,
            vec![env_value(
                &run,
                5,
                Event::RunCompleted {
                    output: json!({ "done": true })
                }
            )],
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(provider_hits(&mock).await, 1, "one live model call");
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        1,
        "one live tool dispatch"
    );

    // Re-open, rebuild a cursor from the fetched log, re-drive: every step is
    // replayed and neither the provider nor the tool runs again.
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(
        log.len(),
        6,
        "started, model intent+completion, tool intent+completion, completed"
    );
    let mut cursor = ReplayCursor::new(log).expect("the log is a well-formed run");
    assert!(matches!(
        cursor.begin("sha256:agent", None, None).expect("begin"),
        Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .model_call(&hash_value(&request), None)
            .expect("model"),
        Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .tool_call("render", &json!({ "doc": "plan.typ" }), Effect::Read, None)
            .expect("tool"),
        Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .complete_run(&json!({ "done": true }))
            .expect("complete"),
        Outcome::Replayed(_)
    ));
    assert!(
        cursor.is_finished(),
        "the run replayed to its terminal event"
    );
    assert_eq!(provider_hits(&mock).await, 1, "model replay paid nothing");
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        1,
        "tool replay ran nothing"
    );
}

/// Test 8: a resolve on a run that is not ending in a dangling write is the same
/// `wrong_state` error the server-driven resolve gives.
#[tokio::test]
async fn resolve_without_dangling_write_is_wrong_state() {
    let mock = no_provider().await;
    let probe = scripted_tool("render", Effect::Write);
    let server = tool_server(&mock, vec![probe.tool.clone()]).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    // A running run with no dangling write.
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let (status, body) = resolve(
        &client,
        &server.base,
        &run,
        &token,
        json!({ "pdf": "a.pdf" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "wrong state: {body}");
    assert_eq!(body["error"]["code"], "wrong_state");
}
