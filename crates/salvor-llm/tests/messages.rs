//! Hermetic integration tests for the Messages API client.
//!
//! Every test runs against a local [`wiremock`] mock server, so nothing here
//! touches the network. Each test maps to one behaviour of the client: request
//! shape, response parsing, the tool round trip, retry handling, and forward
//! compatibility.

use std::sync::atomic::{AtomicUsize, Ordering};

use salvor_llm::{
    AuthKind, Client, Config, ContentBlock, Error, Message, MessageRequest, StopReason, Tool,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A canned successful text response.
fn text_response() -> Value {
    json!({
        "id": "msg_text",
        "model": "claude-opus-4-8",
        "role": "assistant",
        "content": [{ "type": "text", "text": "Hi there" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 5 }
    })
}

/// Build a client pointed at the mock server, with an optional API key sent
/// under the default `x-api-key` scheme.
fn client_for(server: &MockServer, api_key: Option<&str>) -> Client {
    client_for_auth(server, api_key, AuthKind::ApiKey)
}

/// Build a client pointed at the mock server, with an optional API key sent
/// under an explicit authentication scheme.
fn client_for_auth(server: &MockServer, api_key: Option<&str>, auth_kind: AuthKind) -> Client {
    let mut config = Config::new()
        .with_base_url(server.uri())
        .with_max_retries(2)
        .with_auth_kind(auth_kind);
    if let Some(key) = api_key {
        config = config.with_api_key(key);
    }
    Client::new(config).expect("client builds")
}

// 1. Request shape: correct path, headers, and JSON body for a text turn.
#[tokio::test]
async fn sends_correct_path_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, Some("secret-key"));
    let request = MessageRequest::new("claude-opus-4-8", 1024)
        .with_system("You are terse.")
        .push_message(Message::user("Hello"));
    client
        .send_message(&request)
        .await
        .expect("request succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    let recorded = &requests[0];

    assert_eq!(recorded.method.as_str(), "POST");
    assert_eq!(recorded.url.path(), "/v1/messages");
    assert_eq!(header(recorded, "x-api-key").as_deref(), Some("secret-key"));
    // The default scheme is x-api-key only: the bearer-mode headers must not
    // leak into it.
    assert!(
        header(recorded, "authorization").is_none(),
        "authorization must be absent under the api-key scheme"
    );
    assert!(
        header(recorded, "anthropic-beta").is_none(),
        "the oauth beta header must be absent under the api-key scheme"
    );
    assert_eq!(
        header(recorded, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert!(
        header(recorded, "content-type")
            .unwrap_or_default()
            .contains("application/json")
    );

    let body: Value = serde_json::from_slice(&recorded.body).expect("body is json");
    assert_eq!(body["model"], "claude-opus-4-8");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["system"], "You are terse.");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "Hello");
    // The non-streaming path must not carry a `stream` key: the field defaults
    // false and is skipped, so the recorded request shape is unchanged.
    assert!(
        body.get("stream").is_none(),
        "a non-streaming request must not serialize a stream flag"
    );
}

// 1 (continued). The x-api-key header is absent when no key is configured.
#[tokio::test]
async fn omits_api_key_header_when_unconfigured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    let client = client_for(&server, None);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hello"));
    client
        .send_message(&request)
        .await
        .expect("request succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    assert!(
        header(&requests[0], "x-api-key").is_none(),
        "x-api-key must be absent when no key is configured"
    );
}

// 1 (continued). Bearer mode sends the OAuth headers and no x-api-key.
#[tokio::test]
async fn bearer_mode_sends_oauth_headers_and_no_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    let client = client_for_auth(&server, Some("sk-ant-oat-token"), AuthKind::Bearer);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hello"));
    client
        .send_message(&request)
        .await
        .expect("request succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    let recorded = &requests[0];
    assert_eq!(
        header(recorded, "authorization").as_deref(),
        Some("Bearer sk-ant-oat-token")
    );
    assert_eq!(
        header(recorded, "anthropic-beta").as_deref(),
        Some("oauth-2025-04-20")
    );
    assert!(
        header(recorded, "x-api-key").is_none(),
        "x-api-key must be absent under the bearer scheme"
    );
}

