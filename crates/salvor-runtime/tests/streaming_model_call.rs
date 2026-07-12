//! Streaming `model_call` proofs.
//!
//! [`RunCtx::model_call_streaming`] is a live-progress affordance layered on
//! top of the durable record: it streams events to a caller callback while the
//! call runs, and records exactly the completion the non-streaming
//! [`RunCtx::model_call`] records. These tests pin the invariant that governs
//! everything.
//!
//! - `both_paths_record_a_byte_identical_log` is the load-bearing one: the
//!   SAME underlying model response, one non-streaming JSON body and one SSE
//!   body that folds to the identical [`MessageResponse`], drives each path
//!   against its own fresh store, and the two logs compare equal envelope for
//!   envelope.
//! - `replay_of_a_streamed_run_makes_no_live_call` proves the replay branch:
//!   after a streamed live run records the completion, a fresh context over the
//!   stored log returns the same response with no server request and no
//!   per-event callback.
//! - `mid_stream_error_leaves_a_dangling_intent_that_reissues_on_resume` proves
//!   write-ahead (the intent is durable before the stream produces a
//!   completion), the mid-stream error surface, and safe re-issue on resume.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{fixed_clock, fixed_random, fixed_run_id};
use salvor_core::Event;
use salvor_llm::{Client, Config, Message, MessageRequest};
use salvor_runtime::{RunCtx, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The agent-definition hash every drive in this file begins under.
const DEF_HASH: &str = "sha256:streaming-model-call-v1";

/// The non-streaming JSON body: one text block, fixed usage. `send_message`
/// parses this into the [`MessageResponse`] the non-streaming path records.
fn non_streaming_body() -> Value {
    json!({
        "id": "msg_stream_parity",
        "model": "test-model",
        "role": "assistant",
        "content": [{"type": "text", "text": "the plan: study otters"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

/// Render one SSE frame: an `event:` line, a `data:` line, and a blank line.
fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// An SSE body that folds to the exact [`MessageResponse`] [`non_streaming_body`]
/// parses to: same id, model, role, single text block, stop reason, and usage.
/// The text arrives as two deltas, so accumulation is actually exercised.
fn equivalent_sse_body() -> String {
    let mut body = frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_stream_parity",
                "type": "message",
                "model": "test-model",
                "role": "assistant",
                "content": [],
                "stop_reason": null,
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }
        }),
    );
    body.push_str(&frame(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "the plan: "}
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "study otters"}
        }),
    ));
    body.push_str(&frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    body.push_str(&frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 5}
        }),
    ));
    body.push_str(&frame("message_stop", &json!({"type": "message_stop"})));
    body
}

/// A `200` SSE response carrying `body`.
fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A client pointed at `uri` with retries disabled (the scripts are
/// deterministic; a retried failure would only slow the test down).
fn client_for(uri: &str) -> Client {
    Client::new(Config::new().with_base_url(uri).with_max_retries(0)).expect("client builds")
}

/// The one-turn request both paths send.
fn request() -> MessageRequest {
    MessageRequest::new("test-model", 256).push_message(Message::user("draft a plan"))
}

/// The load-bearing proof: the same underlying response recorded through the
/// streaming and the non-streaming path produces byte-identical event logs.
#[tokio::test]
async fn both_paths_record_a_byte_identical_log() {
    // Non-streaming path: its own server, store, and context.
    let json_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(non_streaming_body()))
        .mount(&json_server)
        .await;
    let json_client = client_for(&json_server.uri());
    let json_store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(60);

    let mut ctx = RunCtx::with_hooks(
        json_store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin");
    let plain = ctx
        .model_call(&json_client, &request())
        .await
        .expect("non-streaming call");
    drop(ctx);
    let non_streaming_log = json_store.read_log(run_id).await.expect("log reads");

    // Streaming path: its own server, store, and context. Same run id so the
    // envelopes (run_id, seq, fixed timestamp) line up for the comparison.
    let sse_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(equivalent_sse_body()))
        .mount(&sse_server)
        .await;
    let sse_client = client_for(&sse_server.uri());
    let sse_store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));

    let mut ctx = RunCtx::with_hooks(
        sse_store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin");
    let mut events_seen = 0usize;
    let streamed = ctx
        .model_call_streaming(&sse_client, &request(), |_event| events_seen += 1)
        .await
        .expect("streaming call");
    drop(ctx);
    let streaming_log = sse_store.read_log(run_id).await.expect("log reads");

    // The live stream did fire the ticker (this is the whole point of the
    // streaming path), and returned the same typed turn.
    assert!(events_seen > 0, "the live stream must feed the ticker");
    assert_eq!(plain.response, streamed.response, "same typed response");
    assert_eq!(plain.usage, streamed.usage, "same recorded usage");

    // The load-bearing assertion: the two logs are equal envelope for envelope,
    // which means the same RunStarted, the same ModelCallRequested (incl.
    // request_hash), and the same ModelCallCompleted (response + usage).
    assert_eq!(
        non_streaming_log, streaming_log,
        "streaming and non-streaming must record byte-identical logs"
    );

    // Spell out the completion parity, so a failure names the field.
    let Event::ModelCallCompleted {
        response: plain_response,
        usage: plain_usage,
        ..
    } = &non_streaming_log[2].event
    else {
        panic!("non-streaming seq 2 is the completion");
    };
    let Event::ModelCallCompleted {
        response: streamed_response,
        usage: streamed_usage,
        ..
    } = &streaming_log[2].event
    else {
        panic!("streaming seq 2 is the completion");
    };
    assert_eq!(plain_response, streamed_response, "recorded response value");
    assert_eq!(plain_usage, streamed_usage, "recorded usage");
}

