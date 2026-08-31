//! The client-performed model call over real HTTP: the intent salvor records
//! for a call it does not make, the completion the client reports, the replay
//! that saves paying the provider twice, and the refusals that keep an
//! unwitnessed call from being confused with a witnessed one.
//!
//! Nothing here calls a provider. That is the point of the feature: a
//! LangChain-style middleware holds its own key and its own model
//! configuration, calls the provider in its own process, and hands salvor the
//! hash and the answer so a resume replays the answer instead of paying for it
//! again. The one exception is
//! [`the_server_will_not_perform_a_client_performed_intent`], which wires a real
//! (scripted) executor purely to prove it is never reached.

mod common;

use common::{
    CountBehavior, TestServer, agent_factory, app_state, counter, get_json, memory_store,
    model_executor, post_json, text_response,
};
use reqwest::StatusCode;
use salvor_core::{Effect, Event, EventEnvelope, Performer, RunId, SequenceNumber};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A fixed recorded timestamp for hand-built envelopes.
fn ts() -> time::OffsetDateTime {
    datetime!(2026-07-11 12:00:00 UTC)
}

/// The client's own hash of the request it performed. Salvor never sees the
/// request, so this is a claim, and any stable string a client hashes with
/// would do; the `sha256:` shape is what every other hash in a log wears.
const REQUEST_HASH: &str = "sha256:client-request-1";

/// A server with no model executor wired at all: the client performs the call,
/// so this server has nothing to perform it with, and the tests still pass.
async fn client_model_server() -> TestServer {
    let mock = MockServer::start().await;
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    // The factory holds only the uri and is never invoked here; keep the mock
    // alive for the test's duration.
    Box::leak(Box::new(mock));
    TestServer::spawn(app_state(memory_store(), factory)).await
}

/// Opens a fresh client-driven run and appends its `RunStarted`, leaving the
/// log ready for an intent at seq 1.
async fn started_run(
    client: &reqwest::Client,
    base: &str,
    record_prompts: bool,
) -> (String, String) {
    let (status, body) = post_json(
        client,
        &format!("{base}/v1/client-runs"),
        json!({ "record_prompts": record_prompts }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    let token = body["drive_token"].as_str().expect("token").to_owned();

    let started = env_value(
        &run,
        0,
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({ "topic": "otters" }),
            labels: None,
            driven_by: None,
            caller: None,
        },
    );
    let (status, body) = post_driven(
        client,
        &format!("{base}/v1/client-runs/{run}/events"),
        json!({ "events": [started] }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "RunStarted append: {body}");
    (run, token)
}

/// A `POST` carrying the drive token.
async fn post_driven(
    client: &reqwest::Client,
    url: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    if let Some(token) = token {
        request = request.header("x-drive-token", token);
    }
    let response = request.send().await.expect("request sends");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Opens a client-performed model intent.
async fn intent(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    post_driven(
        client,
        &format!("{base}/v1/client-runs/{run}/client-model-intent"),
        body,
        token,
    )
    .await
}

/// Reports a client-performed model call's completion.
async fn completion(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    post_driven(
        client,
        &format!("{base}/v1/client-runs/{run}/client-model-completion"),
        body,
        token,
    )
    .await
}

/// Reads a client-driven run's log back as decoded envelopes.
async fn read_log(client: &reqwest::Client, base: &str, run: &str) -> Vec<EventEnvelope> {
    let (status, body) = get_json(client, &format!("{base}/v1/client-runs/{run}/log"), None).await;
    assert_eq!(status, StatusCode::OK, "log read: {body}");
    serde_json::from_value(body["log"].clone()).expect("decode log")
}

/// The wire JSON of an envelope for `run` at `seq`.
fn env_value(run: &str, seq: u64, event: Event) -> Value {
    let run_id = RunId::from_uuid(Uuid::parse_str(run).expect("run id"));
    let envelope = EventEnvelope::new(run_id, SequenceNumber::new(seq), ts(), event);
    serde_json::to_value(envelope).expect("serialize envelope")
}

/// The response body a client reports for its own model call.
fn response_body() -> Value {
    json!({ "content": [{ "type": "text", "text": "the plan" }] })
}

/// Test 1: the whole flow. The client opens an intent, calls the provider in
/// its own process, and reports back; the log ends up holding a
/// `ModelCallRequested` that says the CLIENT performed it, followed by its
/// completion, and the derived state moves the way a server-performed call's
/// would: awaiting-model while the intent is open, tokens counted once it is
/// closed.
#[tokio::test]
async fn a_client_performed_model_call_is_recorded_end_to_end() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "request_hash": REQUEST_HASH }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");
    assert_eq!(opened["seq"], json!(1));
    assert_eq!(
        opened["settled"],
        json!(false),
        "a fresh intent is never already settled"
    );
    assert!(
        opened.get("response").is_none(),
        "an unsettled intent carries no completion: {opened}"
    );

    // While the intent is open the run folds exactly as a server-performed
    // model call's does.
    let (_, state) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(
        state["status"]["state"], "awaiting_model",
        "an open client-performed intent is a pending model call: {state}"
    );
    assert_eq!(state["pending"]["kind"], "model");
    assert_eq!(state["pending"]["request_hash"], json!(REQUEST_HASH));

    // The client calls the provider here, in its own process, with its own key.

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "response": response_body(),
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");
    assert_eq!(done, json!({ "seq": 1, "completed": true }));

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, completion");
    let Event::ModelCallRequested {
        seq,
        request_hash,
        request_body,
        performed_by,
    } = &log[1].event
    else {
        panic!("seq 1 holds the model intent, got {:?}", log[1].event);
    };
    assert_eq!(seq.get(), 1);
    assert_eq!(
        request_hash, REQUEST_HASH,
        "the client's own hash, verbatim"
    );
    assert_eq!(
        request_body.as_ref(),
        None,
        "no body recorded with prompt recording off"
    );
    assert_eq!(
        *performed_by,
        Some(Performer::Client),
        "the log says who performed it"
    );
    assert!(
        matches!(&log[2].event, Event::ModelCallCompleted { seq, response, usage }
            if seq.get() == 1
                && response == &response_body()
                && usage.input_tokens == 10
                && usage.output_tokens == 5),
        "the completion correlates to the intent, got {:?}",
        log[2].event
    );

    // And the fold counts the tokens, so a budget holds a client-performed call
    // to the same account a server-performed one is held to.
    let (_, state) = get_json(&client, &format!("{}/v1/runs/{run}", server.base), None).await;
    assert_eq!(state["status"]["state"], "running", "{state}");
    assert_eq!(state["usage"]["input_tokens"], 10);
    assert_eq!(state["usage"]["output_tokens"], 5);
    assert_eq!(state["pending"], Value::Null, "nothing is outstanding");
}

/// Test 2: the replay that is the whole point. Re-posting the intent at the
/// same position with the same hash, after the completion is recorded, answers
/// `settled` with the recorded response and usage and writes nothing, so a
/// middleware short-circuits instead of paying the provider a second time.
#[tokio::test]
async fn a_settled_intent_replays_with_its_recorded_completion() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let body = json!({ "seq": 1, "request_hash": REQUEST_HASH });
    let (status, _) = intent(&client, &server.base, &run, Some(&token), body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "response": response_body(),
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, replayed) = intent(&client, &server.base, &run, Some(&token), body).await;
    assert_eq!(status, StatusCode::OK, "re-post: {replayed}");
    assert_eq!(replayed["settled"], json!(true), "{replayed}");
    assert_eq!(
        replayed["response"],
        response_body(),
        "the recorded answer comes back, so nobody pays twice"
    );
    assert_eq!(replayed["usage"]["input_tokens"], 10);
    assert_eq!(replayed["usage"]["output_tokens"], 5);

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "the re-post wrote nothing: {log:?}");
}

