//! The client-driven run surface over real HTTP: open and resume, read the
//! log, and the generic guarded append with its drive-token lease and its
//! idempotency, divergence, and legality rules.

mod common;

use common::{
    CountBehavior, ScriptedModel, TestServer, agent_factory, app_state, counter, fixed_clock,
    get_json, memory_store, post_json, register_agent, sample_toml,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, ReplayCursor, RunId, SequenceNumber};
use salvor_server::{AppState, ClientToolDecl, ClientToolRegistry};
use serde_json::{Value, json};
use std::time::Duration;
use time::macros::datetime;
use uuid::Uuid;

/// A fixed recorded timestamp, so hand-built envelopes are stable. Distinct
/// from the server's own clock (see [`fixed_clock`]): a test that wants to
/// prove the server's stamp wins picks a `recorded_at` that could never be
/// confused with the server's.
fn ts() -> time::OffsetDateTime {
    datetime!(2026-07-11 12:00:00 UTC)
}

/// The wire JSON of an envelope for `run_id` at `seq`, carrying `ts()` as the
/// client's claimed `recorded_at`. The server overwrites this on every
/// append (see [`client_runs::append`](salvor_server) and the module docs on
/// `crates/salvor-server/src/client_runs.rs`), so the value chosen here is
/// never what lands in the store; [`env_value_at`] exists for tests that need
/// to choose it explicitly to prove exactly that.
fn env_value(run_id: &str, seq: u64, event: Event) -> Value {
    env_value_at(run_id, seq, ts(), event)
}

/// The wire JSON of an envelope for `run_id` at `seq`, carrying a caller-chosen
/// claimed `recorded_at`. Used to submit an absurd stamp (the Unix epoch, a
/// far-future date) and confirm the server discards it.
fn env_value_at(run_id: &str, seq: u64, recorded_at: time::OffsetDateTime, event: Event) -> Value {
    let run_id = RunId::from_uuid(Uuid::parse_str(run_id).expect("run id"));
    let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq), recorded_at, event);
    serde_json::to_value(envelope).expect("serialize envelope")
}

fn run_started() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:agent".into(),
        input: json!({ "topic": "otters" }),
        labels: None,
        driven_by: None,
    }
}

/// Spins up a server whose factory is never invoked (client-driven
/// runs perform no model call), but which the state still requires.
async fn client_server() -> TestServer {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    // Keep the mock server alive for the test's duration by leaking it; the
    // factory holds only its uri and is never called here.
    Box::leak(Box::new(model));
    TestServer::spawn(app_state(memory_store(), factory)).await
}

/// Opens a fresh client-driven run, returning its id and drive token.
async fn open_run(client: &reqwest::Client, base: &str) -> (String, String) {
    let (status, body) =
        post_json(client, &format!("{base}/v1/client-runs"), json!({}), None).await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    assert_eq!(
        body["log"],
        json!([]),
        "a fresh run opens with an empty log"
    );
    (
        body["run"].as_str().expect("run id").to_owned(),
        body["drive_token"]
            .as_str()
            .expect("drive token")
            .to_owned(),
    )
}

