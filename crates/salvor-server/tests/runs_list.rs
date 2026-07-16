//! `GET /v1/runs`: the wire-shape pin for the enriched listing, and the
//! honest-absence contract for a run whose log cannot be read.
//!
//! There was no prior pinned-JSON test for this endpoint (checked before
//! writing this file), so [`list_pins_the_enriched_shape`] is a fresh pin
//! covering both the pre-existing fields (`run`, `status`, `event_count`,
//! `first_recorded_at`, `last_recorded_at`) and the new additive ones
//! (`usage`, `step_count`, `agent_def_hash`) in one shape, since that is now
//! the whole contract a consumer sees.

mod common;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    get_json, memory_store, open_store, post_json, register_agent, sample_toml, text_response,
    tool_use_response,
};
use reqwest::StatusCode;
use salvor_core::Effect;
use serde_json::{Value, json};

/// Starts a run through `server`/`agent` and streams it to completion,
/// returning its id.
async fn run_to_completion(client: &reqwest::Client, server: &TestServer, agent: &str) -> String {
    let (status, body) = post_json(
        client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "research otters" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {body}");
    let run_id = body["run"].as_str().expect("run id").to_owned();

    let mut reader = SseReader::open(client, &server.base, &run_id, None, None, None).await;
    let frames = reader.read_to_end().await;
    assert_eq!(
        frames.last().expect("at least one frame").json()["status"]["state"],
        "completed",
        "run reaches completed before the list is read"
    );
    run_id
}

#[tokio::test]
async fn list_pins_the_enriched_shape() {
    // Two model calls (a tool step, then the final turn), the same script
    // `full_lifecycle_streams_to_completion_with_matching_envelopes` in
    // lifecycle.rs uses, so the usage and step-count numbers are known.
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

    let run_id = run_to_completion(&client, &server, &agent).await;

    let (status, body) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "exactly the one run started");
    let run = &runs[0];

    assert_eq!(
        run["run"], run_id,
        "run id is the pre-existing field, unchanged"
    );

    // Normalize the fields whose exact value is nondeterministic (the run id
    // is already asserted above; the timestamps and the agent hash vary run
    // to run) to fixed placeholders, then pin the rest of the shape exactly,
    // key set and all.
    let mut normalized = run.clone();
    normalized["run"] = json!("<run-id>");
    normalized["first_recorded_at"] = json!("<ts>");
    normalized["last_recorded_at"] = json!("<ts>");
    assert_eq!(
        normalized["agent_def_hash"], agent,
        "agent_def_hash is the registered agent's own hash"
    );
    normalized["agent_def_hash"] = json!("<hash>");

    assert_eq!(
        normalized,
        json!({
            "run": "<run-id>",
            // Pre-existing fields, byte-identical in shape and value to
            // before this change.
            "status": { "state": "completed", "output": "all done" },
            "event_count": 10,
            "first_recorded_at": "<ts>",
            "last_recorded_at": "<ts>",
            // New, additive fields.
            "usage": { "input_tokens": 250, "output_tokens": 50 },
            "step_count": 2,
            "agent_def_hash": "<hash>",
        }),
        "the full enriched shape, old fields and new"
    );
}

#[tokio::test]
async fn list_reports_a_single_model_call_as_a_real_step_count_not_a_default() {
    // One model call, no tool use: the fold genuinely ran, so `step_count`
    // and `usage` are real, present values (1 and the scripted usage), the
    // sibling check to the corrupt-log test's "these are absent, never a
    // fabricated number" rule -- here the number is real, so it is present.
    let model = ScriptedModel::mount(vec![(1, text_response("done", 10, 2), None)]).await;
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
    run_to_completion(&client, &server, &agent).await;

    let (_, body) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let run = &body["runs"][0];
    assert_eq!(run["step_count"], 1, "one ModelCallRequested, a real count");
    assert_eq!(run["usage"]["input_tokens"], 10);
    assert_eq!(run["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn list_omits_folded_fields_for_a_run_whose_log_cannot_be_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("store.sqlite");
    let store = open_store(&db_path);

    let model = ScriptedModel::mount(vec![(1, text_response("all done", 10, 2), None)]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let server = TestServer::spawn(app_state(store, factory)).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    // One healthy run, and one whose log will be corrupted after it finishes.
    let healthy_run = run_to_completion(&client, &server, &agent).await;
    let corrupt_run = run_to_completion(&client, &server, &agent).await;

    // Tamper with the corrupt run's first row through a second raw connection
    // to the same file-backed store -- a stand-in for the "corrupt row on
    // disk" scenario `read_log` reports as `StoreError::Serialization`. The
    // aggregate columns `list_runs` reads (`recorded_at`) are untouched, so
    // the summary projection stays intact; only the JSON payload is broken.
    {
        let raw = rusqlite::Connection::open(&db_path).expect("open raw connection");
        let changed = raw
            .execute(
                "UPDATE events SET envelope = 'not valid json' WHERE run_id = ?1 AND seq = 0",
                rusqlite::params![corrupt_run],
            )
            .expect("corrupt the row");
        assert_eq!(changed, 1, "exactly the corrupt run's seq-0 row changed");
    }

    let (status, body) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "one corrupt run does not fail the whole list: {body}"
    );
    let runs = body["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 2, "both runs are still listed");

    let healthy = runs
        .iter()
        .find(|r| r["run"] == healthy_run)
        .expect("healthy run present");
    assert!(healthy.get("status").is_some(), "healthy run keeps status");
    assert!(healthy.get("usage").is_some(), "healthy run keeps usage");
    assert!(
        healthy.get("step_count").is_some(),
        "healthy run keeps step_count"
    );
    assert!(
        healthy.get("agent_def_hash").is_some(),
        "healthy run keeps agent_def_hash"
    );

    let corrupt = runs
        .iter()
        .find(|r| r["run"] == corrupt_run)
        .expect("corrupt run still listed");
    // The pre-existing, store-summary-only fields survive: they never needed
    // the unreadable row.
    assert!(corrupt.get("event_count").is_some());
    assert!(corrupt.get("first_recorded_at").is_some());
    assert!(corrupt.get("last_recorded_at").is_some());
    // Everything that needed the fold is honestly absent, not zeroed.
    assert_eq!(
        corrupt.get("status"),
        None,
        "status is unknowable, so it is absent, not a guess"
    );
    assert_eq!(corrupt.get("usage"), None, "usage is absent, never 0/0");
    assert_eq!(
        corrupt.get("step_count"),
        None,
        "step_count is absent, never 0"
    );
    assert_eq!(corrupt.get("agent_def_hash"), None);

    // Sanity: none of the absent keys serialize as JSON null either -- true
    // absence, not a null placeholder.
    let corrupt_str = corrupt.to_string();
    let _: Value = serde_json::from_str(&corrupt_str).expect("still valid JSON");
    assert!(!corrupt_str.contains("\"status\""));
    assert!(!corrupt_str.contains("\"usage\""));
}