/// Test 3: a dangling intent (the middleware died between opening it and
/// reporting back) re-posts as an unsettled replay. Nothing is written, and the
/// answer says the call still has to be made.
#[tokio::test]
async fn a_dangling_intent_replays_unsettled() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let body = json!({ "seq": 1, "request_hash": REQUEST_HASH });
    let (status, _) = intent(&client, &server.base, &run, Some(&token), body.clone()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, replayed) = intent(&client, &server.base, &run, Some(&token), body).await;
    assert_eq!(status, StatusCode::OK, "re-post: {replayed}");
    assert_eq!(
        replayed["settled"],
        json!(false),
        "the call is still outstanding: {replayed}"
    );
    assert!(replayed.get("response").is_none(), "{replayed}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the re-post wrote nothing: {log:?}");

    // And the completion still lands afterward, so a re-post is a safe retry
    // rather than a state change.
    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "response": response_body(),
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion after re-post: {done}");
}

/// Test 4: a different hash at a recorded position is divergence, and nothing
/// is written. The hash is the client's claim, but it is a key into this run's
/// own log, so an inconsistent client diverges against its own history.
#[tokio::test]
async fn a_different_hash_at_a_recorded_position_diverges() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let (status, _) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "request_hash": REQUEST_HASH }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, error) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "request_hash": "sha256:a-different-request" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "divergence", "{error}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "divergence wrote nothing: {log:?}");
}

/// Test 5: a completion with no open client-performed intent is refused. Two
/// shapes of "no open intent" are checked: a log that ends at something else
/// entirely, and one that ends at an intent for a different position.
#[tokio::test]
async fn a_completion_with_no_open_intent_is_refused() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    // The log ends at the RunStarted: there is no model call outstanding.
    let (status, error) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "response": response_body(),
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "divergence", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("does not end at a model intent"),
        "{error}"
    );

    // Now open an intent at seq 1 and name the wrong position on the completion.
    let (status, _) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "request_hash": REQUEST_HASH }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, error) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 7,
            "response": response_body(),
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "divergence", "{error}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "neither refusal wrote anything: {log:?}");
}