/// A guarded append carrying an optional drive token.
async fn append(
    client: &reqwest::Client,
    base: &str,
    run_id: &str,
    token: Option<&str>,
    events: Vec<Value>,
) -> (StatusCode, Value) {
    let mut request = client
        .post(format!("{base}/v1/client-runs/{run_id}/events"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(json!({ "events": events }).to_string());
    if let Some(token) = token {
        request = request.header("x-drive-token", token);
    }
    let response = request.send().await.expect("append sends");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn open_append_legal_sequence_and_read_back() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let events = vec![
        env_value(&run, 0, run_started()),
        env_value(&run, 1, Event::NowObserved { now: ts() }),
        env_value(&run, 2, Event::RandomObserved { value: 7 }),
        env_value(
            &run,
            3,
            Event::RunCompleted {
                output: json!({ "done": true }),
            },
        ),
    ];
    let (status, body) = append(&client, &server.base, &run, Some(&token), events).await;
    assert_eq!(status, StatusCode::OK, "legal append: {body}");
    assert_eq!(body["appended"], json!([0, 1, 2, 3]));

    // A second client reads the log back and rebuilds the cursor over it: every
    // recorded step replays, with nothing left to execute.
    let (status, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envelopes: Vec<EventEnvelope> =
        serde_json::from_value(log["log"].clone()).expect("decode log");
    assert_eq!(envelopes.len(), 4, "the log holds every appended event");

    let mut cursor = ReplayCursor::new(envelopes).expect("the log is a well-formed run");
    assert!(cursor.is_replaying(), "a full log replays");
    // Drive the recorded run: each step comes from the log, none executes.
    assert!(matches!(
        cursor.begin("sha256:agent", None).expect("begin"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor.now().expect("now"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor.random().expect("random"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .complete_run(&json!({ "done": true }))
            .expect("complete"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(
        cursor.is_finished(),
        "the run replayed to its terminal event"
    );
}

#[tokio::test]
async fn idempotent_retry_is_a_no_op() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let started = env_value(&run, 0, run_started());
    let now = env_value(&run, 1, Event::NowObserved { now: ts() });
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![started.clone(), now.clone()],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The exact same bytes at the same positions are a 200 no-op.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![started, now],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "idempotent retry: {body}");
    assert_eq!(body["appended"], json!([0, 1]));

    // The log did not grow.
    let (_, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(log["log"].as_array().unwrap().len(), 2, "no duplicate rows");
}

#[tokio::test]
async fn divergent_bytes_at_existing_seq_is_409() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A different event at the already-recorded seq 0 is a divergence.
    let divergent = env_value(
        &run,
        0,
        Event::RunStarted {
            agent_def_hash: "sha256:DIFFERENT".into(),
            input: json!({ "topic": "badgers" }),
            labels: None,
            driven_by: None,
        },
    );
    let (status, body) = append(&client, &server.base, &run, Some(&token), vec![divergent]).await;
    assert_eq!(status, StatusCode::CONFLICT, "divergence: {body}");
    assert_eq!(body["error"]["code"], "divergence");
}

#[tokio::test]
async fn illegal_next_event_is_409() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    // The first event of a fresh run must be RunStarted; a NowObserved is not
    // the legal next event.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, Event::NowObserved { now: ts() })],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "illegal head: {body}");
    assert_eq!(body["error"]["code"], "divergence");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("RunStarted"),
        "the validator's error surfaces: {body}"
    );

    // A duplicate RunStarted after a legal head is also rejected.
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 1, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate RunStarted: {body}");
    assert_eq!(body["error"]["code"], "divergence");
}

#[tokio::test]
async fn drive_token_is_required_and_checked() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    let events = vec![env_value(&run, 0, run_started())];

    // No token at all.
    let (status, body) = append(&client, &server.base, &run, None, events.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "missing token: {body}");
    assert_eq!(body["error"]["code"], "missing_drive_token");

    // A token that is not the run's lease.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some("dt_wrong"),
        events.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "wrong token: {body}");
    assert_eq!(body["error"]["code"], "invalid_drive_token");

    // The real lease works.
    let (status, _) = append(&client, &server.base, &run, Some(&token), events).await;
    assert_eq!(status, StatusCode::OK, "the current lease drives the run");
}

#[tokio::test]
async fn model_and_tool_kinds_are_rejected() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let model_intent = env_value(
        &run,
        1,
        Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
            request_body: None,
            performed_by: None,
        },
    );
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![model_intent],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "model event rejected: {body}"
    );
    assert_eq!(body["error"]["code"], "unsupported_event_kind");
}

