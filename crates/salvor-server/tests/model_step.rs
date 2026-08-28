//! The server-performed model step over real HTTP: the write-ahead
//! intent/execute/completion flow, the `(seq, request_hash)` retry identity, the
//! streaming variant, and the end-to-end client-loop replay proof.
//!
//! The provider is a scripted `wiremock` server, so nothing touches the network.
//! The executor is the default `LlmModelExecutor` wrapping a `salvor_llm::Client`
//! pointed at that mock, exactly as `salvor serve` wires its own out of the box.

mod common;

use common::{
    CountBehavior, TestServer, agent_factory, app_state, counter, get_json, memory_store,
    model_executor, post_json, text_response,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, Outcome, ReplayCursor, RunId, SequenceNumber};
use salvor_runtime::hash_value;
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A fixed recorded timestamp for hand-seeded envelopes.
fn ts() -> time::OffsetDateTime {
    datetime!(2026-07-11 12:00:00 UTC)
}

/// The canonical model request the client reserves a model step for.
fn request() -> Value {
    json!({
        "model": "test-model",
        "max_tokens": 256,
        "messages": [{ "role": "user", "content": "draft a plan" }]
    })
}

/// A `RunStarted` event value for `run_id` at seq 0.
fn run_started_env(run_id: &str) -> Value {
    env_value(
        run_id,
        0,
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({ "topic": "otters" }),
            labels: None,
            driven_by: None,
        },
    )
}

/// The wire JSON of an envelope for `run_id` at `seq`.
fn env_value(run_id: &str, seq: u64, event: Event) -> Value {
    let run_id = RunId::from_uuid(Uuid::parse_str(run_id).expect("run id"));
    let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq), ts(), event);
    serde_json::to_value(envelope).expect("serialize envelope")
}

/// A server whose model executor answers `POST /v1/messages` from `mock`, with a
/// factory that is never invoked (client-driven runs build no agent).
async fn model_server(mock: &MockServer) -> TestServer {
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let state = app_state(memory_store(), factory).with_model_executor(model_executor(&mock.uri()));
    TestServer::spawn(state).await
}

/// Opens a fresh client-driven run with an optional `record_prompts` flag.
async fn open_run(client: &reqwest::Client, base: &str, record_prompts: bool) -> (String, String) {
    let (status, body) = post_json(
        client,
        &format!("{base}/v1/client-runs"),
        json!({ "record_prompts": record_prompts }),
        None,
    )
    .await;
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
    let response = client
        .post(format!("{base}/v1/client-runs/{run}/events"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(json!({ "events": events }).to_string())
        .send()
        .await
        .expect("append sends");
    response.status()
}

/// A non-streaming model step at `seq`.
async fn model_step(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: &str,
    seq: u64,
    request: &Value,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base}/v1/client-runs/{run}/model-step"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(json!({ "seq": seq, "request": request }).to_string())
        .send()
        .await
        .expect("model-step sends");
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

/// A canned JSON provider that returns one text response.
async fn json_provider() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("the plan", 10, 5)))
        .mount(&mock)
        .await;
    mock
}

/// Test 1: a fresh model step appends the intent and the completion, returns the
/// response, and hits the provider exactly once.
#[tokio::test]
async fn model_step_records_intent_and_completion_hitting_provider_once() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;

    // The client appends its own RunStarted first, then reserves seq 1 for the
    // model intent.
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

    let (status, body) = model_step(&client, &server.base, &run, &token, 1, &request()).await;
    assert_eq!(status, StatusCode::OK, "model-step: {body}");
    assert_eq!(body["response"]["content"][0]["text"], "the plan");
    assert_eq!(body["usage"]["input_tokens"], 10);
    assert_eq!(body["usage"]["output_tokens"], 5);

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, completion");
    assert!(matches!(log[1].event, Event::ModelCallRequested { .. }));
    assert!(matches!(log[2].event, Event::ModelCallCompleted { .. }));
    assert_eq!(provider_hits(&mock).await, 1, "the provider was hit once");
}

/// Test 2: retrying the same step (same seq, same body) returns the recorded
/// completion, the provider is not hit again, and the log does not grow.
#[tokio::test]
async fn retry_returns_recorded_completion_without_re_paying() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let (status, first) = model_step(&client, &server.base, &run, &token, 1, &request()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(provider_hits(&mock).await, 1);

    // The exact same step again: the recorded completion comes back, the provider
    // is not re-hit, and the log stays three events long.
    let (status, second) = model_step(&client, &server.base, &run, &token, 1, &request()).await;
    assert_eq!(status, StatusCode::OK, "retry: {second}");
    assert_eq!(
        second, first,
        "the recorded completion is returned verbatim"
    );
    assert_eq!(provider_hits(&mock).await, 1, "no re-pay");
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        3,
        "no growth"
    );
}