/// Replay of a streamed run returns the recorded response with no live call and
/// fires no per-event callback.
#[tokio::test]
async fn replay_of_a_streamed_run_makes_no_live_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(equivalent_sse_body()))
        .expect(1) // exactly one live connection across both drives
        .mount(&server)
        .await;
    let client = client_for(&server.uri());
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(61);

    // Drive 1: live streamed call records the completion.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin");
    let mut live_events = 0usize;
    let live = ctx
        .model_call_streaming(&client, &request(), |_| live_events += 1)
        .await
        .expect("live streaming call");
    drop(ctx);
    assert!(live_events > 0, "the live drive streamed events");

    // Drive 2: fresh context over the recorded log. Replay answers the call
    // from history, so the callback must never fire and no request must reach
    // the server.
    let log = store.read_log(run_id).await.expect("log reads");
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, fixed_clock(), fixed_random())
        .expect("ctx builds over the recorded log");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin replays");
    let mut replay_events = 0usize;
    let replayed = ctx
        .model_call_streaming(&client, &request(), |_| replay_events += 1)
        .await
        .expect("replayed streaming call");

    assert_eq!(
        replay_events, 0,
        "replay has no live tokens, so the ticker never fires"
    );
    assert_eq!(
        replayed.response, live.response,
        "replay returns the recorded response exactly"
    );
    assert_eq!(
        replayed.usage, live.usage,
        "replay returns the recorded usage"
    );

    // The `.expect(1)` on the mock verifies on drop that the server saw exactly
    // one request; make it explicit too.
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1, "the replayed call never hit the server");
}

/// First call serves a mid-stream `error` event, the second serves the good
/// stream: exactly the interruption-then-resume shape.
struct ErrorThenGoodStream {
    calls: AtomicUsize,
}

impl Respond for ErrorThenGoodStream {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // A 200 that starts a message and then errors mid-stream. Status 200
            // means the error is surfaced, never retried.
            let mut body = frame(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_stream_parity",
                        "type": "message",
                        "model": "test-model",
                        "role": "assistant",
                        "content": [],
                        "stop_reason": null,
                        "usage": {"input_tokens": 10, "output_tokens": 0}
                    }
                }),
            );
            body.push_str(&frame(
                "error",
                &json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "the stream fell over"}
                }),
            ));
            sse_response(body)
        } else {
            sse_response(equivalent_sse_body())
        }
    }
}

/// A mid-stream error surfaces as `RuntimeError::Model`, leaves the write-ahead
/// intent durable with no completion (a dangling intent), and the same call
/// re-issues safely on resume and records the completion.
#[tokio::test]
async fn mid_stream_error_leaves_a_dangling_intent_that_reissues_on_resume() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ErrorThenGoodStream {
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let client = client_for(&server.uri());
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(62);

    // Drive 1: the stream fails mid-flight. The call returns RuntimeError::Model
    // (the same error the non-streaming path returns for a failed provider
    // call), and no completion is recorded.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin");
    let mut events_before_error = 0usize;
    let result = ctx
        .model_call_streaming(&client, &request(), |_| events_before_error += 1)
        .await;
    assert!(
        matches!(result, Err(RuntimeError::Model(_))),
        "a mid-stream error surfaces as RuntimeError::Model, got {result:?}"
    );
    drop(ctx);

    // Write-ahead held: the intent is durable, but there is no completion. The
    // log ends at a dangling ModelCallRequested.
    let log = store.read_log(run_id).await.expect("log reads");
    assert!(
        matches!(
            log.last().map(|envelope| &envelope.event),
            Some(Event::ModelCallRequested { .. })
        ),
        "the write-ahead intent is durable with no completion (dangling intent)"
    );
    let completions = log
        .iter()
        .filter(|e| matches!(e.event, Event::ModelCallCompleted { .. }))
        .count();
    assert_eq!(
        completions, 0,
        "no completion was recorded for the failed stream"
    );

    // Drive 2: resume over the recorded log against a now-healthy stream. The
    // dangling intent re-issues (a fresh completion correlates to it), and the
    // run records the completion.
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, fixed_clock(), fixed_random())
        .expect("ctx builds over the recorded log");
    ctx.begin(DEF_HASH, &json!("draft a plan"))
        .await
        .expect("begin replays");
    let recovered = ctx
        .model_call_streaming(&client, &request(), |_| {})
        .await
        .expect("the re-issued call succeeds");
    assert_eq!(recovered.response.text(), "the plan: study otters");
    drop(ctx);

    let final_log = store.read_log(run_id).await.expect("log reads");
    let completions = final_log
        .iter()
        .filter(|e| matches!(e.event, Event::ModelCallCompleted { .. }))
        .count();
    assert_eq!(
        completions, 1,
        "the re-issued call recorded exactly one completion"
    );
}
