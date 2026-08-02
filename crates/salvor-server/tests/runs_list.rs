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
    // to the same file-backed store -- a stand-in for the "unreadable row on
    // disk" scenario, which `read_log` now refuses with
    // `StoreError::TamperEvident` because the row no longer matches the run's
    // hash chain. The aggregate columns `list_runs` reads (`recorded_at`) are
    // untouched, so the summary projection stays intact; only the log is
    // unreadable.
    //
    // The store keeps triggers on `events` that refuse UPDATE and DELETE, so
    // the tamper has to drop them first. That is the point of them: they stop
    // a casual edit, they do not stop anyone who can write the file, and the
    // chain is what catches the write either way.
    {
        let raw = rusqlite::Connection::open(&db_path).expect("open raw connection");
        raw.execute_batch(
            "DROP TRIGGER IF EXISTS events_refuse_update;
             DROP TRIGGER IF EXISTS events_refuse_delete;",
        )
        .expect("drop the append-only guards");
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

/// A run started with `labels` reports them back on `GET /v1/runs`; a run
/// started with none omits the key entirely (never `"labels": {}`), the same
/// zero-vs-absent honesty `agent_def_hash` already gets.
#[tokio::test]
async fn list_reports_labels_when_present_and_omits_them_otherwise() {
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

    // Labeled run.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({
            "agent": agent,
            "input": "research otters",
            "labels": { "build": "42", "env": "prod" },
        }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {body}");
    let labeled_run = body["run"].as_str().expect("run id").to_owned();
    let mut reader = SseReader::open(&client, &server.base, &labeled_run, None, None, None).await;
    reader.read_to_end().await;

    // Unlabeled run.
    let unlabeled_run = run_to_completion(&client, &server, &agent).await;

    let (status, body) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().expect("runs array");

    let labeled = runs
        .iter()
        .find(|r| r["run"] == labeled_run)
        .expect("labeled run present");
    assert_eq!(
        labeled["labels"],
        json!({ "build": "42", "env": "prod" }),
        "labels recorded at creation come back on list"
    );

    let unlabeled = runs
        .iter()
        .find(|r| r["run"] == unlabeled_run)
        .expect("unlabeled run present");
    assert_eq!(
        unlabeled.get("labels"),
        None,
        "no labels recorded means the key is absent, not an empty object"
    );
    assert!(
        !unlabeled.to_string().contains("\"labels\""),
        "absence is a missing key, never a null or empty placeholder"
    );
}

/// `POST /v1/runs` rejects labels that violate the sanity bounds before the
/// run is even spawned: a synchronous `400`, not a background task failure,
/// and no run is created at all.
#[tokio::test]
async fn start_rejects_labels_over_the_bounds() {
    let model = ScriptedModel::mount(vec![]).await;
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

    let too_many: serde_json::Map<String, Value> =
        (0..17).map(|i| (format!("k{i}"), json!("v"))).collect();
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "x", "labels": too_many }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    let (status, body) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["runs"].as_array().expect("runs array").len(),
        0,
        "a rejected start creates no run at all"
    );
}

/// Polls `GET /v1/runs/{id}` until the run reports `want` as its status state,
/// or panics after a bounded wait. Used to await a mid-step resting point a
/// hanging tool produces, without a fixed sleep.
async fn wait_for_status(client: &reqwest::Client, server: &TestServer, run: &str, want: &str) {
    for _ in 0..200 {
        let (_, body) = get_json(client, &format!("{}/v1/runs/{run}", server.base), None).await;
        if body["status"]["state"] == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("run {run} never reached status {want}");
}

/// A server-driven run reports an ATTACHED driver while a task is driving it,
/// and NONE once that task is gone: the same `is_run_active` truth the event
/// stream's `detached` end-frame already reports, now surfaced on the run list
/// so the dashboard can derive a stalled verdict. The status fold is identical
/// before and after the abort (the log does not change); only the liveness
/// evidence flips, which is exactly the point: a `running`/mid-step run that
/// LOOKS alive but has no driver is the stall this makes visible.
#[tokio::test]
async fn list_reports_driver_attached_while_driving_and_none_after_the_task_is_gone() {
    // Turn 1 calls a Read tool that hangs, so the run rests at a dangling read
    // intent (awaiting_tool) with its driver task still alive on the hang.
    let model = ScriptedModel::mount(vec![(
        1,
        tool_use_response("record", "record", json!({ "line": "otters" }), 40, 4),
        None,
    )])
    .await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Hang,
        counter(),
    );
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "research otters" }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "start: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();

    // The tool is now hanging: the run is mid-step and its task is alive.
    wait_for_status(&client, &server, &run, "awaiting_tool").await;

    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"][0].clone();
    assert_eq!(entry["run"], run);
    assert_eq!(entry["status"]["state"], "awaiting_tool", "mid-step");
    assert_eq!(
        entry["driver"], "attached",
        "a live driver task is an attached driver"
    );

    // Kill every driver task, as a server shutdown or a `kill -9` would. Nothing
    // is written; the log still ends at the dangling read intent.
    server.state.abort_all();

    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"][0].clone();
    assert_eq!(
        entry["status"]["state"], "awaiting_tool",
        "the fold is unchanged: the run still LOOKS mid-step"
    );
    assert_eq!(
        entry["driver"], "none",
        "the task is gone, so no driver is attached: the stall the dashboard derives"
    );

    // The single-run read agrees with the list.
    let (_, one) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(one["driver"], "none");
}

/// A terminal run omits `driver` entirely: asking whether a driver is attached
/// to a finished run is not a meaningful question, so the field is absent, never
/// `"driver": "none"`. Same zero-vs-absent house rule the folded fields follow.
#[tokio::test]
async fn list_omits_driver_for_a_terminal_run() {
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
    let run = run_to_completion(&client, &server, &agent).await;

    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"][0].clone();
    assert_eq!(entry["status"]["state"], "completed");
    assert_eq!(
        entry.get("driver"),
        None,
        "a completed run carries no driver"
    );
    assert!(
        !entry.to_string().contains("\"driver\""),
        "absence is a missing key, never a null placeholder"
    );

    // The single-run read omits it too.
    let (_, one) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(one.get("driver"), None);
}