/// Test 6: a client may not close a call this SERVER performed. The
/// server-performed model step records its own completion from the response it
/// saw, so a client completion there would overwrite a witnessed fact with a
/// claim.
#[tokio::test]
async fn a_client_cannot_complete_a_server_performed_call() {
    // A provider that never answers a request, so the server's own step leaves a
    // dangling SERVER-performed intent behind, which is what this test needs.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock)
        .await;
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let state = app_state(memory_store(), factory).with_model_executor(model_executor(&mock.uri()));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    // The server-performed step fails at the provider, leaving its write-ahead
    // intent as the log's last event.
    let (status, _) = post_driven(
        &client,
        &format!("{}/v1/client-runs/{run}/model-step", server.base),
        json!({ "seq": 1, "request": { "model": "m", "messages": [] } }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "the provider failed");
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the write-ahead intent is dangling: {log:?}");

    let (status, error) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "response": response_body(),
            "usage": { "input_tokens": 99, "output_tokens": 99 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(
        error["error"]["code"], "client_completion_refused",
        "{error}"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the refusal wrote nothing: {log:?}");
}

/// Test 7: the mirror of test 6. This server will not perform, or answer, a
/// call the log says the CLIENT performed: re-issuing it would let the server
/// witness a response for an intent attributed to the client, smearing the one
/// distinction `performed_by` exists to keep.
#[tokio::test]
async fn the_server_will_not_perform_a_client_performed_intent() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("the plan", 10, 5)))
        .mount(&mock)
        .await;
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let state = app_state(memory_store(), factory).with_model_executor(model_executor(&mock.uri()));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let (status, _) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "request_hash": REQUEST_HASH }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, error) = post_driven(
        &client,
        &format!("{}/v1/client-runs/{run}/model-step", server.base),
        json!({ "seq": 1, "request": { "model": "m", "messages": [] } }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "divergence", "{error}");
    assert_eq!(
        mock.received_requests()
            .await
            .expect("requests recorded")
            .iter()
            .filter(|request| request.url.path() == "/v1/messages")
            .count(),
        0,
        "the provider was never called"
    );
}

/// Test 8: both endpoints sit under the drive-token lease, exactly as the
/// client-tool pair does. A missing token is `401`; a superseded one is `403`.
#[tokio::test]
async fn both_endpoints_require_the_current_lease() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let body = json!({ "seq": 1, "request_hash": REQUEST_HASH });
    let (status, error) = intent(&client, &server.base, &run, None, body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{error}");
    assert_eq!(error["error"]["code"], "missing_drive_token", "{error}");

    let (status, error) = intent(&client, &server.base, &run, Some("dt_wrong"), body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "invalid_drive_token", "{error}");

    let (status, _) = intent(&client, &server.base, &run, Some(&token), body).await;
    assert_eq!(status, StatusCode::OK);

    let done = json!({
        "seq": 1,
        "response": response_body(),
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    let (status, error) = completion(&client, &server.base, &run, None, done.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{error}");
    let (status, error) = completion(&client, &server.base, &run, Some("dt_wrong"), done).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "no unauthorized write landed: {log:?}");
}

/// Test 9: prompt recording is the run's, not the request's. A body sent to a
/// run opened without `record_prompts` is dropped and never written; the same
/// body on a recording run rides on the intent. This is the same rule the
/// server-performed step reads off the lease.
#[tokio::test]
async fn record_prompts_governs_the_recorded_request_body() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let body = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });

    let (run_off, token_off) = started_run(&client, &server.base, false).await;
    let (status, _) = intent(
        &client,
        &server.base,
        &run_off,
        Some(&token_off),
        json!({ "seq": 1, "request_hash": REQUEST_HASH, "request_body": body }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let log = read_log(&client, &server.base, &run_off).await;
    let Event::ModelCallRequested { request_body, .. } = &log[1].event else {
        panic!("expected the model intent, got {:?}", log[1].event);
    };
    assert_eq!(
        request_body.as_ref(),
        None,
        "recording off drops the body the client sent"
    );

    let (run_on, token_on) = started_run(&client, &server.base, true).await;
    let (status, _) = intent(
        &client,
        &server.base,
        &run_on,
        Some(&token_on),
        json!({ "seq": 1, "request_hash": REQUEST_HASH, "request_body": body }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let log = read_log(&client, &server.base, &run_on).await;
    let Event::ModelCallRequested { request_body, .. } = &log[1].event else {
        panic!("expected the model intent, got {:?}", log[1].event);
    };
    assert_eq!(
        request_body.as_ref(),
        Some(&body),
        "recording on stores the body verbatim"
    );
}

/// Test 10: the generic append still refuses a model event outright, so the
/// only way a client-performed model call reaches the log is through the pair
/// of endpoints that stamp `performed_by` and check the correlation.
#[tokio::test]
async fn the_generic_append_still_refuses_a_model_event() {
    let server = client_model_server().await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base, false).await;

    let forged = env_value(
        &run,
        1,
        Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: REQUEST_HASH.into(),
            request_body: None,
            performed_by: None,
        },
    );
    let (status, error) = post_driven(
        &client,
        &format!("{}/v1/client-runs/{run}/events", server.base),
        json!({ "events": [forged] }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["code"], "unsupported_event_kind", "{error}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 1, "nothing was appended: {log:?}");
}
