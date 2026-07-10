//! The forward flows over real HTTP: a full run streamed to completion, a
//! parked run resumed, and the auth gate.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    get_json, memory_store, post_json, register_agent, sample_toml, text_response,
    tool_use_response,
};
use reqwest::StatusCode;
use salvor_core::Effect;
use serde_json::json;

/// The event kinds carried by the non-terminal stream frames, in order.
fn streamed_kinds(frames: &[common::Frame]) -> Vec<String> {
    frames
        .iter()
        .filter(|frame| !frame.is_end())
        .map(|frame| {
            frame.json()["event"]["kind"]
                .as_str()
                .unwrap_or("?")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn full_lifecycle_streams_to_completion_with_matching_envelopes() {
    // Turn 1 (1 message): call `record`. Turn 2 (3 messages): final text, with
    // a small delay so the stream tails the tail live rather than replaying an
    // already-finished log.
    let model = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
            None,
        ),
        (
            3,
            text_response("all done", 150, 30),
            Some(Duration::from_millis(80)),
        ),
    ])
    .await;

    let calls = counter();
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        calls.clone(),
    );
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();

    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    // Start the run; the id comes back at once.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "research otters" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {body}");
    let run_id = body["run"].as_str().expect("run id").to_owned();

    // Stream to completion. Every frame is a pinned envelope; the last is the
    // terminal end frame.
    let mut reader = SseReader::open(&client, &server.base, &run_id, None, None, None).await;
    let frames = reader.read_to_end().await;

    let end = frames.last().expect("at least one frame");
    assert!(end.is_end(), "stream closes with an end frame");
    assert_eq!(end.json()["status"]["state"], "completed");

    assert_eq!(
        streamed_kinds(&frames),
        vec![
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted",
        ],
        "the streamed envelopes match the recorded log"
    );

    // The envelope frames carry the run id and ascending, gap-free sequence
    // numbers.
    let event_frames: Vec<_> = frames.iter().filter(|frame| !frame.is_end()).collect();
    for (index, frame) in event_frames.iter().enumerate() {
        let value = frame.json();
        assert_eq!(value["run_id"], run_id, "frame names the run");
        assert_eq!(frame.id, Some(index as u64), "frame id is the sequence");
        assert_eq!(value["seq"], index as u64, "envelope seq matches the id");
    }

    // Derived state: completed, with the exact scripted usage.
    let (status, run) = get_json(&client, &format!("{}/v1/runs/{run_id}", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"]["state"], "completed");
    assert_eq!(run["status"]["output"], "all done");
    assert_eq!(run["usage"]["input_tokens"], 250);
    assert_eq!(run["usage"]["output_tokens"], 50);

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool executed once");
}

#[tokio::test]
async fn parked_run_resumes_over_http() {
    let schema = json!({"type": "object", "required": ["approved"]});
    let model = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "approve", json!({}), 50, 5),
            None,
        ),
        (3, text_response("approved and done", 60, 6), None),
    ])
    .await;

    let calls = counter();
    let factory = agent_factory(
        model.uri(),
        "approve",
        Effect::Read,
        CountBehavior::Suspend(schema),
        calls,
    );
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "please approve" }),
        None,
    )
    .await;
    let run_id = body["run"].as_str().unwrap().to_owned();

    // The run streams its park: the end frame is a suspension.
    let mut reader = SseReader::open(&client, &server.base, &run_id, None, None, None).await;
    let parked = reader.read_to_end().await;
    let end = parked.last().unwrap();
    assert_eq!(end.json()["status"]["state"], "suspended");
    let suspend_seq = parked
        .iter()
        .rfind(|f| !f.is_end())
        .and_then(|f| f.id)
        .expect("a suspended event has a seq");

    // Missing the required field is a synchronous 400.
    let (status, _) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id}/resume", server.base),
        json!({ "input": {} }),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "schema-invalid input refused"
    );

    // A valid extension resumes it.
    let (status, resumed) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id}/resume", server.base),
        json!({ "input": { "approved": true } }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "resume accepted: {resumed}");

    // Streaming from just past the suspension shows the run finish.
    let mut reader = SseReader::open(
        &client,
        &server.base,
        &run_id,
        Some(suspend_seq + 1),
        None,
        None,
    )
    .await;
    let rest = reader.read_to_end().await;
    assert_eq!(rest.last().unwrap().json()["status"]["state"], "completed");

    let (_, run) = get_json(&client, &format!("{}/v1/runs/{run_id}", server.base), None).await;
    assert_eq!(run["status"]["output"], "approved and done");
}

#[tokio::test]
async fn auth_gate_requires_the_bearer() {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let state = app_state(memory_store(), factory).with_auth_token("s3cret");
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let runs = format!("{}/v1/runs", server.base);

    let (status, body) = get_json(&client, &runs, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token: {body}");
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _) = get_json(&client, &runs, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong token");

    let (status, _) = get_json(&client, &runs, Some("s3cret")).await;
    assert_eq!(status, StatusCode::OK, "correct token passes");
}