#[tokio::test]
async fn reopening_returns_the_log_and_a_fresh_lease() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Re-opening the same run returns its recorded log and a fresh token.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", server.base),
        json!({ "run_id": run }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-open: {body}");
    assert_eq!(body["log"].as_array().unwrap().len(), 1, "log comes back");
    let fresh = body["drive_token"].as_str().unwrap();
    assert_ne!(fresh, token, "re-opening mints a fresh lease");

    // The superseded token no longer drives the run.
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 1, Event::NowObserved { now: ts() })],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the old lease is refused");
}

#[tokio::test]
async fn foreign_run_id_with_history_is_refused() {
    let server = client_server().await;
    let client = reqwest::Client::new();

    // Pre-populate the store with a run this process did not open as a
    // client-driven run (standing in for a server-driven run).
    let foreign = RunId::new();
    server
        .state
        .store()
        .append(&EventEnvelope::new(
            foreign,
            SequenceNumber::new(0),
            ts(),
            run_started(),
        ))
        .await
        .expect("seed a foreign run");

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", server.base),
        json!({ "run_id": foreign.as_uuid().to_string() }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "foreign run refused: {body}");
    assert_eq!(body["error"]["code"], "run_exists");

    // And it is not reachable through the client-driven log endpoint.
    let (status, _) = get_json(
        &client,
        &format!("{}/v1/client-runs/{}/log", server.base, foreign.as_uuid()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "modes stay separate");
}

#[tokio::test]
async fn log_read_honors_from_seq() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![
            env_value(&run, 0, run_started()),
            env_value(&run, 1, Event::NowObserved { now: ts() }),
            env_value(&run, 2, Event::RandomObserved { value: 3 }),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log?from_seq=1", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envelopes = log["log"].as_array().unwrap();
    assert_eq!(envelopes.len(), 2, "from_seq trims the prefix");
    assert_eq!(envelopes[0]["seq"], 1);
}

/// A client-synthesized `RunStarted` carrying labels over the sanity bounds is
/// rejected on append: `400`, and nothing is written. The client, not this
/// server, builds the event, so this is the one point the server ever
/// inspects `labels` for a client-driven run.
#[tokio::test]
async fn appended_run_started_with_too_many_labels_is_rejected() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let mut labels = std::collections::BTreeMap::new();
    for i in 0..17 {
        labels.insert(format!("k{i}"), "v".to_owned());
    }
    let over_the_cap = Event::RunStarted {
        agent_def_hash: "sha256:agent".into(),
        input: json!({ "topic": "otters" }),
        labels: Some(labels),
        driven_by: None,
    };
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, over_the_cap)],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    let (status, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        log["log"],
        json!([]),
        "a rejected RunStarted writes nothing"
    );
}

/// A client-synthesized `RunStarted` with labels inside the bounds is accepted
/// and comes back on the log unchanged, exercising the append path end to end
/// for the labeled case (the happy-path append test above never sets labels).
#[tokio::test]
async fn appended_run_started_with_labels_round_trips() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let labeled = Event::RunStarted {
        agent_def_hash: "sha256:agent".into(),
        input: json!({ "topic": "otters" }),
        labels: Some(std::collections::BTreeMap::from([(
            "build".to_owned(),
            "42".to_owned(),
        )])),
        driven_by: None,
    };
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, labeled)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        log["log"][0]["event"]["payload"]["labels"],
        json!({ "build": "42" }),
        "the accepted labels come back on read exactly as submitted"
    );
}