/// Test 3: a divergent request body at the same recorded position is a 409.
#[tokio::test]
async fn divergent_body_at_recorded_position_is_409() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;
    model_step(&client, &server.base, &run, &token, 1, &request()).await;

    // A different request at seq 1 hashes differently than the recorded intent.
    let divergent = json!({
        "model": "test-model",
        "max_tokens": 256,
        "messages": [{ "role": "user", "content": "a DIFFERENT prompt" }]
    });
    let (status, body) = model_step(&client, &server.base, &run, &token, 1, &divergent).await;
    assert_eq!(status, StatusCode::CONFLICT, "divergence: {body}");
    assert_eq!(body["error"]["code"], "divergence");
    assert_eq!(
        provider_hits(&mock).await,
        1,
        "the divergent retry ran nothing"
    );
}

/// Test 4: a log ending in a dangling model intent (a crash mid-call, seeded
/// through the store) re-executes on a matching model step and completes.
#[tokio::test]
async fn dangling_intent_reissues_and_completes() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    // Seed a dangling intent directly through the store (the generic append
    // refuses model kinds, and this is the crash the retry rule is for).
    let run_uuid = RunId::from_uuid(Uuid::parse_str(&run).unwrap());
    let request = request();
    let intent = EventEnvelope::new(
        run_uuid,
        SequenceNumber::new(1),
        ts(),
        Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: hash_value(&request),
            request_body: None,
            performed_by: None,
        },
    );
    server
        .state
        .store()
        .append(&intent)
        .await
        .expect("seed intent");

    // A model step at the dangling position with the matching body re-issues the
    // call (an unanswered request has no external effect to double) and records
    // the completion correlated to the recorded intent.
    let (status, body) = model_step(&client, &server.base, &run, &token, 1, &request).await;
    assert_eq!(status, StatusCode::OK, "re-issue: {body}");
    assert_eq!(body["response"]["content"][0]["text"], "the plan");
    assert_eq!(provider_hits(&mock).await, 1, "the dangling call ran once");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "the seeded intent gained its completion");
    let Event::ModelCallCompleted { seq: corr, .. } = &log[2].event else {
        panic!("seq 2 is the completion");
    };
    assert_eq!(
        corr.get(),
        1,
        "the completion correlates to the seeded intent"
    );
}

/// A provider that returns JSON for a non-streaming request and an equivalent
/// server-sent-events body for a streaming one, both folding to the same
/// response. This is the byte-identical parity pattern from the runtime tests.
struct DualModeProvider;

impl Respond for DualModeProvider {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        if body.get("stream").and_then(Value::as_bool) == Some(true) {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(equivalent_sse_body())
        } else {
            ResponseTemplate::new(200).set_body_json(non_streaming_body())
        }
    }
}

/// The non-streaming JSON body the dual-mode provider returns.
fn non_streaming_body() -> Value {
    json!({
        "id": "msg_parity",
        "model": "test-model",
        "role": "assistant",
        "content": [{ "type": "text", "text": "the plan: study otters" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 5 }
    })
}

/// One server-sent-events frame.
fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// An SSE body that folds to the exact response `non_streaming_body` parses to;
/// the text arrives as two deltas, so accumulation is exercised.
fn equivalent_sse_body() -> String {
    let mut body = frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_parity", "type": "message", "model": "test-model", "role": "assistant",
                "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 0}
            }
        }),
    );
    body.push_str(&frame(
        "content_block_start",
        &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "the plan: "}}),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "study otters"}}),
    ));
    body.push_str(&frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    body.push_str(&frame(
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 5}}),
    ));
    body.push_str(&frame("message_stop", &json!({"type": "message_stop"})));
    body
}

/// Parses server-sent-event frames from a buffered body into `(event, data)`.
fn parse_sse(text: &str) -> Vec<(String, String)> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            event.map(|event| (event, data))
        })
        .collect()
}

/// Test 5: the streaming variant delivers events AND records a completion
/// byte-identical to the non-streaming path for the same canned response.
#[tokio::test]
async fn streaming_delivers_events_and_records_byte_identical_completion() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(DualModeProvider)
        .mount(&mock)
        .await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();

    // Run A: non-streaming.
    let (run_a, token_a) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run_a,
        &token_a,
        vec![run_started_env(&run_a)],
    )
    .await;
    let (status, _) = model_step(&client, &server.base, &run_a, &token_a, 1, &request()).await;
    assert_eq!(status, StatusCode::OK);
    let log_a = read_log(&client, &server.base, &run_a).await;

    // Run B: streaming (Accept: text/event-stream).
    let (run_b, token_b) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run_b,
        &token_b,
        vec![run_started_env(&run_b)],
    )
    .await;
    let sse = client
        .post(format!("{}/v1/client-runs/{run_b}/model-step", server.base))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .header("x-drive-token", &token_b)
        .body(json!({ "seq": 1, "request": request() }).to_string())
        .send()
        .await
        .expect("streaming model-step sends");
    assert_eq!(sse.status(), StatusCode::OK);
    let frames = parse_sse(&sse.text().await.expect("read stream body"));

    let delta_frames = frames.iter().filter(|(event, _)| event == "delta").count();
    assert!(
        delta_frames > 0,
        "the live stream delivered ticker frames: {frames:?}"
    );
    let complete = frames
        .iter()
        .find(|(event, _)| event == "complete")
        .expect("a complete frame closes the stream");
    let complete: Value = serde_json::from_str(&complete.1).expect("complete frame is JSON");
    assert_eq!(
        complete["response"]["content"][0]["text"],
        "the plan: study otters"
    );

    // The load-bearing assertion: the recorded completion is byte-identical.
    let log_b = read_log(&client, &server.base, &run_b).await;
    let Event::ModelCallCompleted {
        response: ra,
        usage: ua,
        ..
    } = &log_a[2].event
    else {
        panic!("A seq 2 is a completion");
    };
    let Event::ModelCallCompleted {
        response: rb,
        usage: ub,
        ..
    } = &log_b[2].event
    else {
        panic!("B seq 2 is a completion");
    };
    assert_eq!(
        ra, rb,
        "streaming and non-streaming record the same response value"
    );
    assert_eq!(ua, ub, "and the same usage");
}

