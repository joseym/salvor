//! The event stream's replay-and-resume contract: a dropped connection
//! reconnects with the cursor and sees no gaps and no duplicates through to
//! completion.

mod common;

use std::time::Duration;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    memory_store, post_json, register_agent, sample_toml, text_response, tool_use_response,
};
use salvor_core::Effect;
use serde_json::json;

/// The sequence numbers of the non-terminal frames.
fn seqs(frames: &[common::Frame]) -> Vec<u64> {
    frames
        .iter()
        .filter(|frame| !frame.is_end())
        .filter_map(|frame| frame.id)
        .collect()
}

#[tokio::test]
async fn dropped_stream_resumes_from_cursor_without_gaps_or_duplicates() {
    // A small delay on the final turn keeps the run in flight while the first
    // reader disconnects, so the reconnect resumes a live tail.
    let model = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
            None,
        ),
        (
            3,
            text_response("all done", 150, 30),
            Some(Duration::from_millis(120)),
        ),
    ])
    .await;

    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "research otters" }),
        None,
    )
    .await;
    let run_id = body["run"].as_str().unwrap().to_owned();

    // First connection: read the first three events, then drop it.
    let mut first = SseReader::open(&client, &server.base, &run_id, None, None, None).await;
    let seen = first.read_frames(3).await;
    let first_seqs = seqs(&seen);
    assert_eq!(first_seqs, vec![0, 1, 2], "the first frames are 0, 1, 2");
    let last_seen = *first_seqs.last().unwrap();
    drop(first);

    // Reconnect with Last-Event-ID; the tail resumes at last + 1.
    let mut second =
        SseReader::open(&client, &server.base, &run_id, None, Some(last_seen), None).await;
    let rest = second.read_to_end().await;

    assert_eq!(
        rest.last().unwrap().json()["status"]["state"],
        "completed",
        "the resumed stream runs through to completion"
    );

    let resumed_seqs = seqs(&rest);
    assert_eq!(
        *resumed_seqs.first().unwrap(),
        last_seen + 1,
        "the resume starts one past the last event seen, no duplicate"
    );

    // The two connections together cover every sequence once, in order.
    let mut all = first_seqs;
    all.extend(resumed_seqs);
    let expected: Vec<u64> = (0..all.len() as u64).collect();
    assert_eq!(all, expected, "gap-free and duplicate-free end to end");
}

#[tokio::test]
async fn from_seq_query_replays_from_a_chosen_point() {
    let model = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
            None,
        ),
        (3, text_response("all done", 150, 30), None),
    ])
    .await;

    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "x" }),
        None,
    )
    .await;
    let run_id = body["run"].as_str().unwrap().to_owned();

    // Let the run finish, then replay only from sequence 5 onward.
    let mut full = SseReader::open(&client, &server.base, &run_id, None, None, None).await;
    let _ = full.read_to_end().await;

    let mut tail = SseReader::open(&client, &server.base, &run_id, Some(5), None, None).await;
    let frames = tail.read_to_end().await;
    assert_eq!(
        *seqs(&frames).first().unwrap(),
        5,
        "?from_seq starts exactly at the requested sequence"
    );
    assert_eq!(
        frames.last().unwrap().json()["status"]["state"],
        "completed"
    );
}