/// The real-world bug this fix closes: a client that claims the Unix epoch
/// (or a wildly far-future date) for `recorded_at` on `RunStarted`,
/// `NowObserved`, and `RunCompleted` gets back the SERVER's stamp on every one
/// of them, never its own claim. The stamp is `fixed_clock()`'s constant, the
/// same clock hook every server-performed step already stamped with, which is
/// this test's proof of the stamp's source: the recorded envelopes carry that
/// exact constant regardless of what the client sent or when the test
/// actually ran.
#[tokio::test]
async fn absurd_client_recorded_at_is_overwritten_with_the_server_stamp() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let epoch = datetime!(1970-01-01 00:00:00 UTC);
    let far_future = datetime!(2999-12-31 23:59:59 UTC);

    let events = vec![
        env_value_at(&run, 0, epoch, run_started()),
        env_value_at(&run, 1, far_future, Event::NowObserved { now: ts() }),
        env_value_at(
            &run,
            2,
            epoch,
            Event::RunCompleted {
                output: json!({ "done": true }),
            },
        ),
    ];
    let (status, body) = append(&client, &server.base, &run, Some(&token), events).await;
    assert_eq!(status, StatusCode::OK, "append: {body}");
    assert_eq!(body["appended"], json!([0, 1, 2]));

    let (status, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let envelopes: Vec<EventEnvelope> =
        serde_json::from_value(log["log"].clone()).expect("decode log");
    assert_eq!(envelopes.len(), 3, "every event landed");

    let server_stamp = fixed_clock()();
    for envelope in &envelopes {
        assert_eq!(
            envelope.recorded_at, server_stamp,
            "the server's own clock stamps every appended envelope, never the client's claim"
        );
    }

    // The append-guard and the replay cursor never look at `recorded_at`
    // (salvor-replay/src/validate.rs's `validate_next` and
    // salvor-replay/src/replay.rs's `ReplayCursor` match only on `.event`
    // content), so overwriting it here cannot desynchronize replay: the run
    // still folds to completion from the stamped log.
    let mut cursor = ReplayCursor::new(envelopes).expect("the log is a well-formed run");
    assert!(matches!(
        cursor.begin("sha256:agent", None).expect("begin"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor.now().expect("now"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(matches!(
        cursor
            .complete_run(&json!({ "done": true }))
            .expect("complete"),
        salvor_core::Outcome::Replayed(_)
    ));
    assert!(
        cursor.is_finished(),
        "the run replays to its terminal event even though every recorded_at was rewritten"
    );
}

/// A retry submitted with a different claimed `recorded_at` than the original
/// is still a no-op: `recorded_at` was never the client's fact to assert, so
/// it plays no part in deciding whether a resend at an already-recorded
/// position is the same event again.
#[tokio::test]
async fn retry_with_a_different_claimed_recorded_at_is_still_idempotent() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let epoch = datetime!(1970-01-01 00:00:00 UTC);
    let far_future = datetime!(2999-12-31 23:59:59 UTC);

    let first = env_value_at(&run, 0, epoch, run_started());
    let (status, _) = append(&client, &server.base, &run, Some(&token), vec![first]).await;
    assert_eq!(status, StatusCode::OK);

    let retry = env_value_at(&run, 0, far_future, run_started());
    let (status, body) = append(&client, &server.base, &run, Some(&token), vec![retry]).await;
    assert_eq!(status, StatusCode::OK, "retry: {body}");
    assert_eq!(body["appended"], json!([0]));

    let (_, log) = get_json(
        &client,
        &format!("{}/v1/client-runs/{run}/log", server.base),
        None,
    )
    .await;
    assert_eq!(
        log["log"].as_array().unwrap().len(),
        1,
        "no duplicate row from the retry"
    );
}

/// A client-driven run reports an ATTACHED driver while its lease is current,
/// and NONE once the lease lapses: the client-driven half of the liveness
/// evidence `GET /v1/runs` carries. This is the honest addition the design
/// needed: the pre-existing lease had no expiry, so a driverless client run
/// (the tab closed, the SDK exited) was indistinguishable from a live one.
///
/// A real clock and a short TTL are used deliberately: the lease's freshness is
/// a wall-clock property, so this test lets real time pass rather than driving a
/// fixed hook. Its logs are never compared against a control run, so it forgoes
/// the deterministic clock the other tests here rely on.
#[tokio::test]
async fn client_run_driver_is_attached_while_leased_and_none_once_it_lapses() {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    Box::leak(Box::new(model));
    // Real clock (no fixed hooks) and a short lease TTL, so the lease genuinely
    // lapses in bounded test time.
    let state = AppState::new(memory_store(), factory)
        .with_poll_interval(Duration::from_millis(10))
        .with_client_lease_ttl(Duration::from_millis(150));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let (run, token) = open_run(&client, &server.base).await;

    // Append a RunStarted so the run's log folds to `running`: the exact state a
    // stall hides in. The append presents the drive token, refreshing the lease.
    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Right away: the lease is current, so the run reports an attached driver.
    let (_, body) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(body["status"]["state"], "running", "the run is mid-flight");
    assert_eq!(
        body["driver"], "attached",
        "a current lease is an attached driver"
    );

    // Let the lease lapse: no further guarded operation refreshes it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (_, body) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(
        body["status"]["state"], "running",
        "the fold is unchanged: the run still LOOKS running"
    );
    assert_eq!(
        body["driver"], "none",
        "but the lease lapsed, so no driver is attached: the stall the dashboard derives"
    );

    // The list surface reports the same evidence for the same run.
    let (_, list) = get_json(&client, &format!("{}/v1/runs", server.base), None).await;
    let entry = list["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .find(|r| r["run"] == run)
        .expect("the client-driven run is enumerated once it has events");
    assert_eq!(
        entry["driver"], "none",
        "list agrees with the single-run read"
    );
}

/// The deadline the sleep tests park on: an hour BEFORE the server's fixed
/// clock, so the run is genuinely overdue while the sweeper looks at it. A
/// deadline the sweeper would skip anyway proves nothing about the skip.
fn due_wake_at() -> time::OffsetDateTime {
    datetime!(2026-07-10 11:00:00 UTC)
}

/// A client-driven run parks on a durable timer and wakes itself: both halves
/// of the pair go through the generic append, the run reads as `sleeping` with
/// its recorded `wake_at` in between, and the server never touches it.
///
/// The sweep in the middle is the load-bearing part. This run is overdue by an
/// hour against the server's own clock, so a sweeper that treated it like any
/// other sleeping run would spawn a driver against a log the client holds the
/// single-writer lease on. It leaves it alone, and the client's own next
/// append is what ends the sleep.
#[tokio::test]
async fn a_client_driven_run_sleeps_and_the_client_wakes_it() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    // The client's own drive: a recorded clock reading, then the sleep derived
    // from it. `wake_at` is the client's fact, exactly as it is in the runtime.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![
            env_value(&run, 0, run_started()),
            env_value(&run, 1, Event::NowObserved { now: ts() }),
            env_value(
                &run,
                2,
                Event::SleepStarted {
                    wake_at: due_wake_at(),
                },
            ),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the sleep pair's first half: {body}"
    );
    assert_eq!(body["appended"], json!([0, 1, 2]));

    let (status, body) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["status"]["state"], "sleeping",
        "the fold reports the park: {body}"
    );
    assert_eq!(
        body["status"]["wake_at"], "2026-07-10T11:00:00Z",
        "carrying the instant the client recorded"
    );

    // The sweeper's pass: this run is overdue and still left alone.
    assert!(
        salvor_server::sweep(&server.state).await.is_empty(),
        "an overdue client-driven run is not driven from the server"
    );
    assert_eq!(
        server
            .state
            .store()
            .read_log(RunId::from_uuid(Uuid::parse_str(&run).expect("run id")))
            .await
            .expect("log reads")
            .len(),
        3,
        "and its log is untouched by the pass"
    );

    // The client compares the recorded deadline against a fresh reading of its
    // own clock, finds it passed, and closes the pair itself.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![
            env_value(
                &run,
                3,
                Event::NowObserved {
                    now: datetime!(2026-07-10 11:30:00 UTC),
                },
            ),
            env_value(&run, 4, Event::SleepCompleted {}),
            env_value(
                &run,
                5,
                Event::RunCompleted {
                    output: json!({ "done": true }),
                },
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the woken run continues: {body}");
    assert_eq!(body["appended"], json!([3, 4, 5]));

    let (_, body) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(
        body["status"]["state"], "completed",
        "the sleep ended and the run finished: {body}"
    );
}

/// A `SleepCompleted` may close only a sleep the log has open. The shared
/// append-guard is lenient about the pair (a run still asleep has recorded only
/// the start, so nothing may demand the completion), which is exactly why this
/// surface, where the client hand-appends both halves, checks the order itself.
#[tokio::test]
async fn a_sleep_completion_with_no_sleep_started_is_refused() {
    let server = client_server().await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &server.base).await;

    let (status, _) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 0, run_started())],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 1, Event::SleepCompleted {})],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "unopened sleep: {body}");
    assert_eq!(body["error"]["code"], "divergence");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("has not started"),
        "the refusal names what is wrong: {body}"
    );

    // Nothing was written, so the run carries on from where it was: the same
    // position is still the one the log is ready for.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(
            &run,
            1,
            Event::SleepStarted {
                wake_at: due_wake_at(),
            },
        )],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the refusal wrote nothing: {body}");
    assert_eq!(body["appended"], json!([1]));

    // And once a sleep is open, the completion that was refused is accepted.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 2, Event::SleepCompleted {})],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the pair in order: {body}");
    assert_eq!(body["appended"], json!([2]));

    // A second completion has nothing left to close.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![env_value(&run, 3, Event::SleepCompleted {})],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "already awake: {body}");
    assert_eq!(body["error"]["code"], "divergence");
}

