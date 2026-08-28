//! The client-performed tool call over real HTTP: the operator's declaration,
//! the intent the server records on the client's behalf, the derived
//! idempotency key, and every refusal that keeps an unwitnessed call honest.
//!
//! Nothing here dispatches a tool for the tool under test. That is the point of
//! the feature: the work happens in the client's process, and this server only
//! records that it was asked for and what came back. The one exception is the
//! server-performed intent in
//! [`a_client_cannot_complete_a_server_performed_call`], which is recorded by
//! the existing `tool-step` endpoint through a real (failing) registered tool,
//! so the test proves the refusal against a genuinely server-performed intent
//! rather than a hand-seeded one.

mod common;

use std::sync::Arc;

use common::{
    CountBehavior, TestServer, agent_factory, app_state, counter, get_json, memory_store,
};
use reqwest::StatusCode;
use salvor_core::{
    Effect, Event, EventEnvelope, Performer, RunId, RunStatus, SequenceNumber, derive_state,
};
use salvor_server::{ClientToolDecl, ClientToolRegistry};
use salvor_tools::{DynTool, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;
use wiremock::MockServer;

/// A fixed recorded timestamp for hand-built envelopes.
fn ts() -> time::OffsetDateTime {
    datetime!(2026-07-11 12:00:00 UTC)
}

/// A registered tool that always fails, used only to leave a dangling
/// SERVER-performed intent behind through the ordinary `tool-step` endpoint.
struct FailingTool {
    name: String,
    effect: Effect,
}

#[async_trait::async_trait]
impl DynTool for FailingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "a tool that always fails, to leave a dangling server intent"
    }
    fn effect(&self) -> Effect {
        self.effect
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        _input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        Err(ToolError::Handler {
            tool: self.name.clone(),
            source: salvor_tools::HandlerError::message("the provider was unreachable"),
        })
    }
}

/// The declaration the happy path uses: a write the client performs, with both
/// schemas declared and the client trusted to report its own result.
fn charge_card_decl() -> ClientToolDecl {
    decl(
        "charge_card",
        Effect::Write,
        json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": { "amount_cents": { "type": "integer" } }
        }),
        Some(json!({
            "type": "object",
            "required": ["charge_id"],
            "properties": { "charge_id": { "type": "string" } }
        })),
        true,
    )
}

/// Builds a declaration the way an operator's TOML file would.
fn decl(
    name: &str,
    effect: Effect,
    input_schema: Value,
    output_schema: Option<Value>,
    trust_completion: bool,
) -> ClientToolDecl {
    ClientToolDecl {
        name: name.to_owned(),
        effect,
        input_schema,
        output_schema,
        trust_completion,
        require_equal: Vec::new(),
    }
}

/// A server holding `decls` as client-performed tool declarations and `tools`
/// in its ordinary executable registry.
async fn client_tool_server(
    decls: Vec<ClientToolDecl>,
    tools: Vec<Arc<dyn DynTool>>,
) -> TestServer {
    let mock = MockServer::start().await;
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let mut client_tools = ClientToolRegistry::new();
    for decl in decls {
        client_tools.declare(decl);
    }
    let mut registry = salvor_server::ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    let state = app_state(memory_store(), factory)
        .with_tool_registry(Arc::new(registry))
        .with_client_tools(Arc::new(client_tools));
    TestServer::spawn(state).await
}

/// Opens a fresh client-driven run and appends its `RunStarted`, leaving the
/// log ready for an intent at seq 1.
async fn started_run(client: &reqwest::Client, base: &str) -> (String, String) {
    let (status, body) = post(client, &format!("{base}/v1/client-runs"), json!({}), None).await;
    assert_eq!(status, StatusCode::CREATED, "open: {body}");
    let run = body["run"].as_str().expect("run id").to_owned();
    let token = body["drive_token"].as_str().expect("token").to_owned();

    let started = env_value(
        &run,
        0,
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({ "invoice": "INV-1" }),
            labels: None,
            driven_by: None,
        },
    );
    let (status, body) = post(
        client,
        &format!("{base}/v1/client-runs/{run}/events"),
        json!({ "events": [started] }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "RunStarted append: {body}");
    (run, token)
}

