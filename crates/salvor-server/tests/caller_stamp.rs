//! The caller name the server stamps onto the events a person acts through.
//!
//! `auth_identity.rs` covers the layer that attaches the name to a verified
//! request. This covers what the endpoints then do with it: `caller` on the
//! `RunStarted`, `Resumed`, and `RunAbandoned` a client-driven run appends,
//! `caller` on an abandonment, and `settled_caller` on the completion
//! `resolve` records. Two rules are checked on every path: the name comes from
//! the token, so a submitted one is discarded, and a server with no bearer
//! configured records no name at all.
//!
//! The runs here are seeded or client-driven rather than agent-driven, for the
//! same reason `abandon.rs` seeds: none of these endpoints builds an agent, and
//! the stamp is what is under test.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::{
    CountBehavior, ScriptedModel, TestServer, agent_factory, app_state, counter, get_json,
    memory_store, post_json,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber};
use salvor_server::{AgentFactory, TokenStore};
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

/// The token the named-token servers below are configured with.
const CI_TOKEN: &str = "sv_ci_token_value";

/// A factory these tests never call: no endpoint under test builds an agent.
fn unused_factory() -> AgentFactory {
    agent_factory(
        "http://127.0.0.1:1".into(),
        "noop",
        Effect::Read,
        CountBehavior::Record,
        Arc::new(AtomicUsize::new(0)),
    )
}

/// Writes a token file at mode 0600 declaring `ci` by its token's SHA-256.
fn token_file(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("tokens.toml");
    let hash: String = salvor_server::tokens::digest(CI_TOKEN)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut file = std::fs::File::create(&path).expect("create the token file");
    write!(file, "[tokens.ci]\nhash = \"{hash}\"\n").expect("write it");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    path
}

/// An envelope for `run` at `seq`, with a fixed clock.
fn env(run: RunId, seq: u64, event: Event) -> EventEnvelope {
    EventEnvelope::new(
        run,
        SequenceNumber::new(seq),
        datetime!(2026-07-10 12:00:00 UTC),
        event,
    )
}

fn started() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:agent".into(),
        input: json!({ "topic": "otters" }),
        labels: None,
        driven_by: None,
        caller: None,
    }
}

/// The whole recorded log of `run`, read back through the store.
async fn log_of(state: &salvor_server::AppState, run: RunId) -> Vec<EventEnvelope> {
    state.store().read_log(run).await.expect("read the log")
}

/// The `caller` on the log's last event, whatever kind it is.
fn caller_of(event: &Event) -> Option<&str> {
    match event {
        Event::RunStarted { caller, .. }
        | Event::Resumed { caller, .. }
        | Event::RunAbandoned { caller, .. } => caller.as_deref(),
        _ => None,
    }
}

#[tokio::test]
async fn an_abandonment_records_the_token_that_asked_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(memory_store(), unused_factory())
        .with_token_file(TokenStore::load(&token_file(dir.path())).expect("load the token file"));
    let run = RunId::from_uuid(Uuid::new_v4());
    state
        .store()
        .append(&env(run, 0, started()))
        .await
        .expect("seed the run");
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{}/abandon", server.base, run.as_uuid()),
        json!({ "reason": "husk is dead forever" }),
        Some(CI_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abandon: {body}");

    let log = log_of(&server.state, run).await;
    let last = &log.last().expect("a terminal event").event;
    assert!(matches!(last, Event::RunAbandoned { .. }), "{last:?}");
    assert_eq!(
        caller_of(last),
        Some("ci"),
        "the abandonment names the token it came in under"
    );
}

#[tokio::test]
async fn a_pass_through_server_records_no_name() {
    let state = app_state(memory_store(), unused_factory());
    let run = RunId::from_uuid(Uuid::new_v4());
    state
        .store()
        .append(&env(run, 0, started()))
        .await
        .expect("seed the run");
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{}/abandon", server.base, run.as_uuid()),
        json!({}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abandon: {body}");

    let log = log_of(&server.state, run).await;
    let last = &log.last().expect("a terminal event").event;
    assert_eq!(
        caller_of(last),
        None,
        "with no bearer configured there is no verified name to record"
    );
}

#[tokio::test]
async fn a_hand_recorded_completion_names_the_token_that_settled_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = app_state(memory_store(), unused_factory())
        .with_token_file(TokenStore::load(&token_file(dir.path())).expect("load the token file"));
    let run = RunId::from_uuid(Uuid::new_v4());
    // A recorded write intent with no completion: the one state resolve serves.
    for envelope in [
        env(run, 0, started()),
        env(
            run,
            1,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "charge_card".into(),
                input: json!({ "amount_cents": 500 }),
                effect: Effect::Write,
                idempotency_key: None,
                performed_by: None,
            },
        ),
    ] {
        state
            .store()
            .append(&envelope)
            .await
            .expect("seed the dangling write");
    }
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/runs/{}/resolve", server.base, run.as_uuid()),
        json!({ "output": { "charge_id": "po_1" } }),
        Some(CI_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");

    let log = log_of(&server.state, run).await;
    let Event::ToolCallCompleted {
        settled_by,
        settled_caller,
        ..
    } = &log.last().expect("a completion").event
    else {
        panic!("expected a ToolCallCompleted");
    };
    assert_eq!(
        *settled_by,
        Some(salvor_core::SettledBy::Operator),
        "the mechanism: a person recorded this"
    );
    assert_eq!(
        settled_caller.as_deref(),
        Some("ci"),
        "and the person: the token the request came in under"
    );
}