/// The server-driven resume endpoint refuses a client-driven run outright,
/// even when the agent its `RunStarted` names is registered on this very
/// server: that agent being buildable here is not permission to drive this
/// run here, since the run's client already holds the single-writer lease on
/// it.
#[tokio::test]
async fn resuming_a_client_driven_run_through_the_server_endpoint_is_refused() {
    let server = client_server().await;
    let client = reqwest::Client::new();

    // The same agent the run's RunStarted will name, registered on this
    // server: the exact case the owner decision calls out.
    let agent = register_agent(&client, &server.base, sample_toml(), None).await;

    let (run, token) = open_run(&client, &server.base).await;

    // Park it on a durable timer whose instant has not arrived against the
    // server's fixed clock (noon on 2026-07-10). An unguarded resume would
    // dispatch on this state and answer `409 still_sleeping`; the refusal
    // this test checks for must arrive before that dispatch, not instead of
    // it only when the run happens to be due.
    let (status, body) = append(
        &client,
        &server.base,
        &run,
        Some(&token),
        vec![
            env_value(
                &run,
                0,
                Event::RunStarted {
                    agent_def_hash: agent,
                    input: json!({ "topic": "otters" }),
                    labels: None,
                    driven_by: None,
                },
            ),
            env_value(&run, 1, Event::NowObserved { now: ts() }),
            env_value(
                &run,
                2,
                Event::SleepStarted {
                    wake_at: datetime!(2026-07-10 13:00:00 UTC),
                },
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "parking the run: {body}");

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/resume", server.base),
        json!({}),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a client-driven run is refused, not dispatched on: {body}"
    );
    assert_eq!(body["error"]["code"], "client_driven_run");

    let run_id = RunId::from_uuid(Uuid::parse_str(&run).expect("run id"));

    // Nothing was appended past the park the refusal ran ahead of.
    let log = server
        .state
        .store()
        .read_log(run_id)
        .await
        .expect("log reads");
    assert_eq!(log.len(), 3, "the refusal recorded nothing: {log:?}");

    // And no server-side driver task attached to a run its own client drives.
    assert!(
        !server.state.is_run_active(run_id),
        "the refusal never reaches the code that spawns a drive"
    );
}

/// A client-driven server over `store`, holding one declared client-performed
/// tool so a run can be left mid-call.
///
/// It takes the store instead of minting one, which is what lets a test drop
/// the whole server and stand a second one over the same store. That is all a
/// `salvor serve` restart is from a run's point of view: the process's lease
/// registry goes, the recorded log stays.
async fn client_server_over(store: std::sync::Arc<dyn salvor_store::EventStore>) -> TestServer {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    Box::leak(Box::new(model));
    let mut client_tools = ClientToolRegistry::new();
    client_tools.declare(ClientToolDecl {
        name: "charge_card".into(),
        effect: Effect::Write,
        input_schema: json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": { "amount_cents": { "type": "integer" } }
        }),
        output_schema: Some(json!({
            "type": "object",
            "required": ["charge_id"],
            "properties": { "charge_id": { "type": "string" } }
        })),
        trust_completion: true,
        require_equal: Vec::new(),
    });
    let state = app_state(store, factory).with_client_tools(std::sync::Arc::new(client_tools));
    TestServer::spawn(state).await
}