// 1 (continued). Bearer mode with no key configured sends no auth headers at
// all, exactly like the api-key scheme.
#[tokio::test]
async fn bearer_mode_omits_all_auth_headers_when_unconfigured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    let client = client_for_auth(&server, None, AuthKind::Bearer);
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hello"));
    client
        .send_message(&request)
        .await
        .expect("request succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    let recorded = &requests[0];
    assert!(
        header(recorded, "authorization").is_none(),
        "authorization must be absent when no key is configured"
    );
    assert!(
        header(recorded, "anthropic-beta").is_none(),
        "the oauth beta header must be absent when no key is configured"
    );
    assert!(
        header(recorded, "x-api-key").is_none(),
        "x-api-key must be absent when no key is configured"
    );
}

// 2. Response parsing: a text response exposes text and usage.
#[tokio::test]
async fn parses_text_response_with_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response()))
        .mount(&server)
        .await;

    let client = client_for(&server, Some("k"));
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let response = client.send_message(&request).await.expect("succeeds");

    assert_eq!(response.text(), "Hi there");
    assert_eq!(response.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
}

// 2 (continued). A tool_use response gives typed access to id, name, and input.
#[tokio::test]
async fn parses_tool_use_response() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_tool",
        "model": "claude-opus-4-8",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "Let me check." },
            {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "get_weather",
                "input": { "location": "Paris" }
            }
        ],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 12, "output_tokens": 8 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = client_for(&server, Some("k"));
    let tool = Tool::new(
        "get_weather",
        "Look up the weather for a location",
        json!({ "type": "object", "properties": { "location": { "type": "string" } } }),
    );
    let request = MessageRequest::new("claude-opus-4-8", 1024)
        .with_tools(vec![tool])
        .push_message(Message::user("Weather in Paris?"));
    let response = client.send_message(&request).await.expect("succeeds");

    assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
    let tool_uses = response.tool_uses();
    assert_eq!(tool_uses.len(), 1);
    let (id, name, input) = tool_uses[0];
    assert_eq!(id, "toolu_1");
    assert_eq!(name, "get_weather");
    assert_eq!(input["location"], "Paris");
}

// 3. Tool round trip: the tool_result follow-up message serializes correctly.
#[tokio::test]
async fn builds_tool_result_follow_up_message() {
    let message = Message::tool_result("toolu_1", "72F and sunny");
    let serialized = serde_json::to_value(&message).expect("serializes");

    assert_eq!(
        serialized,
        json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "72F and sunny"
                }
            ]
        })
    );
}

// 3 (continued). Image and document input blocks round-trip through serde,
//    including the URL and unknown source forms.
#[tokio::test]
async fn image_and_document_blocks_round_trip() {
    let blocks = vec![
        ContentBlock::image_base64("image/png", "aGVsbG8="),
        ContentBlock::image_url("https://example.com/cat.png"),
        ContentBlock::document_pdf("JVBERi0="),
        ContentBlock::document_base64("application/pdf", "cGRmYnl0ZXM="),
        ContentBlock::document_url("https://example.com/report.pdf"),
    ];
    for block in blocks {
        let json = serde_json::to_value(&block).expect("serializes");
        let restored: ContentBlock = serde_json::from_value(json).expect("deserializes");
        assert_eq!(restored, block, "block must survive a round trip");
    }

    // A source shape the client does not model (a Files API file_id reference)
    // still round-trips through the Unknown fallback.
    let file_block = json!({
        "type": "image",
        "source": { "type": "file", "file_id": "file_123" }
    });
    let parsed: ContentBlock =
        serde_json::from_value(file_block.clone()).expect("unknown source parses");
    assert_eq!(
        serde_json::to_value(&parsed).expect("re-serializes"),
        file_block,
        "an unknown source form must re-serialize unchanged"
    );
}