/// Test 6: a full client loop, then a fresh cursor rebuilt from the fetched log
/// re-drives with the model call replayed and zero live provider calls.
#[tokio::test]
async fn full_client_loop_then_replay_makes_no_live_call() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;

    // open -> RunStarted (generic append) -> model-step -> RunCompleted.
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;
    let request = request();
    let (status, _) = model_step(&client, &server.base, &run, &token, 1, &request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        append(
            &client,
            &server.base,
            &run,
            &token,
            vec![env_value(
                &run,
                3,
                Event::RunCompleted {
                    output: json!({ "done": true })
                }
            )],
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        provider_hits(&mock).await,
        1,
        "one live call across the whole loop"
    );

    // Re-open, rebuild a cursor from the fetched log, re-drive: every step is
    // replayed and the provider is never hit again.
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 4, "RunStarted, intent, completion, RunCompleted");
    let mut cursor = ReplayCursor::new(log).expect("the log is a well-formed run");
    assert!(matches!(
        cursor.begin("sha256:agent", None).expect("begin"),
        Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .model_call(&hash_value(&request), None)
            .expect("model call"),
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
    assert_eq!(provider_hits(&mock).await, 1, "replay paid nothing");
}

/// Test 7: `record_prompts` on the lease controls whether the request body is
/// recorded on the intent.
#[tokio::test]
async fn record_prompts_controls_request_body_on_intent() {
    let mock = json_provider().await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let request = request();

    // record_prompts = true: the intent carries the exact request body.
    let (run_on, token_on) = open_run(&client, &server.base, true).await;
    append(
        &client,
        &server.base,
        &run_on,
        &token_on,
        vec![run_started_env(&run_on)],
    )
    .await;
    model_step(&client, &server.base, &run_on, &token_on, 1, &request).await;
    let log_on = read_log(&client, &server.base, &run_on).await;
    let Event::ModelCallRequested { request_body, .. } = &log_on[1].event else {
        panic!("seq 1 is the intent");
    };
    assert_eq!(
        request_body.as_ref(),
        Some(&request),
        "the recorded body is the exact request"
    );

    // record_prompts = false: no body on the intent.
    let (run_off, token_off) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run_off,
        &token_off,
        vec![run_started_env(&run_off)],
    )
    .await;
    model_step(&client, &server.base, &run_off, &token_off, 1, &request).await;
    let log_off = read_log(&client, &server.base, &run_off).await;
    let Event::ModelCallRequested { request_body, .. } = &log_off[1].event else {
        panic!("seq 1 is the intent");
    };
    assert_eq!(request_body.as_ref(), None, "no body recorded when off");
}

/// A model step against a server with no executor wired is a 503, and no intent
/// is written for the call it cannot make, so the run stays drivable once one
/// exists.
#[tokio::test]
async fn missing_executor_is_503() {
    let factory = agent_factory(
        "http://127.0.0.1:1".to_owned(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    // No `with_model_executor`: this server cannot perform a model step.
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let (status, body) = model_step(&client, &server.base, &run, &token, 1, &request()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no executor: {body}"
    );
    assert_eq!(body["error"]["code"], "model_executor_unavailable");
    // No intent was written for a call the server cannot make, so the run is left
    // exactly where it was: the model step is retriable once an executor exists.
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 1, "only the RunStarted the client appended");
}

/// A model step whose upstream rejects the key with a 401 must not leak a bare
/// `x-api-key header is required`; the message has to name the environment
/// variable this server's client-driven model step reads it from, and say to
/// set it where the server runs, not the client driving the run over HTTP.
#[tokio::test]
async fn model_step_401_names_the_server_side_api_key_env() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "x-api-key header is required"
            }
        })))
        .mount(&mock)
        .await;
    let server = model_server(&mock).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base, false).await;
    append(
        &client,
        &server.base,
        &run,
        &token,
        vec![run_started_env(&run)],
    )
    .await;

    let (status, body) = model_step(&client, &server.base, &run, &token, 1, &request()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "401 upstream: {body}");
    assert_eq!(body["error"]["code"], "model_execution");
    let message = body["error"]["message"].as_str().expect("message string");
    assert!(
        message.contains("ANTHROPIC_API_KEY"),
        "message should name the env var this server reads: {message}"
    );
    assert!(
        message.contains("server"),
        "message should say to set the key where the server runs: {message}"
    );
}
