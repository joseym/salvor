//! Durability over HTTP: a run killed mid-flight recovers on a fresh server
//! with no duplicate write, and a dangling write is refused then resolved.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    get_json, open_store, post_json, register_agent, sample_toml, text_response, tool_use_response,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, RunId};
use salvor_store::EventStore;
use serde_json::json;
use tempfile::tempdir;

/// Polls a run's log until `predicate` holds, or fails after a bounded wait.
async fn wait_for(
    store: &Arc<dyn EventStore>,
    run_id: RunId,
    label: &str,
    predicate: impl Fn(&[EventEnvelope]) -> bool,
) {
    for _ in 0..600 {
        let log = store.read_log(run_id).await.expect("read log");
        if predicate(&log) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

fn has_kind(log: &[EventEnvelope], matcher: impl Fn(&Event) -> bool) -> bool {
    log.iter().any(|envelope| matcher(&envelope.event))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_run_recovers_on_a_fresh_server_with_no_duplicate_write() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("salvor.db");

    // Turn 1 calls the write tool; turn 2 is delayed, so there is a window to
    // kill the run after the write is durably recorded but before completion.
    let model = ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
            None,
        ),
        (
            3,
            text_response("all done", 150, 30),
            Some(Duration::from_millis(600)),
        ),
    ])
    .await;

    let calls = counter();
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Write,
        CountBehavior::Record,
        calls.clone(),
    );

    // First server: start the run, then kill it once the write has committed.
    let first = TestServer::spawn(app_state(open_store(&path), factory.clone())).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &first.base, sample_toml(), None).await;

    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", first.base),
        json!({ "agent": agent, "input": "research otters" }),
        None,
    )
    .await;
    let run_id_text = body["run"].as_str().unwrap().to_owned();
    let run_id = RunId::from_uuid(uuid::Uuid::parse_str(&run_id_text).unwrap());

    wait_for(
        &first.state.store(),
        run_id,
        "the write to complete",
        |log| {
            has_kind(log, |event| {
                matches!(event, Event::ToolCallCompleted { .. })
            })
        },
    )
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the write ran once before the kill"
    );

    // Kill: abort the driver task and drop the server. The store file is all
    // that survives.
    first.state.abort_all();
    drop(first);

    // Fresh server over the same store. The in-process registry is empty, so
    // the definition is re-registered; its hash is stable, so the run's
    // recorded reference still resolves.
    let second = TestServer::spawn(app_state(open_store(&path), factory.clone())).await;
    let agent_again = register_agent(&client, &second.base, sample_toml(), None).await;
    assert_eq!(agent, agent_again, "the re-registered hash is unchanged");

    let (status, _) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id_text}/resume", second.base),
        json!({}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "recover accepted");

    let mut reader = SseReader::open(&client, &second.base, &run_id_text, None, None, None).await;
    let frames = reader.read_to_end().await;
    assert_eq!(
        frames.last().unwrap().json()["status"]["state"],
        "completed",
        "the recovered run completes"
    );

    // The write executed exactly once across the whole lifetime.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "recover replayed the write, it never re-executed"
    );

    let log = second.state.store().read_log(run_id).await.unwrap();
    let completions = log
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolCallCompleted { .. }))
        .count();
    assert_eq!(completions, 1, "exactly one recorded tool completion");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangling_write_is_refused_then_resolved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("salvor.db");

    // Turn 1 calls the write tool, which hangs; there is no turn 2, because the
    // run is killed while the tool is still running.
    let model = ScriptedModel::mount(vec![(
        1,
        tool_use_response("tu_1", "charge", json!({"amount": 10}), 40, 4),
        None,
    )])
    .await;

    let calls = counter();
    let factory = agent_factory(
        model.uri(),
        "charge",
        Effect::Write,
        CountBehavior::Hang,
        calls,
    );

    let first = TestServer::spawn(app_state(open_store(&path), factory.clone())).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &first.base, sample_toml(), None).await;

    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", first.base),
        json!({ "agent": agent, "input": "charge the card" }),
        None,
    )
    .await;
    let run_id_text = body["run"].as_str().unwrap().to_owned();
    let run_id = RunId::from_uuid(uuid::Uuid::parse_str(&run_id_text).unwrap());

    // Wait until the write intent is recorded (the tool is now hanging), then
    // kill: the log ends at a write intent with no completion.
    wait_for(&first.state.store(), run_id, "the write intent", |log| {
        has_kind(log, |event| {
            matches!(
                event,
                Event::ToolCallRequested {
                    effect: Effect::Write,
                    ..
                }
            )
        })
    })
    .await;
    first.state.abort_all();
    drop(first);

    let second = TestServer::spawn(app_state(open_store(&path), factory.clone())).await;
    register_agent(&client, &second.base, sample_toml(), None).await;

    // Resume refuses with a 409 that carries the recorded intent as evidence.
    let (status, refusal) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id_text}/resume", second.base),
        json!({}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "resume refuses: {refusal}");
    assert_eq!(refusal["error"]["code"], "needs_reconciliation");
    let intent = &refusal["error"]["details"]["intent"];
    assert_eq!(intent["tool"], "charge");
    assert_eq!(intent["effect"], "write");
    assert!(intent["seq"].is_number(), "intent names its seq");
    assert!(
        intent["recorded_at"].is_string(),
        "intent carries when it was recorded"
    );

    // Resolve records the completion by hand; the run leaves reconciliation.
    let (status, resolved) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id_text}/resolve", second.base),
        json!({ "output": { "charged": true } }),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "resolve records the completion: {resolved}"
    );
    assert_eq!(resolved["resolved"], true);
    assert_eq!(resolved["status"]["state"], "running");

    // Resolving again is refused: there is no longer a dangling write.
    let (status, _) = post_json(
        &client,
        &format!("{}/v1/runs/{run_id_text}/resolve", second.base),
        json!({ "output": {} }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "second resolve refused");

    let (_, run) = get_json(
        &client,
        &format!("{}/v1/runs/{run_id_text}", second.base),
        None,
    )
    .await;
    assert_ne!(run["status"]["state"], "needs_reconciliation");
}
