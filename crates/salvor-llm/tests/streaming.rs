//! Hermetic integration tests for the streaming Messages API client.
//!
//! Each test serves a canned server-sent-events body from a local [`wiremock`]
//! mock, so nothing here touches the network. The tests cover the two streaming
//! surfaces (the live event feed and the final-message fold), the proof that the
//! folded message equals the non-streaming result, forward tolerance of unknown
//! events and deltas, and the `error` event and initial-connection retry paths.

use std::sync::atomic::{AtomicUsize, Ordering};

use salvor_llm::{
    Client, Config, Error, Message, MessageAccumulator, MessageRequest, MessageResponse,
    StopReason, StreamEvent,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Build a client pointed at the mock server.
fn client_for(server: &MockServer) -> Client {
    let config = Config::new()
        .with_base_url(server.uri())
        .with_max_retries(2)
        .with_api_key("k");
    Client::new(config).expect("client builds")
}

/// Render one SSE frame: an `event:` line, a `data:` line, and a blank line.
fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The canned SSE body used by the accumulation tests: two text deltas and a
/// tool call whose input arrives as two JSON fragments, with a ping and an
/// unknown event/delta interleaved to exercise forward tolerance.
fn canned_stream() -> String {
    let mut body = String::new();
    body.push_str(&frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_stream",
                "type": "message",
                "model": "claude-opus-4-8",
                "role": "assistant",
                "content": [],
                "stop_reason": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        }),
    ));
    body.push_str(&frame("ping", &json!({ "type": "ping" })));
    body.push_str(&frame(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "Hello, " }
        }),
    ));
    // An unknown delta type must be tolerated and leave the block unchanged.
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "future_delta", "payload": 1 }
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "world" }
        }),
    ));
    body.push_str(&frame(
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": 0 }),
    ));
    body.push_str(&frame(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {} }
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": "{\"location\":" }
        }),
    ));
    body.push_str(&frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": "\"Paris\"}" }
        }),
    ));
    body.push_str(&frame(
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": 1 }),
    ));
    // An unknown top-level event type must be tolerated.
    body.push_str(&frame(
        "quantum_event",
        &json!({ "type": "quantum_event", "payload": true }),
    ));
    body.push_str(&frame(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use", "stop_sequence": null },
            "usage": { "output_tokens": 7 }
        }),
    ));
    body.push_str(&frame("message_stop", &json!({ "type": "message_stop" })));
    body
}

fn sse_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

// 6. Events parse in order; text, tool-call input, and usage accumulate; the
//    folded final message matches expectations. Also proves the live feed and
//    the accumulator work together (aarg's ticker use case).
#[tokio::test]
async fn streams_events_in_order_and_accumulates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(canned_stream()))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let mut stream = client.stream_message(&request).await.expect("connects");

    let mut labels = Vec::new();
    let mut accumulator = MessageAccumulator::new();
    while let Some(event) = stream.next_event().await {
        let event = event.expect("event parses");
        labels.push(label(&event));
        accumulator.apply(&event).expect("event applies");
    }

    // The events arrive in exactly the order the server emitted them, including
    // the ping, the unknown event, and the unknown delta.
    assert_eq!(
        labels,
        vec![
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta",
            "content_block_delta", // the unknown delta type
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "unknown", // the unknown event type
            "message_delta",
            "message_stop",
        ]
    );

    let message = accumulator.into_message().expect("assembles");
    assert_eq!(message.text(), "Hello, world");
    let tool_uses = message.tool_uses();
    assert_eq!(tool_uses.len(), 1);
    let (id, name, input) = tool_uses[0];
    assert_eq!(id, "toolu_1");
    assert_eq!(name, "get_weather");
    assert_eq!(input["location"], "Paris");
    assert_eq!(message.usage.input_tokens, 10);
    assert_eq!(message.usage.output_tokens, 7);
    assert_eq!(message.stop_reason, Some(StopReason::ToolUse));

    // The streaming request carried `stream: true`; the non-stream path never
    // sets it (asserted separately in tests/messages.rs).
    let requests = server.received_requests().await.expect("recorded requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body is json");
    assert_eq!(body["stream"], true);
}

// 3. The folded final message equals the non-streaming result for the same
//    canned response: identical content blocks and usage.
#[tokio::test]
async fn final_message_equals_non_streaming_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(canned_stream()))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let streamed = client
        .stream_message(&request)
        .await
        .expect("connects")
        .get_final_message()
        .await
        .expect("assembles");

    // The equivalent non-streaming JSON body, parsed exactly as send_message
    // would parse it.
    let non_streaming: MessageResponse = serde_json::from_value(json!({
        "id": "msg_stream",
        "model": "claude-opus-4-8",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "Hello, world" },
            { "type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": { "location": "Paris" } }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 10, "output_tokens": 7 }
    }))
    .expect("non-streaming body parses");

    assert_eq!(
        streamed, non_streaming,
        "the folded stream must equal the non-streaming response"
    );
}

// 5. An `error` SSE event becomes an Error, surfaced (not retried) mid-stream.
#[tokio::test]
async fn error_event_surfaces_as_error() {
    let server = MockServer::start().await;
    let mut body = frame(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_err",
                "type": "message",
                "model": "claude-opus-4-8",
                "role": "assistant",
                "content": [],
                "usage": { "input_tokens": 3, "output_tokens": 0 }
            }
        }),
    );
    body.push_str(&frame(
        "error",
        &json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "the stream fell over" }
        }),
    ));

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_response(body))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let mut stream = client.stream_message(&request).await.expect("connects");

    // The first event is the message_start.
    let first = stream
        .next_event()
        .await
        .expect("an event")
        .expect("parses");
    assert!(matches!(first, StreamEvent::MessageStart(_)));

    // The error event surfaces as an Error, not a variant.
    let second = stream.next_event().await.expect("an event");
    match second {
        Err(Error::Api(api)) => {
            assert_eq!(api.kind, "overloaded_error");
            assert_eq!(api.message, "the stream fell over");
            // Status 200 means is_retryable is false: a mid-stream error is
            // surfaced, never silently retried.
            assert!(!Error::Api(api).is_retryable());
        }
        other => panic!("expected Error::Api, got {other:?}"),
    }
}

/// A responder that returns `429` once with `retry-after: 0`, then the SSE body.
struct RetryOnceThenStream {
    calls: AtomicUsize,
    body: String,
}

impl Respond for RetryOnceThenStream {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(json!({
                    "type": "error",
                    "error": { "type": "rate_limit_error", "message": "slow down" }
                }))
        } else {
            sse_response(self.body.clone())
        }
    }
}

// 5 (continued). A failed initial connection is retried, then the stream flows.
#[tokio::test]
async fn retries_initial_stream_connection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(RetryOnceThenStream {
            calls: AtomicUsize::new(0),
            body: canned_stream(),
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let message = client
        .stream_message(&request)
        .await
        .expect("connects after retry")
        .get_final_message()
        .await
        .expect("assembles");

    assert_eq!(message.text(), "Hello, world");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2, "expected exactly one connection retry");
}

/// A short label for a stream event, for asserting arrival order.
fn label(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::MessageStart(_) => "message_start",
        StreamEvent::ContentBlockStart { .. } => "content_block_start",
        StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
        StreamEvent::ContentBlockStop { .. } => "content_block_stop",
        StreamEvent::MessageDelta { .. } => "message_delta",
        StreamEvent::MessageStop => "message_stop",
        StreamEvent::Ping => "ping",
        StreamEvent::Unknown(_) => "unknown",
    }
}