/// A `POST` with an optional drive token, decoding the body as JSON.
async fn post(
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

/// Opens a client-performed tool intent.
async fn intent(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    post(
        client,
        &format!("{base}/v1/client-runs/{run}/client-tool-intent"),
        body,
        token,
    )
    .await
}

/// Reports a client-performed tool call's completion.
async fn completion(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    post(
        client,
        &format!("{base}/v1/client-runs/{run}/client-tool-completion"),
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

/// Test 1: the whole flow. The client opens an intent, performs the call in its
/// own process, and reports back; the log ends up holding a `ToolCallRequested`
/// that says the CLIENT performed it, carrying the operator-declared effect and
/// the server-derived key, followed by its completion.
#[tokio::test]
async fn a_client_performed_call_is_recorded_end_to_end() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");
    let key = opened["idempotency_key"]
        .as_str()
        .expect("the derived key comes back")
        .to_owned();
    assert!(key.starts_with("sha256:"), "a derived key, not a caller's");
    assert_eq!(opened["effect"], json!("write"), "the declared effect");

    // The client performs the call here, in its own process, under `key`.

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, completion");
    let Event::ToolCallRequested {
        tool,
        input,
        effect,
        idempotency_key,
        performed_by,
        ..
    } = &log[1].event
    else {
        panic!("seq 1 holds the tool intent, got {:?}", log[1].event);
    };
    assert_eq!(tool, "charge_card");
    assert_eq!(input, &json!({ "amount_cents": 2500 }));
    assert_eq!(*effect, Effect::Write, "the effect is the operator's");
    assert_eq!(idempotency_key.as_deref(), Some(key.as_str()));
    assert_eq!(
        *performed_by,
        Some(Performer::Client),
        "the log says who performed it"
    );
    assert!(
        matches!(&log[2].event, Event::ToolCallCompleted { seq, output, .. }
            if seq.get() == 1 && output == &json!({ "charge_id": "ch_9" })),
        "the completion correlates to the intent, got {:?}",
        log[2].event
    );
}

/// Test 2: the key is derived from the call's position, so it is stable for a
/// given (run, seq, tool) and different for a different position. That is what
/// makes an honest retry collapse at the provider and stops a second attempt
/// from minting itself a fresh key.
#[tokio::test]
async fn the_derived_key_is_stable_per_position() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let body = json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } });
    let (status, first) = intent(&client, &server.base, &run, Some(&token), body.clone()).await;
    assert_eq!(status, StatusCode::OK, "first intent: {first}");
    let (status, second) = intent(&client, &server.base, &run, Some(&token), body).await;
    assert_eq!(status, StatusCode::OK, "re-posting the intent: {second}");
    assert_eq!(
        first["idempotency_key"], second["idempotency_key"],
        "the same (run, seq, tool) derives the same key"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "the re-post wrote nothing"
    );

    // Settle the first call, then open a second one at a fresh position.
    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    let (status, later) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 3, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second intent: {later}");
    assert_ne!(
        first["idempotency_key"], later["idempotency_key"],
        "a different position derives a different key, even for the identical input"
    );
}

/// Test 3: a tool no operator declared is refused, and nothing is written. A
/// client cannot bring its own tool into existence by naming one.
#[tokio::test]
async fn an_undeclared_tool_is_refused() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, body) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "wire_transfer", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "undeclared tool: {body}");
    assert_eq!(body["error"]["code"], "unknown_tool");
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        1,
        "only RunStarted; no intent was written"
    );
}

/// Test 4: an input that fails the declared `input_schema` is refused BEFORE
/// anything is recorded, so a malformed call never becomes history.
#[tokio::test]
async fn a_bad_input_is_refused_and_records_nothing() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, body) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": "lots" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad input: {body}");
    assert_eq!(body["error"]["code"], "bad_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("input_schema")),
        "the refusal names the schema it failed: {body}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        1,
        "only RunStarted; the intent was never written"
    );
}