/// Stops `server` and gives up everything it held in memory. The store it was
/// serving is the caller's and survives, so what comes next reads a log with
/// no process behind it, exactly as a restarted server does.
fn restart(server: TestServer) {
    server.state.abort_all();
    server.handle.abort();
    drop(server);
}

/// A `POST` carrying a run's drive token.
async fn post_driven(
    client: &reqwest::Client,
    url: &str,
    body: Value,
    token: &str,
) -> (StatusCode, Value) {
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", token)
        .body(body.to_string())
        .send()
        .await
        .expect("request sends");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A restart does not strand a run its client is still driving. The lease
/// registry that used to be the only thing saying "this run is client-driven"
/// died with the first process; the `driven_by` the server stamped on the
/// run's `RunStarted` did not, so the second process re-opens the run from its
/// log, hands out a fresh lease, and the client carries on from where it was:
/// here, with an unanswered tool intent still open.
#[tokio::test]
async fn a_restart_re_opens_a_client_driven_run_from_its_log() {
    let store = memory_store();
    let first = client_server_over(store.clone()).await;
    let client = reqwest::Client::new();
    let (run, first_token) = open_run(&client, &first.base).await;
    let run_id = RunId::from_uuid(Uuid::parse_str(&run).expect("run id"));

    let (status, body) = append(
        &client,
        &first.base,
        &run,
        Some(&first_token),
        vec![
            env_value(&run, 0, run_started()),
            env_value(&run, 1, Event::NowObserved { now: ts() }),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the head and a clock reading: {body}"
    );

    // A call the client had asked for but not yet reported back on when the
    // server went down: the state a resuming client most needs returned to it.
    let (status, body) = post_driven(
        &client,
        &format!("{}/v1/client-runs/{run}/client-tool-intent", first.base),
        json!({ "seq": 2, "tool": "charge_card", "input": { "amount_cents": 250 } }),
        &first_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the intent is recorded: {body}");

    restart(first);
    let second = client_server_over(store.clone()).await;
    assert!(
        !second.state.is_client_run(run_id),
        "the fresh process remembers no lease for the run; only its log speaks"
    );

    // Re-opening is an adoption: an existing run, so `200`, not `201`.
    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", second.base),
        json!({ "run_id": run }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the run is re-opened: {body}");
    let kinds: Vec<&str> = body["log"]
        .as_array()
        .expect("the recorded log comes back")
        .iter()
        .map(|envelope| envelope["event"]["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["RunStarted", "NowObserved", "ToolCallRequested"],
        "the recorded state, intent included: {body}"
    );

    let token = body["drive_token"].as_str().expect("a lease").to_owned();
    assert_ne!(
        token, first_token,
        "adoption mints a fresh lease, as any re-open does"
    );
    assert!(
        second.state.is_client_run(run_id),
        "and the run is a client-driven run of this process from here on"
    );

    // The dead process's token is not the current lease.
    let (status, _) = append(
        &client,
        &second.base,
        &run,
        Some(&first_token),
        vec![env_value(&run, 3, Event::RandomObserved { value: 4 })],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the lease the first process minted is superseded"
    );

    // The client finishes the call it had open, and then the run.
    let (status, body) = post_driven(
        &client,
        &format!(
            "{}/v1/client-runs/{run}/client-tool-completion",
            second.base
        ),
        json!({ "seq": 2, "output": { "charge_id": "ch_1" } }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the open call is answered: {body}");

    let (status, body) = append(
        &client,
        &second.base,
        &run,
        Some(&token),
        vec![env_value(
            &run,
            4,
            Event::RunCompleted {
                output: json!({ "charged": true }),
            },
        )],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the run finishes: {body}");

    let (status, body) = get_json(&client, &format!("{}/v1/runs/{run}", second.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["status"]["state"], "completed",
        "the run a restart interrupted ran to completion: {body}"
    );
}

/// Adoption reads the marker, not merely "this id has history". A run this
/// server drove itself carries no `driven_by`, so a restart leaves it exactly
/// as refused as it was before: the two modes still cannot collide over one
/// store.
#[tokio::test]
async fn a_restart_still_refuses_a_server_driven_run_id() {
    let store = memory_store();
    let first = client_server_over(store.clone()).await;
    let client = reqwest::Client::new();

    // A server-driven run's head: the same event, without the marker this
    // server stamps only under a client's lease.
    let foreign = RunId::new();
    store
        .append(&EventEnvelope::new(
            foreign,
            SequenceNumber::new(0),
            ts(),
            run_started(),
        ))
        .await
        .expect("seed a server-driven run");

    restart(first);
    let second = client_server_over(store.clone()).await;

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", second.base),
        json!({ "run_id": foreign.as_uuid().to_string() }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "still refused: {body}");
    assert_eq!(body["error"]["code"], "run_exists");
}

/// The other two surfaces that must never become a second writer read the same
/// evidence, and they read it before any re-open: a client-driven run that a
/// restart has left with no lease anywhere is still refused by `resume` and
/// still left asleep by the wake sweeper. Were either to consult only the
/// in-memory registry, the first restart would put this server back to racing
/// the client for the run's log positions.
#[tokio::test]
async fn after_a_restart_resume_refuses_and_the_sweeper_skips_a_client_driven_run() {
    let store = memory_store();
    let first = client_server_over(store.clone()).await;
    let client = reqwest::Client::new();
    let (run, token) = open_run(&client, &first.base).await;
    let run_id = RunId::from_uuid(Uuid::parse_str(&run).expect("run id"));

    // The client parks its own run on a timer that is already overdue against
    // the server's clock, so the sweeper genuinely selects it.
    let (status, body) = append(
        &client,
        &first.base,
        &run,
        Some(&token),
        vec![
            env_value(&run, 0, run_started()),
            env_value(&run, 1, Event::NowObserved { now: ts() }),
            env_value(
                &run,
                2,
                Event::SleepStarted {
                    wake_at: datetime!(2026-07-10 11:00:00 UTC),
                },
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the run parks on its timer: {body}");

    restart(first);
    let second = client_server_over(store.clone()).await;
    assert!(
        !second.state.is_client_run(run_id),
        "no lease survived; the log is the only evidence either surface has"
    );

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{run}/resume", second.base),
        json!({}),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "resume still refuses a run whose client drives it: {body}"
    );
    assert_eq!(body["error"]["code"], "client_driven_run");

    assert!(
        salvor_server::sweep(&second.state).await.is_empty(),
        "and the sweeper still leaves the due timer to the client"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads").len(),
        3,
        "neither surface wrote anything to the run"
    );
    assert!(
        !second.state.is_run_active(run_id),
        "and no driver task was spawned for it"
    );
}