// 3 (continued). Pinned wire JSON: an image block and a document block
//    serialize to the exact shapes the current Messages API expects.
//
// Shapes cited from the claude-api skill (Document & File Input quick reference
// and the Python/TypeScript Vision examples): an image block is
// `{"type":"image","source":{"type":"base64"|"url",...}}` and a PDF document is
// `{"type":"document","source":{"type":"base64","media_type":"application/pdf",...}}`.
#[test]
fn image_block_matches_pinned_wire_shape() {
    let block = ContentBlock::image_base64("image/png", "aGVsbG8=");
    assert_eq!(
        serde_json::to_value(&block).expect("serializes"),
        json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "aGVsbG8="
            }
        })
    );

    let url_block = ContentBlock::image_url("https://example.com/cat.png");
    assert_eq!(
        serde_json::to_value(&url_block).expect("serializes"),
        json!({
            "type": "image",
            "source": { "type": "url", "url": "https://example.com/cat.png" }
        })
    );
}

#[test]
fn document_block_matches_pinned_wire_shape() {
    let block = ContentBlock::document_pdf("JVBERi0=");
    assert_eq!(
        serde_json::to_value(&block).expect("serializes"),
        json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBERi0="
            }
        })
    );
}

/// A responder that returns `429` once with `retry-after: 0`, then `200`.
struct RetryOnce {
    calls: AtomicUsize,
}

impl Respond for RetryOnce {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(json!({
                    "type": "error",
                    "error": { "type": "rate_limit_error", "message": "slow down" }
                }))
        } else {
            ResponseTemplate::new(200).set_body_json(text_response())
        }
    }
}

// 4. Retry: a 429 (retry-after 0) then a 200 succeeds with exactly one retry.
#[tokio::test]
async fn retries_once_on_rate_limit_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(RetryOnce {
            calls: AtomicUsize::new(0),
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = client_for(&server, Some("k"));
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let response = client
        .send_message(&request)
        .await
        .expect("succeeds after retry");

    assert_eq!(response.text(), "Hi there");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2, "expected exactly one retry");
}

// 4 (continued). A 400 fails immediately with a typed API error, no retries.
#[tokio::test]
async fn four_hundred_fails_immediately() {
    let server = MockServer::start().await;
    let body = json!({
        "type": "error",
        "error": { "type": "invalid_request_error", "message": "max_tokens is required" },
        "request_id": "req_abc"
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(body))
        .mount(&server)
        .await;

    let client = client_for(&server, Some("k"));
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let error = client
        .send_message(&request)
        .await
        .expect_err("should fail");

    match error {
        Error::Api(api) => {
            assert_eq!(api.status, 400);
            assert_eq!(api.kind, "invalid_request_error");
            assert_eq!(api.message, "max_tokens is required");
        }
        other => panic!("expected Error::Api, got {other:?}"),
    }

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "a 400 must not be retried");
}

// 5. Forward compatibility: unknown block type, unknown stop reason, and extra
//    top-level fields all parse without error.
#[tokio::test]
async fn tolerates_unknown_fields() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_future",
        "model": "claude-future",
        "role": "assistant",
        "content": [
            { "type": "text", "text": "ok" },
            { "type": "quantum_block", "payload": { "a": 1 } }
        ],
        "stop_reason": "some_new_reason",
        "usage": { "input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 3 },
        "new_top_level_field": { "x": true }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = client_for(&server, Some("k"));
    let request = MessageRequest::new("claude-opus-4-8", 1024).push_message(Message::user("Hi"));
    let response = client.send_message(&request).await.expect("parses");

    assert_eq!(response.text(), "ok");
    assert_eq!(
        response.stop_reason,
        Some(StopReason::Other("some_new_reason".to_string()))
    );
    assert_eq!(response.usage.cache_read_input_tokens, Some(3));

    match &response.content[1] {
        ContentBlock::Unknown(value) => {
            assert_eq!(value["type"], "quantum_block");
            assert_eq!(value["payload"]["a"], 1);
        }
        other => panic!("expected an unknown block, got {other:?}"),
    }
}

/// Read a request header as an owned string, if present.
fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