/// Test 5: an output that fails the declared `output_schema` is refused, no
/// completion is recorded, and the run is therefore left where an uncompleted
/// write always leaves it: needing reconciliation.
#[tokio::test]
async fn a_bad_output_is_refused_leaving_the_write_unsettled() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charged": true } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad output: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("output_schema")),
        "the refusal names the schema it failed: {body}"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the intent stands; no completion was written");
    assert_eq!(
        derive_state(&log).status,
        RunStatus::NeedsReconciliation,
        "an uncompleted write is exactly what needs reconciliation means"
    );
}

/// Test 6: `trust_completion = false` refuses the client's own completion, the
/// message points at the resolve endpoint, and the run lands in the state the
/// existing pure fold already reports for an unsettled write. Nothing in
/// `derive_state` knows about declarations, and nothing needed to.
#[tokio::test]
async fn an_untrusted_tool_refuses_a_self_completion() {
    let strict = decl(
        "charge_card",
        Effect::Write,
        json!({ "type": "object" }),
        Some(json!({ "type": "object" })),
        false,
    );
    let server = client_tool_server(vec![strict], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "self-completion: {body}");
    assert_eq!(body["error"]["code"], "client_completion_refused");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("trust_completion = false"),
        "the refusal names the declaration that caused it: {message}"
    );
    assert!(
        message.contains(&format!("/v1/client-runs/{run}/resolve")),
        "the refusal names the endpoint that settles it: {message}"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "no completion was recorded");
    assert_eq!(
        derive_state(&log).status,
        RunStatus::NeedsReconciliation,
        "the strict mode needs no new state: the existing write rule already says this"
    );
}

/// Test 7: a declaration with no `output_schema` refuses a client completion.
/// There is nothing to check the report against, and an unfalsifiable
/// completion is what the schema exists to prevent.
#[tokio::test]
async fn a_tool_without_an_output_schema_refuses_a_completion() {
    let unfalsifiable = decl(
        "charge_card",
        Effect::Write,
        json!({ "type": "object" }),
        None,
        true,
    );
    let server = client_tool_server(vec![unfalsifiable], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "anything": true } }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "completion: {body}");
    assert_eq!(body["error"]["code"], "client_completion_refused");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no output_schema"),
        "the refusal names the missing schema: {message}"
    );
    assert!(
        message.contains(&format!("/v1/client-runs/{run}/resolve")),
        "the refusal names the endpoint that settles it: {message}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "no completion was recorded"
    );
}

/// Test 8: a client may not close a call the SERVER made. The dangling intent
/// here is recorded by the ordinary `tool-step` endpoint (its tool fails, which
/// is the legal crash story), so the refusal is proved against a genuinely
/// server-performed intent.
#[tokio::test]
async fn a_client_cannot_complete_a_server_performed_call() {
    let failing: Arc<dyn DynTool> = Arc::new(FailingTool {
        name: "charge_card".to_owned(),
        effect: Effect::Write,
    });
    let server = client_tool_server(vec![charge_card_decl()], vec![failing]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, body) = post(
        &client,
        &format!("{}/v1/client-runs/{run}/tool-step", server.base),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "tool-step: {body}");
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the write-ahead intent is recorded");
    assert!(
        matches!(&log[1].event, Event::ToolCallRequested { performed_by, .. }
            if performed_by.is_none()),
        "the intent is server-performed, got {:?}",
        log[1].event
    );

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "completion: {body}");
    assert_eq!(body["error"]["code"], "client_completion_refused");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("performed by this server")),
        "the refusal says whose call it was: {body}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "no completion was recorded"
    );
}

/// Test 9: both new endpoints are behind the per-run single-writer lease, like
/// every other driving endpoint on this surface.
#[tokio::test]
async fn both_endpoints_require_the_drive_token() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, body) = intent(
        &client,
        &server.base,
        &run,
        None,
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "tokenless intent: {body}");
    assert_eq!(body["error"]["code"], "missing_drive_token");

    // Open the intent properly, then try to complete it without the token.
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        None,
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "tokenless completion: {body}"
    );
    assert_eq!(body["error"]["code"], "missing_drive_token");
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "the tokenless completion wrote nothing"
    );
}