#[tokio::test]
async fn a_client_run_carries_the_verified_name_and_never_a_submitted_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    Box::leak(Box::new(model));
    let state = app_state(memory_store(), factory)
        .with_token_file(TokenStore::load(&token_file(dir.path())).expect("load the token file"));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", server.base),
        json!({}),
        Some(CI_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    let drive_token = body["drive_token"]
        .as_str()
        .expect("drive token")
        .to_owned();
    let run_id = RunId::from_uuid(Uuid::parse_str(&run).expect("run id parses"));

    // The client names itself `admin` on every event it submits. Every one of
    // them comes back named `ci`: the name is the server's to write.
    let forged = |seq: u64, event: Event| {
        serde_json::to_value(EventEnvelope::new(
            run_id,
            SequenceNumber::new(seq),
            datetime!(2026-07-11 12:00:00 UTC),
            event,
        ))
        .expect("serialize the envelope")
    };
    let events = vec![
        forged(
            0,
            Event::RunStarted {
                agent_def_hash: "sha256:agent".into(),
                input: json!({ "topic": "otters" }),
                labels: None,
                driven_by: None,
                caller: Some("admin".into()),
            },
        ),
        forged(
            1,
            Event::Suspended {
                reason: "awaiting approval".into(),
                input_schema: json!({ "type": "object" }),
                kind: None,
            },
        ),
        forged(
            2,
            Event::Resumed {
                input: json!({ "approved": true }),
                caller: Some("admin".into()),
            },
        ),
    ];
    let response = client
        .post(format!("{}/v1/client-runs/{run}/events", server.base))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", &drive_token)
        .bearer_auth(CI_TOKEN)
        .body(json!({ "events": events }).to_string())
        .send()
        .await
        .expect("append sends");
    assert_eq!(response.status(), StatusCode::OK, "append");

    let log = log_of(&server.state, run_id).await;
    assert_eq!(
        caller_of(&log[0].event),
        Some("ci"),
        "the run's head names the token, not the submitted `admin`"
    );
    assert_eq!(
        caller_of(&log[2].event),
        Some("ci"),
        "and so does the resume"
    );

    // The run JSON carries the name folded off that head.
    let (status, body) = get_json(
        &client,
        &format!("{}/v1/runs/{run}", server.base),
        Some(CI_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get run: {body}");
    assert_eq!(body["caller"], json!("ci"));

    let (status, body) =
        get_json(&client, &format!("{}/v1/runs", server.base), Some(CI_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "list runs: {body}");
    let entry = body["runs"]
        .as_array()
        .expect("a runs array")
        .iter()
        .find(|entry| entry["run"] == json!(run))
        .expect("the run is listed")
        .clone();
    assert_eq!(entry["caller"], json!("ci"));
}

#[tokio::test]
async fn a_client_run_on_a_pass_through_server_clears_a_submitted_name() {
    let model = ScriptedModel::mount(vec![]).await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    Box::leak(Box::new(model));
    let server = TestServer::spawn(app_state(memory_store(), factory)).await;
    let client = reqwest::Client::new();

    let (status, body) = post_json(
        &client,
        &format!("{}/v1/client-runs", server.base),
        json!({}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    let drive_token = body["drive_token"]
        .as_str()
        .expect("drive token")
        .to_owned();
    let run_id = RunId::from_uuid(Uuid::parse_str(&run).expect("run id parses"));

    let envelope = serde_json::to_value(EventEnvelope::new(
        run_id,
        SequenceNumber::new(0),
        datetime!(2026-07-11 12:00:00 UTC),
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({ "topic": "otters" }),
            labels: None,
            driven_by: None,
            caller: Some("admin".into()),
        },
    ))
    .expect("serialize the envelope");
    let response = client
        .post(format!("{}/v1/client-runs/{run}/events", server.base))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("x-drive-token", &drive_token)
        .body(json!({ "events": [envelope] }).to_string())
        .send()
        .await
        .expect("append sends");
    assert_eq!(response.status(), StatusCode::OK, "append");

    let log = log_of(&server.state, run_id).await;
    assert_eq!(
        caller_of(&log[0].event),
        None,
        "a name this server never verified is worse than no name, so it is cleared"
    );

    let (status, body) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(status, StatusCode::OK, "get run: {body}");
    assert_eq!(
        body.get("caller"),
        None,
        "a run that recorded no caller carries no key"
    );
}
