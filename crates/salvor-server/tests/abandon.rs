//! The abandon endpoint over real HTTP: retire a non-terminal run, and the
//! refusals. Abandonment is an operator action over the store (it drives no
//! agent and needs no lease), so these tests seed the run's log directly through
//! the shared store and then drive the endpoint, rather than standing up the
//! model/tool machinery a real drive would need.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::{CountBehavior, TestServer, agent_factory, app_state, memory_store, post_json};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber};
use salvor_server::AgentFactory;
use serde_json::{Value, json};
use time::macros::datetime;

/// An envelope for `run` at `seq`, with the fixed test clock.
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
        input: json!({"topic": "otters"}),
        labels: None,
    }
}

/// A factory the abandon tests never call (abandon drives no agent), but
/// `app_state` requires one.
fn unused_factory() -> AgentFactory {
    agent_factory(
        "http://127.0.0.1:1".into(),
        "noop",
        Effect::Read,
        CountBehavior::Record,
        Arc::new(AtomicUsize::new(0)),
    )
}

/// POST `/v1/runs/{run}/abandon`.
async fn abandon(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    body: Value,
) -> (StatusCode, Value) {
    post_json(client, &format!("{base}/v1/runs/{run}/abandon"), body, None).await
}

/// A running (non-terminal) run is retired: the receipt carries the appended seq
/// and the re-derived `abandoned` status with its reason, and a second abandon is
/// refused because the run is now terminal.
#[tokio::test]
async fn abandon_retires_a_running_run_then_refuses_a_second() {
    let store = memory_store();
    let run = RunId::new();
    store
        .append(&env(run, 0, started()))
        .await
        .expect("seed RunStarted");

    let server = TestServer::spawn(app_state(store, unused_factory())).await;
    let client = reqwest::Client::new();
    let id = run.as_uuid().to_string();

    let (status, body) = abandon(
        &client,
        &server.base,
        &id,
        json!({"reason": "husk is dead forever"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "abandon: {body}");
    assert_eq!(body["abandoned"], json!(true));
    assert_eq!(body["appended_seq"], json!(1));
    assert_eq!(body["status"]["state"], "abandoned");
    assert_eq!(body["status"]["reason"], "husk is dead forever");
    // A bare abandonment records no unresolved write.
    assert!(
        body["status"].get("unresolved_write").is_none(),
        "a run with no dangling write must not carry unresolved_write: {body}"
    );

    // Already terminal: the second abandon is refused with wrong_state (409).
    let (status2, body2) = abandon(&client, &server.base, &id, json!({})).await;
    assert_eq!(status2, StatusCode::CONFLICT, "second abandon: {body2}");
    assert_eq!(body2["error"]["code"], "wrong_state");
}

/// Abandoning an unknown run id is a 404.
#[tokio::test]
async fn abandon_unknown_run_is_404() {
    let server = TestServer::spawn(app_state(memory_store(), unused_factory())).await;
    let client = reqwest::Client::new();
    let id = RunId::new().as_uuid().to_string();

    let (status, body) = abandon(&client, &server.base, &id, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_run");
}

/// Abandoning a run parked at a dangling write is ALLOWED, not refused: the
/// endpoint records the outstanding write on the terminal event, so the
/// abandoned status carries `unresolved_write` and never claims the write
/// settled.
#[tokio::test]
async fn abandon_needs_reconciliation_records_the_unresolved_write() {
    let store = memory_store();
    let run = RunId::new();
    store
        .append(&env(run, 0, started()))
        .await
        .expect("seed head");
    store
        .append(&env(
            run,
            1,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "charge".into(),
                input: json!({"amount": 10}),
                effect: Effect::Write,
                idempotency_key: None,
            },
        ))
        .await
        .expect("seed dangling write");

    let server = TestServer::spawn(app_state(store, unused_factory())).await;
    let client = reqwest::Client::new();
    let id = run.as_uuid().to_string();

    let (status, body) = abandon(&client, &server.base, &id, json!({})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "abandon needs_reconciliation: {body}"
    );
    assert_eq!(body["status"]["state"], "abandoned");
    assert_eq!(
        body["status"]["unresolved_write"],
        json!({ "seq": 1, "tool": "charge" }),
        "the abandonment must record the dangling write as evidence: {body}"
    );
    // No reason was given, so no reason key rides on the status.
    assert!(
        body["status"].get("reason").is_none(),
        "a reasonless abandonment must omit reason: {body}"
    );
}