/// Test 10: `GET /v1/client-tools` returns every declaration an operator's
/// file loaded, schemas and all. This is the endpoint a client-driven loop
/// reads to get the model's function definitions from, so the response has to
/// carry `input_schema` and `output_schema` verbatim, plus `trust_completion`,
/// not just the name.
#[tokio::test]
async fn declarations_come_back_with_their_schemas_intact() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();

    let (status, body) = get_json(&client, &format!("{}/v1/client-tools", server.base), None).await;
    assert_eq!(status, StatusCode::OK, "list: {body}");
    let tools = body["client_tools"].as_array().expect("client_tools array");
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(tool["name"], json!("charge_card"));
    assert_eq!(tool["effect"], json!("write"));
    assert_eq!(
        tool["input_schema"],
        json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": { "amount_cents": { "type": "integer" } }
        }),
        "the input schema comes back verbatim: it doubles as the model's function parameters"
    );
    assert_eq!(
        tool["output_schema"],
        json!({
            "type": "object",
            "required": ["charge_id"],
            "properties": { "charge_id": { "type": "string" } }
        }),
    );
    assert_eq!(tool["trust_completion"], json!(true));
}

/// Test 11: a server started with no `--client-tool` files answers with an
/// empty collection, not an error. Every client-tool intent on such a server
/// is a clean `unknown_tool`, and this listing has to agree with that: nothing
/// declared is a complete, honest state, not a failure to report.
#[tokio::test]
async fn a_server_with_no_declarations_returns_an_empty_collection() {
    let server = client_tool_server(vec![], vec![]).await;
    let client = reqwest::Client::new();

    let (status, body) = get_json(&client, &format!("{}/v1/client-tools", server.base), None).await;
    assert_eq!(status, StatusCode::OK, "list: {body}");
    assert_eq!(
        body["client_tools"],
        json!([]),
        "no declarations is an empty list, not an error"
    );
}

/// Test 12: the listing sits behind the same bearer-auth layer as every other
/// `/v1` route, even though it carries no drive token: it is server
/// configuration, not run state, so the process-wide bearer is the only gate
/// it needs.
#[tokio::test]
async fn the_listing_is_behind_bearer_auth_when_a_token_is_configured() {
    let mock = MockServer::start().await;
    let factory = agent_factory(
        mock.uri(),
        "record",
        Effect::Read,
        CountBehavior::Record,
        counter(),
    );
    let mut client_tools = ClientToolRegistry::new();
    client_tools.declare(charge_card_decl());
    let state = app_state(memory_store(), factory)
        .with_client_tools(Arc::new(client_tools))
        .with_auth_token("s3cret");
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/client-tools", server.base);

    let (status, body) = get_json(&client, &url, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token: {body}");
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _) = get_json(&client, &url, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong token");

    let (status, body) = get_json(&client, &url, Some("s3cret")).await;
    assert_eq!(status, StatusCode::OK, "correct token passes: {body}");
    assert_eq!(body["client_tools"].as_array().map(Vec::len), Some(1));
}

/// Test 13: `settled` tells a caller re-posting an intent whether the work is
/// already done. A fresh intent comes back `false`; once the completion lands,
/// a byte-identical re-post of the very same intent comes back `true`, with the
/// same key, so a paranoid (payments) caller can tell "safe to perform" from
/// "already settled" from the response alone, without reading the log.
#[tokio::test]
async fn a_settled_intent_says_so_on_a_re_post() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let body = json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 2500 } });

    let (status, opened) = intent(&client, &server.base, &run, Some(&token), body.clone()).await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");
    assert_eq!(
        opened["settled"],
        json!(false),
        "a freshly-opened intent has no completion yet"
    );

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    let (status, reposted) = intent(&client, &server.base, &run, Some(&token), body).await;
    assert_eq!(status, StatusCode::OK, "re-posted intent: {reposted}");
    assert_eq!(
        reposted["settled"],
        json!(true),
        "the same intent, re-posted after completion, says so"
    );
    assert_eq!(
        reposted["idempotency_key"], opened["idempotency_key"],
        "the same key comes back either way"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        3,
        "RunStarted, intent, completion; the re-post wrote nothing"
    );
}

