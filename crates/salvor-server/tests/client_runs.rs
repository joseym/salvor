//! The client-driven run surface over real HTTP: open and resume, read the
//! log, and the generic guarded append with its drive-token lease and its
//! idempotency, divergence, and legality rules.

mod common;

use common::{
    CountBehavior, ScriptedModel, TestServer, agent_factory, app_state, counter, fixed_clock,
    get_json, memory_store, post_json,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, ReplayCursor, RunId, SequenceNumber};
use serde_json::{Value, json};
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
