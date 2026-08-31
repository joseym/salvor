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
//!
//! Four groups of tests sit here, and they run in this order: the declaration
//! and the end-to-end call, then the resolve tests (a hand-recorded output meets
//! the same declaration a client's report does, and the completion it writes
//! names its settler), then the failure tests (a client-reported error records
//! the sentinel a native tool records, byte for byte), then the declared-key
//! tests (an idempotency key derived from named input fields makes a repeated
//! call one call).

mod common;

use std::sync::Arc;

use common::{
    CountBehavior, TestServer, agent_factory, app_state, counter, get_json, memory_store,
};
use reqwest::StatusCode;
use salvor_core::{
    Effect, Event, EventEnvelope, Performer, RunId, RunStatus, SequenceNumber, SettledBy,
    derive_state,
};
use salvor_runtime::{ToolFailure, ToolFailureKind, decode_failure, encode_failure};
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
        idempotency_key: Vec::new(),
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
            caller: None,
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
        message.contains(&format!("/v1/runs/{run}/resolve")) && message.contains("salvor resolve"),
        "the refusal names a path that does not need the lease this client is about to drop: \
         {message}"
    );
    assert!(
        !message.contains("/v1/client-runs/"),
        "the lease-gated resolve is not the way out of a refused completion: {message}"
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
        message.contains(&format!("/v1/runs/{run}/resolve")) && message.contains("salvor resolve"),
        "the refusal names a path that does not need the lease this client is about to drop: \
         {message}"
    );
    assert!(
        !message.contains("/v1/client-runs/"),
        "the lease-gated resolve is not the way out of a refused completion: {message}"
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
        idempotency_key: Vec::new(),
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
            && message.contains(&format!("/v1/runs/{run}/resolve"))
            && message.contains("salvor resolve"),
        "the refusal explains the rule and points at a resolve the caller can reach: {message}"
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
        idempotency_key: Vec::new(),
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

/// The pinned-write declaration the resolve tests settle by hand: a write the
/// client performs, whose completion must carry the provider's own id and may
/// not restate the amount as anything but what was authorized, and which the
/// client is NOT trusted to close itself.
fn wire_payout_decl() -> ClientToolDecl {
    ClientToolDecl {
        name: "wire_payout".to_owned(),
        effect: Effect::Write,
        input_schema: json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": { "amount_cents": { "type": "integer" } }
        }),
        output_schema: Some(json!({
            "type": "object",
            "required": ["payout_id", "amount_cents"],
            "properties": {
                "payout_id": { "type": "string" },
                "amount_cents": { "type": "integer" }
            }
        })),
        trust_completion: false,
        require_equal: vec!["amount_cents".to_owned()],
        idempotency_key: Vec::new(),
    }
}