/// Test 14: a `require_equal` field pins what was authorized. An intent for
/// `amount_cents` 5000 whose completion claims 50000 is refused, naming the
/// field, both values, and the resolve endpoint, and nothing is recorded; the
/// honest completion reporting 5000 is accepted. The output schema alone cannot
/// catch this: 50000 is a perfectly well-shaped integer.
#[tokio::test]
async fn a_require_equal_mismatch_is_refused_and_the_honest_report_is_accepted() {
    let pinned = ClientToolDecl {
        name: "charge_card".to_owned(),
        effect: Effect::Write,
        input_schema: json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": { "amount_cents": { "type": "integer" } }
        }),
        output_schema: Some(json!({
            "type": "object",
            "required": ["amount_cents", "charge_id"],
            "properties": {
                "amount_cents": { "type": "integer" },
                "charge_id": { "type": "string" }
            }
        })),
        trust_completion: true,
        require_equal: vec!["amount_cents".to_owned()],
    };
    let server = client_tool_server(vec![pinned], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": { "amount_cents": 5000 } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    // A completion that claims a different amount than was authorized.
    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "amount_cents": 50000, "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "amount mismatch: {body}");
    assert_eq!(body["error"]["code"], "client_completion_refused");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("amount_cents") && message.contains("5000") && message.contains("50000"),
        "the refusal names the field and both values: {message}"
    );
    assert!(
        message.contains("require_equal")
            && message.contains(&format!("/v1/client-runs/{run}/resolve")),
        "the refusal explains the rule and points at resolve: {message}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "the mismatch recorded nothing; the intent still stands"
    );

    // The honest completion, reporting the authorized amount, is accepted.
    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "amount_cents": 5000, "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "honest completion: {done}");
    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, completion");
    assert!(
        matches!(&log[2].event, Event::ToolCallCompleted { seq, output, .. }
            if seq.get() == 1 && output["amount_cents"] == json!(5000)),
        "the honest completion is recorded, got {:?}",
        log[2].event
    );
}

/// Test 15: `GET /v1/client-tools` carries `require_equal` only when the
/// declaration names one, following the same insert-only-when-present shape the
/// endpoint already uses for `output_schema`. A declaration that pins nothing
/// has no `require_equal` key at all.
#[tokio::test]
async fn require_equal_appears_in_the_listing_only_when_set() {
    let pinned = ClientToolDecl {
        name: "wire_payout".to_owned(),
        effect: Effect::Write,
        input_schema: json!({ "type": "object", "required": ["amount_cents"] }),
        output_schema: Some(json!({ "type": "object", "required": ["amount_cents"] })),
        trust_completion: true,
        require_equal: vec!["amount_cents".to_owned()],
    };
    let server = client_tool_server(vec![pinned, charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();

    let (status, body) = get_json(&client, &format!("{}/v1/client-tools", server.base), None).await;
    assert_eq!(status, StatusCode::OK, "list: {body}");
    let tools = body["client_tools"].as_array().expect("client_tools array");
    assert_eq!(tools.len(), 2, "both declarations come back");

    let pinned_entry = tools
        .iter()
        .find(|tool| tool["name"] == json!("wire_payout"))
        .expect("the pinned declaration is listed");
    assert_eq!(
        pinned_entry["require_equal"],
        json!(["amount_cents"]),
        "a pinned declaration carries its require_equal: {pinned_entry}"
    );

    let unpinned_entry = tools
        .iter()
        .find(|tool| tool["name"] == json!("charge_card"))
        .expect("the unpinned declaration is listed");
    assert!(
        unpinned_entry.get("require_equal").is_none(),
        "an unpinned declaration carries no require_equal key: {unpinned_entry}"
    );
}