/// Opens a run whose log ends at a dangling `wire_payout` intent for
/// `amount_cents`, the state both resolve endpoints exist to settle.
async fn run_awaiting_a_payout(
    client: &reqwest::Client,
    base: &str,
    amount_cents: i64,
) -> (String, String) {
    let (run, token) = started_run(client, base).await;
    let (status, opened) = intent(
        client,
        base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "wire_payout", "input": { "amount_cents": amount_cents } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");
    (run, token)
}

/// A resolve against the operator's endpoint, which presents no drive token.
async fn resolve(
    client: &reqwest::Client,
    base: &str,
    run: &str,
    output: Value,
) -> (StatusCode, Value) {
    post(
        client,
        &format!("{base}/v1/runs/{run}/resolve"),
        json!({ "output": output }),
        None,
    )
    .await
}

/// A hand-recorded output may not restate a pinned field as something other
/// than what the intent authorized. The operator resolving is not the client,
/// but the output is still nobody's witnessed fact, and a run whose log said a
/// 5000 payout was authorized must not end up carrying 50000 as its result.
#[tokio::test]
async fn a_resolution_may_not_alter_a_pinned_field() {
    let server = client_tool_server(vec![wire_payout_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, _token) = run_awaiting_a_payout(&client, &server.base, 5000).await;

    let (status, body) = resolve(
        &client,
        &server.base,
        &run,
        json!({ "payout_id": "po_1", "amount_cents": 50000 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "resolve: {body}");
    assert_eq!(body["error"]["code"], "bad_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("wire_payout") && message.contains("amount_cents"),
        "the refusal names the tool and the field: {message}"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the refusal recorded nothing");
    assert_eq!(
        derive_state(&log).status,
        RunStatus::NeedsReconciliation,
        "the write is still outstanding"
    );
}

/// An output missing a field the declaration requires is refused by the same
/// `output_schema` check a client's own report meets. `payout_id` is the one
/// field a report could not have invented, so a resolution without it is a
/// claim rather than a result.
#[tokio::test]
async fn a_resolution_missing_a_required_field_is_refused() {
    let server = client_tool_server(vec![wire_payout_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, _token) = run_awaiting_a_payout(&client, &server.base, 5000).await;

    let (status, body) =
        resolve(&client, &server.base, &run, json!({ "amount_cents": 5000 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "resolve: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("wire_payout") && message.contains("payout_id"),
        "the refusal names the tool and the missing field: {message}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "the refusal recorded nothing"
    );
}

/// The honest resolution passes both checks, records exactly one completion,
/// and stamps who settled it: the log now says an operator closed this call by
/// hand, which is not a thing the run could have said for itself.
#[tokio::test]
async fn a_good_resolution_is_accepted_and_names_its_settler() {
    let server = client_tool_server(vec![wire_payout_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, _token) = run_awaiting_a_payout(&client, &server.base, 5000).await;

    let (status, body) = resolve(
        &client,
        &server.base,
        &run,
        json!({ "payout_id": "po_1", "amount_cents": 5000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");
    assert_eq!(body["resolved"], json!(true));

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "exactly one completion was appended");
    let Event::ToolCallCompleted {
        seq,
        output,
        settled_by,
        ..
    } = &log[2].event
    else {
        panic!("seq 2 holds the completion, got {:?}", log[2].event);
    };
    assert_eq!(seq.get(), 1, "it correlates to the dangling intent");
    assert_eq!(
        output,
        &json!({ "payout_id": "po_1", "amount_cents": 5000 })
    );
    assert_eq!(
        *settled_by,
        Some(SettledBy::Operator),
        "a hand-recorded completion says who recorded it"
    );
    assert_eq!(
        derive_state(&log).status,
        RunStatus::Running,
        "the run is drivable again"
    );
}

/// A run whose tool this server no longer declares is refused rather than
/// resolved unchecked. A stale registry is the operator's to fix, and the
/// message says how, because recording an unexamined output is the one thing
/// this path must not do.
#[tokio::test]
async fn a_resolution_for_an_undeclared_tool_is_refused() {
    let declaring = client_tool_server(vec![wire_payout_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, _token) = run_awaiting_a_payout(&client, &declaring.base, 5000).await;

    // The same store, served by a process that was started without the
    // declaration: exactly what an operator meets after dropping a
    // `--client-tool` file from the command line.
    let forgetful = TestServer::spawn(
        app_state(
            declaring.state.store(),
            agent_factory(
                MockServer::start().await.uri(),
                "record",
                Effect::Read,
                CountBehavior::Record,
                counter(),
            ),
        )
        .with_client_tools(Arc::new(ClientToolRegistry::new())),
    )
    .await;

    let (status, body) = resolve(
        &client,
        &forgetful.base,
        &run,
        json!({ "payout_id": "po_1", "amount_cents": 5000 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "resolve: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("wire_payout") && message.contains("--client-tool"),
        "the refusal names the tool and how to declare it: {message}"
    );
    assert_eq!(
        read_log(&client, &declaring.base, &run).await.len(),
        2,
        "the refusal recorded nothing"
    );
}

/// A read the client never finished is not an operator's problem, and the
/// refusal says so. `recover` is a server-driven verb, and the resume endpoint
/// refuses a client-driven run outright, so naming it would send an operator at
/// a door that is already locked.
#[tokio::test]
async fn a_resolve_refusal_never_sends_a_client_driven_run_to_recover() {
    let lookup = decl(
        "lookup_order",
        Effect::Read,
        json!({ "type": "object" }),
        Some(json!({ "type": "object" })),
        true,
    );
    let server = client_tool_server(vec![lookup], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "lookup_order", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, body) = resolve(&client, &server.base, &run, json!({ "found": true })).await;
    assert_eq!(status, StatusCode::CONFLICT, "resolve: {body}");
    assert_eq!(body["error"]["code"], "wrong_state");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("recover"),
        "a client-driven run has no recover to reach for: {message}"
    );
    assert!(
        message.contains("lookup_order") && message.contains("performs again on its next drive"),
        "the refusal says what actually settles this call: {message}"
    );
}

/// A client tool whose call threw records that, and what lands in the log is
/// byte for byte the sentinel `salvor_runtime` writes when a NATIVE tool
/// exhausts its retries. A failure is not a new event and not a new run state;
/// it is an outcome a completion is allowed to carry, and a log written through
/// this endpoint has to mean to a replay exactly what a natively recorded one
/// means. The declaration carries no `output_schema`, which the value path
/// refuses outright: with no value reported there is nothing for a shape check
/// to look at.
#[tokio::test]
async fn a_reported_failure_records_the_native_sentinel_for_a_read() {
    let lookup = decl(
        "lookup_order",
        Effect::Read,
        json!({ "type": "object" }),
        None,
        true,
    );
    let server = client_tool_server(vec![lookup], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let (status, opened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "lookup_order", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "intent: {opened}");

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "error": { "message": "the order service was unreachable" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "failure completion: {done}");
    assert_eq!(done["completed"], json!(true));

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 3, "RunStarted, intent, failure completion");
    let Event::ToolCallCompleted { seq, output, .. } = &log[2].event else {
        panic!("seq 2 holds the completion, got {:?}", log[2].event);
    };
    assert_eq!(seq.get(), 1, "it correlates to the intent");
    assert_eq!(
        output,
        &encode_failure(&ToolFailure {
            kind: ToolFailureKind::Handler,
            message: "the order service was unreachable".to_owned(),
            attempts: 1,
        }),
        "the recorded bytes are the runtime's own, not this endpoint's idea of them"
    );
    assert_eq!(
        decode_failure(output).map(|failure| failure.kind),
        Some(ToolFailureKind::Handler),
        "an unnamed kind records as the layer a client tool fails at"
    );

    // A recorded failure SETTLES the call, exactly as it does for a native
    // tool: the intent is closed, the run carries on, and a replay reads the
    // failure back rather than performing the call a second time.
    assert_eq!(derive_state(&log).status, RunStatus::Running);
    let (status, reopened) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "lookup_order", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "re-post: {reopened}");
    assert_eq!(
        reopened["settled"],
        json!(true),
        "the position is answered from the log, not performed again: {reopened}"
    );
}

/// The same for a write the operator trusts the client to close, and with a
/// `kind` named on the wire. A trusted write's failure is the client's word
/// like any other completion for it, and the recorded bytes are the runtime's.
#[tokio::test]
async fn a_reported_failure_records_the_native_sentinel_for_a_write() {
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

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "error": { "message": "card declined", "kind": "invalid_input" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "failure completion: {done}");

    let log = read_log(&client, &server.base, &run).await;
    let Event::ToolCallCompleted { output, .. } = &log[2].event else {
        panic!("seq 2 holds the completion, got {:?}", log[2].event);
    };
    assert_eq!(
        output,
        &encode_failure(&ToolFailure {
            kind: ToolFailureKind::InvalidInput,
            message: "card declined".to_owned(),
            attempts: 1,
        }),
        "the named kind is recorded and the bytes are the runtime's"
    );
    assert_eq!(
        derive_state(&log).status,
        RunStatus::Running,
        "a recorded failure closes the call, so the write no longer dangles"
    );
}

/// The trust rule holds for a failure exactly as it holds for a result. A
/// write the operator did not trust the client to close is not closed by the
/// client saying it failed either: "it did not land" is a claim about money,
/// made by the party that would benefit from it being believed. The intent
/// stands, the run needs reconciliation, and the operator's resolve settles it.
#[tokio::test]
async fn an_untrusted_tool_refuses_a_reported_failure_and_resolve_settles_it() {
    let server = client_tool_server(vec![wire_payout_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = run_awaiting_a_payout(&client, &server.base, 5000).await;

    let (status, body) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "error": { "message": "the bank timed out" } }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "failure completion: {body}");
    assert_eq!(body["error"]["code"], "client_completion_refused");

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(log.len(), 2, "the refusal recorded nothing");
    assert_eq!(
        derive_state(&log).status,
        RunStatus::NeedsReconciliation,
        "the write is outstanding, which is what a person has to settle"
    );

    let (status, body) = resolve(
        &client,
        &server.base,
        &run,
        json!({ "payout_id": "po_1", "amount_cents": 5000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve: {body}");
    assert_eq!(
        derive_state(&read_log(&client, &server.base, &run).await).status,
        RunStatus::Running,
        "the operator's resolution is what unsticks it"
    );
}

/// A body carrying both halves, or neither, is refused before anything is
/// looked at: `output` and `error` say opposite things about the same call and
/// this server has no way to pick between them.
#[tokio::test]
async fn a_completion_carries_a_result_or_a_failure_never_both() {
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

    for body in [
        json!({
            "seq": 1,
            "output": { "charge_id": "ch_9" },
            "error": { "message": "it also failed" }
        }),
        json!({ "seq": 1 }),
    ] {
        let (status, refused) = completion(&client, &server.base, &run, Some(&token), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "completion: {refused}");
        assert_eq!(refused["error"]["code"], "bad_request");
    }
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "neither refusal recorded anything"
    );
}

/// A `kind` that names no recorded layer is refused, naming the three that are
/// legal. The strings are a stable format replay parses, so an unrecognized one
/// must not reach the log.
#[tokio::test]
async fn an_unknown_failure_kind_is_refused() {
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
        json!({ "seq": 1, "error": { "message": "boom", "kind": "network" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "completion: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("network") && message.contains("handler"),
        "the refusal names the bad kind and the legal ones: {message}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        2,
        "the refusal recorded nothing"
    );
}

/// A declaration whose key names the fields that say what one call IS: a refund
/// of an amount against an order is one refund, wherever in the run it is asked
/// for.
fn refund_card_decl() -> ClientToolDecl {
    ClientToolDecl {
        name: "refund_card".to_owned(),
        effect: Effect::Write,
        input_schema: json!({
            "type": "object",
            "required": ["order_id", "amount_cents"],
            "properties": {
                "order_id": { "type": "string" },
                "amount_cents": { "type": "integer" }
            }
        }),
        output_schema: Some(json!({
            "type": "object",
            "required": ["provider_refund_id"],
            "properties": { "provider_refund_id": { "type": "string" } }
        })),
        trust_completion: true,
        require_equal: Vec::new(),
        idempotency_key: vec!["order_id".to_owned(), "amount_cents".to_owned()],
    }
}

/// Two intents whose declared key fields are equal derive one key, and the
/// second is answered from the first rather than performed again. This is the
/// whole point of a declared key: a loop that asks for the same refund twice
/// gets the first refund's result back, and the log says where the answer came
/// from instead of pretending a second call happened.
#[tokio::test]
async fn equal_key_fields_settle_the_second_intent_from_the_first() {
    let server = client_tool_server(vec![refund_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let call = json!({ "order_id": "ORD-7781", "amount_cents": 5000 });

    let (status, first) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "refund_card", "input": call }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first intent: {first}");
    assert_eq!(first["settled"], json!(false), "nothing has happened yet");
    let key = first["idempotency_key"].as_str().expect("key").to_owned();

    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "provider_refund_id": "re_1" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    // The model asks for the same refund again, two positions later.
    let (status, second) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 3, "tool": "refund_card", "input": call }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second intent: {second}");
    assert_eq!(
        second["idempotency_key"].as_str(),
        Some(key.as_str()),
        "the same call derives the same key at a different position: {second}"
    );
    assert_eq!(
        second["settled"],
        json!(true),
        "the call already happened: {second}"
    );
    assert_eq!(
        second["output"],
        json!({ "provider_refund_id": "re_1" }),
        "the answer is the first call's, carried on the intent response: {second}"
    );

    let log = read_log(&client, &server.base, &run).await;
    assert_eq!(
        log.len(),
        5,
        "the duplicate intent and its copied completion"
    );
    let Event::ToolCallCompleted {
        seq,
        output,
        deduplicated_from,
        ..
    } = &log[4].event
    else {
        panic!("seq 4 holds the copied completion, got {:?}", log[4].event);
    };
    assert_eq!(seq.get(), 3, "it correlates to the duplicate intent");
    assert_eq!(output, &json!({ "provider_refund_id": "re_1" }));
    let origin = deduplicated_from.expect("a copied completion names its origin");
    assert_eq!(origin.seq.get(), 1, "it names the call it copied");
    assert_eq!(
        origin.run_id.as_uuid().to_string(),
        run,
        "the identity is scoped to this run"
    );
}

/// Different values under the declared fields are different calls, and get
/// different keys. A refund of 6000 is not the refund of 5000, so the second
/// intent is open work the client still has to perform.
#[tokio::test]
async fn different_key_field_values_derive_different_keys() {
    let server = client_tool_server(vec![refund_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;

    let (status, first) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 1,
            "tool": "refund_card",
            "input": { "order_id": "ORD-7781", "amount_cents": 5000 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first intent: {first}");
    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "provider_refund_id": "re_1" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    let (status, second) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({
            "seq": 3,
            "tool": "refund_card",
            "input": { "order_id": "ORD-7781", "amount_cents": 6000 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second intent: {second}");
    assert_ne!(
        second["idempotency_key"], first["idempotency_key"],
        "a different amount is a different call: {second}"
    );
    assert_eq!(
        second["settled"],
        json!(false),
        "there is nothing recorded to answer it from: {second}"
    );
    assert!(
        second.get("output").is_none(),
        "an open call carries no output: {second}"
    );
    assert_eq!(
        read_log(&client, &server.base, &run).await.len(),
        4,
        "the intent was recorded and nothing was copied"
    );
}

/// With no fields declared the key stays positional, which is what every
/// declaration written before the field meant. Two identical calls at two
/// positions are two calls: the key is an attempt identifier, and nothing is
/// answered from anything.
#[tokio::test]
async fn an_undeclared_key_stays_positional() {
    let server = client_tool_server(vec![charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();
    let (run, token) = started_run(&client, &server.base).await;
    let call = json!({ "amount_cents": 2500 });

    let (status, first) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "tool": "charge_card", "input": call }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first intent: {first}");
    let (status, done) = completion(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 1, "output": { "charge_id": "ch_9" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "completion: {done}");

    let (status, second) = intent(
        &client,
        &server.base,
        &run,
        Some(&token),
        json!({ "seq": 3, "tool": "charge_card", "input": call }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second intent: {second}");
    assert_ne!(
        second["idempotency_key"], first["idempotency_key"],
        "the position is part of the key: {second}"
    );
    assert_eq!(
        second["settled"],
        json!(false),
        "a positional key deduplicates nothing: {second}"
    );
}

/// The declared key is published, so a client can derive it independently and
/// check this server's work. The tool that declares none carries no key.
#[tokio::test]
async fn the_declared_key_fields_are_listed() {
    let server = client_tool_server(vec![refund_card_decl(), charge_card_decl()], vec![]).await;
    let client = reqwest::Client::new();

    let (status, body) = get_json(&client, &format!("{}/v1/client-tools", server.base), None).await;
    assert_eq!(status, StatusCode::OK, "list: {body}");
    let tools = body["client_tools"].as_array().expect("client_tools array");
    let keyed = tools
        .iter()
        .find(|tool| tool["name"] == json!("refund_card"))
        .expect("the keyed declaration is listed");
    assert_eq!(
        keyed["idempotency_key"],
        json!(["order_id", "amount_cents"]),
        "the derivation is published: {keyed}"
    );
    let positional = tools
        .iter()
        .find(|tool| tool["name"] == json!("charge_card"))
        .expect("the positional declaration is listed");
    assert!(
        positional.get("idempotency_key").is_none(),
        "a positional declaration carries no key field: {positional}"
    );
}
