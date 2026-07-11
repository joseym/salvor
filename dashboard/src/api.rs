//! The one-shot HTTP client for the control plane.
//!
//! The streaming side of the dashboard lives in [`crate::sse`] (the browser
//! `EventSource`, GET-only). Everything else the list, inbox, and spend views
//! need is a plain request/response call, and that is what this module owns:
//! typed GETs for runs, typed POSTs for resume and resolve, one place that
//! attaches the optional bearer, and one place that turns the server's error
//! envelope into a typed [`ApiError`].
//!
//! # The endpoints, and who calls them
//!
//! Every function here maps to one row of `crates/salvor-server/API.md`:
//!
//! - [`list_runs`] -> `GET /v1/runs`. Called by `RunList` (the landing table)
//!   and by `Spend` (to enumerate the runs it then folds for cost).
//! - [`get_run`] -> `GET /v1/runs/{id}`. Called by the inbox cards to read one
//!   parked run's folded status and its dangling write intent.
//! - [`resume_run`] -> `POST /v1/runs/{id}/resume`. Called by `SuspensionCard`
//!   (form input) and `BudgetCard` (the `extend` payload).
//! - [`resolve_run`] -> `POST /v1/runs/{id}/resolve`. Called by
//!   `ReconciliationCard` to record a dangling write's observed outcome.
//!
//! # Types mirror the wire, not the replay crate
//!
//! The replay vocabulary ([`crate::replay`]) is the fold's own types and does
//! not derive serde, and the server serializes its own JSON shape anyway. So the
//! response structs here are wire types: [`ApiStatus`] matches the documented
//! `{ "state": ... }` status object, [`ApiPending`] matches the pending / intent
//! object. They reuse the replay crate only where its types already ride the
//! wire ([`Budget`](crate::replay::Budget), [`Effect`](crate::replay::Effect)),
//! and [`ApiStatus::to_run_status`] bridges to the real [`RunStatus`] so the
//! shared [`StatusBadge`](crate::status::StatusBadge) and grouping are reused,
//! not re-derived.
//!
//! # Errors
//!
//! Every failure is an [`ApiError`]: a transport failure, a decode failure, or
//! the server's typed envelope. The reconciliation refusal is the one envelope
//! carrying structured evidence; [`ApiError::reconciliation_intent`] reads its
//! `details.intent` back out as an [`ApiPending`].

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use gloo_net::http::{Request, Response};

use crate::config::Config;
use crate::replay::{Budget, Effect, RunStatus};
use crate::status::{StatusGroup, status_group};

/// The folded run status, as the server serializes it.
///
/// This is the wire twin of [`RunStatus`]: the same states, tagged by a
/// snake_case `state` field, with the extra keys the documented status object
/// carries. [`to_run_status`](ApiStatus::to_run_status) maps it to the real
/// replay type so the shared badge and grouping code are reused.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApiStatus {
    /// Empty log.
    NotStarted,
    /// Between steps, can continue.
    Running,
    /// A model call intent with no completion.
    AwaitingModel,
    /// A read or idempotent tool intent with no completion.
    AwaitingTool,
    /// A write intent with no completion: only a human moves it on.
    NeedsReconciliation,
    /// Parked awaiting input, with the schema the input must satisfy.
    Suspended {
        /// Why the run parked.
        reason: String,
        /// The JSON Schema a resume input is validated against.
        input_schema: Value,
    },
    /// A declared budget was crossed.
    BudgetExceeded {
        /// The budget that was crossed (`kind` and `limit`).
        budget: Budget,
        /// The observed value in the budget's units.
        observed: f64,
    },
    /// Finished with an output.
    Completed {
        /// The recorded final output.
        output: Value,
    },
    /// Failed with an error.
    Failed {
        /// The recorded failure description.
        error: String,
    },
}

impl ApiStatus {
    /// Bridges to the real [`RunStatus`] so the shared status atoms render this
    /// without a second status model. A clone, because the replay type owns its
    /// payloads.
    #[must_use]
    pub fn to_run_status(&self) -> RunStatus {
        match self {
            ApiStatus::NotStarted => RunStatus::NotStarted,
            ApiStatus::Running => RunStatus::Running,
            ApiStatus::AwaitingModel => RunStatus::AwaitingModel,
            ApiStatus::AwaitingTool => RunStatus::AwaitingTool,
            ApiStatus::NeedsReconciliation => RunStatus::NeedsReconciliation,
            ApiStatus::Suspended {
                reason,
                input_schema,
            } => RunStatus::Suspended {
                reason: reason.clone(),
                input_schema: input_schema.clone(),
            },
            ApiStatus::BudgetExceeded { budget, observed } => RunStatus::BudgetExceeded {
                budget: *budget,
                observed: *observed,
            },
            ApiStatus::Completed { output } => RunStatus::Completed {
                output: output.clone(),
            },
            ApiStatus::Failed { error } => RunStatus::Failed {
                error: error.clone(),
            },
        }
    }

    /// The attention group this status falls into, via the shared mapping.
    #[must_use]
    pub fn group(&self) -> StatusGroup {
        status_group(&self.to_run_status())
    }
}

/// A dangling call intent, as the server serializes the pending object and the
/// reconciliation `details.intent`.
///
/// The wire twin of [`PendingCall`](crate::replay::PendingCall): tagged by
/// `kind`. The reconciliation evidence carries an extra `recorded_at`, which
/// serde ignores here (only the intent fields matter to the card).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiPending {
    /// An uncompleted model call.
    Model {
        /// Log position of the intent.
        seq: u64,
        /// Recorded request hash.
        request_hash: String,
    },
    /// An uncompleted tool call. A `write` here is the reconciliation evidence.
    Tool {
        /// Log position of the intent.
        seq: u64,
        /// The tool called.
        tool: String,
        /// The recorded input.
        input: Value,
        /// The declared effect class.
        effect: Effect,
        /// The recorded idempotency key, when one was set.
        #[serde(default)]
        idempotency_key: Option<String>,
    },
}

/// Token totals as `GET /v1/runs/{id}` reports them. `u64` so a long run's
/// accumulation cannot overflow the count.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApiUsage {
    /// Total input (prompt) tokens.
    pub input_tokens: u64,
    /// Total output (completion) tokens.
    pub output_tokens: u64,
}

/// One row of `GET /v1/runs`: a run and its folded status.
///
/// This is everything the list endpoint gives, and no more: there is no agent
/// id, no usage, and no step count here (see the `RunList` view for which
/// columns that leaves unavailable).
#[derive(Deserialize, Clone, Debug)]
pub struct RunSummary {
    /// The run id (a UUID string).
    pub run: String,
    /// The folded status.
    pub status: ApiStatus,
    /// Recorded events in the log.
    pub event_count: u64,
    /// When the first event was recorded (RFC 3339).
    pub first_recorded_at: String,
    /// When the last event was recorded (RFC 3339); the run's last activity.
    pub last_recorded_at: String,
}

/// `GET /v1/runs/{id}`: one run's full derived state, including the usage the
/// list omits and the dangling write intent the inbox needs.
#[derive(Deserialize, Clone, Debug)]
pub struct RunDetail {
    /// The run id.
    pub run: String,
    /// The folded status (carries the suspension schema or the crossed budget).
    pub status: ApiStatus,
    /// Recorded events in the log.
    #[serde(default)]
    pub event_count: u64,
    /// Accumulated token usage. Absent on a just-started run with no log yet.
    #[serde(default)]
    pub usage: ApiUsage,
    /// The dangling call intent, when one exists.
    #[serde(default)]
    pub pending: Option<ApiPending>,
}

/// `POST /v1/runs/{id}/resume` response. `outcome` is `driving` for a `202` or
/// `completed` / `failed` for an already-finished run.
#[derive(Deserialize, Clone, Debug)]
pub struct ResumeOutcome {
    /// The run id.
    pub run: String,
    /// What the resume did.
    #[serde(default)]
    pub outcome: Option<String>,
    /// The run's status after the call, kept raw (the shape varies by outcome).
    #[serde(default)]
    pub status: Value,
}

/// `POST /v1/runs/{id}/resolve` response.
#[derive(Deserialize, Clone, Debug)]
pub struct ResolveOutcome {
    /// The run id.
    pub run: String,
    /// True once the completion was recorded.
    #[serde(default)]
    pub resolved: bool,
    /// The run's status after the call, kept raw.
    #[serde(default)]
    pub status: Value,
}

/// Any way a call can fail.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// The request never produced a response: the network is down, the server
    /// is unreachable, or the browser refused the request.
    Transport(String),
    /// A response arrived but its body did not decode into the expected shape.
    Decode(String),
    /// The server answered with its typed error envelope.
    Server(ServerError),
}

/// The server's error envelope, decoded.
#[derive(Debug, Clone)]
pub struct ServerError {
    /// The HTTP status the envelope came with.
    pub status: u16,
    /// The stable machine code (e.g. `needs_reconciliation`, `bad_request`).
    pub code: String,
    /// The human sentence.
    pub message: String,
    /// Structured evidence, present today only on the reconciliation refusal.
    pub details: Option<ErrorDetails>,
}

/// The `details` object on an error envelope.
#[derive(Deserialize, Debug, Clone)]
pub struct ErrorDetails {
    /// The recorded write intent, on a `needs_reconciliation` refusal.
    #[serde(default)]
    pub intent: Option<ApiPending>,
}

impl ApiError {
    /// The recorded write intent when this is the `needs_reconciliation`
    /// refusal, so the caller can show the evidence and route to resolve. `None`
    /// for every other error.
    #[must_use]
    pub fn reconciliation_intent(&self) -> Option<&ApiPending> {
        match self {
            ApiError::Server(ServerError {
                code,
                details: Some(details),
                ..
            }) if code == "needs_reconciliation" => details.intent.as_ref(),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Transport(detail) => write!(f, "could not reach the server: {detail}"),
            ApiError::Decode(detail) => write!(f, "could not read the server response: {detail}"),
            ApiError::Server(err) => write!(f, "{} ({}): {}", err.code, err.status, err.message),
        }
    }
}

// Response envelopes that wrap the useful payload.

#[derive(Deserialize)]
struct RunsEnvelope {
    runs: Vec<RunSummary>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
    #[serde(default)]
    details: Option<ErrorDetails>,
}

/// `GET /v1/runs`: every run with its folded status. Called by `RunList` and by
/// `Spend`.
///
/// # Errors
///
/// Any [`ApiError`]: transport, decode, or a server envelope.
pub async fn list_runs(config: &Config) -> Result<Vec<RunSummary>, ApiError> {
    let envelope: RunsEnvelope = get_json(config, "/v1/runs").await?;
    Ok(envelope.runs)
}

/// `GET /v1/runs/{id}`: one run's derived state. Called by the inbox cards to
/// read the suspension schema, the crossed budget, or the dangling write intent.
///
/// # Errors
///
/// Any [`ApiError`], including a `404 unknown_run` server envelope.
pub async fn get_run(config: &Config, run_id: &str) -> Result<RunDetail, ApiError> {
    get_json(config, &format!("/v1/runs/{run_id}")).await
}

/// `POST /v1/runs/{id}/resume`: continue a run. Called by `SuspensionCard` (with
/// the form's JSON as `input`) and `BudgetCard` (with the `extend` payload).
///
/// `input` is sent under an `"input"` key; `None` sends `null` (the recover
/// case, where the server ignores it).
///
/// # Errors
///
/// Any [`ApiError`]. A parked run resumed with a rejected input is a
/// `400 bad_request`; a run needing reconciliation is a `409 needs_reconciliation`
/// whose intent [`ApiError::reconciliation_intent`] reads back.
pub async fn resume_run(
    config: &Config,
    run_id: &str,
    input: Option<Value>,
) -> Result<ResumeOutcome, ApiError> {
    let body = json!({ "input": input });
    post_json(config, &format!("/v1/runs/{run_id}/resume"), &body).await
}

/// `POST /v1/runs/{id}/resolve`: record a dangling write's observed outcome.
/// Called by `ReconciliationCard`. Records one completion event and drives
/// nothing; it never retries the write.
///
/// # Errors
///
/// Any [`ApiError`]. A run with no dangling write is a `409 wrong_state`.
pub async fn resolve_run(
    config: &Config,
    run_id: &str,
    output: Value,
) -> Result<ResolveOutcome, ApiError> {
    let body = json!({ "output": output });
    post_json(config, &format!("/v1/runs/{run_id}/resolve"), &body).await
}

/// The bearer token to attach, or `None` when the deployment runs tokenless.
///
/// API.md documents two auth modes: a shared-secret bearer on every request, or
/// no token (the default, where a reverse proxy owns auth). [`Config`] carries
/// no token field today, so this returns `None` and requests go out
/// unauthenticated, which is the no-token default. When `Config` grows a token,
/// return it here: the header is attached in exactly one place (below), so every
/// call site starts carrying it with no other change.
fn bearer_token(_config: &Config) -> Option<String> {
    None
}

/// Formats the `Authorization` header value for an optional token, or `None`
/// when there is no token. Split out from the request builders so the
/// bearer-carrying rule is a pure, tested function rather than browser-only glue.
#[must_use]
fn authorization_header(token: Option<&str>) -> Option<String> {
    token.map(|token| format!("Bearer {token}"))
}

/// A typed GET: build the URL from [`Config`], attach the bearer if present,
/// send, and decode the body (or the error envelope).
async fn get_json<T: DeserializeOwned>(config: &Config, path: &str) -> Result<T, ApiError> {
    let url = config.url(path);
    let mut builder = Request::get(&url);
    if let Some(header) = authorization_header(bearer_token(config).as_deref()) {
        builder = builder.header("Authorization", &header);
    }
    let response = builder
        .send()
        .await
        .map_err(|err| ApiError::Transport(err.to_string()))?;
    read_json(response).await
}

/// A typed POST with a JSON body: same URL and bearer handling as
/// [`get_json`], serializing `body` and setting `Content-Type`.
async fn post_json<T: DeserializeOwned>(
    config: &Config,
    path: &str,
    body: &Value,
) -> Result<T, ApiError> {
    let url = config.url(path);
    let mut builder = Request::post(&url).header("Content-Type", "application/json");
    if let Some(header) = authorization_header(bearer_token(config).as_deref()) {
        builder = builder.header("Authorization", &header);
    }
    let serialized =
        serde_json::to_string(body).map_err(|err| ApiError::Decode(err.to_string()))?;
    let request = builder
        .body(serialized)
        .map_err(|err| ApiError::Transport(err.to_string()))?;
    let response = request
        .send()
        .await
        .map_err(|err| ApiError::Transport(err.to_string()))?;
    read_json(response).await
}

/// Reads a response body: decode `T` on a 2xx, otherwise parse the error
/// envelope. Reading the text once keeps the error path able to fall back to the
/// raw body when the envelope itself does not parse.
async fn read_json<T: DeserializeOwned>(response: Response) -> Result<T, ApiError> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| ApiError::Transport(err.to_string()))?;
    if (200..300).contains(&status) {
        serde_json::from_str::<T>(&text).map_err(|err| ApiError::Decode(err.to_string()))
    } else {
        Err(parse_error(status, &text))
    }
}

/// Turns a non-2xx body into a typed [`ApiError::Server`]. A body that is not
/// the documented envelope still yields a server error, carrying a clipped copy
/// of the raw body rather than losing it. Pure, so it is unit-tested directly.
fn parse_error(status: u16, text: &str) -> ApiError {
    match serde_json::from_str::<ErrorEnvelope>(text) {
        Ok(envelope) => ApiError::Server(ServerError {
            status,
            code: envelope.error.code,
            message: envelope.error.message,
            details: envelope.error.details,
        }),
        Err(_) => ApiError::Server(ServerError {
            status,
            code: "unknown".to_string(),
            message: text.chars().take(200).collect(),
            details: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_header_formats_only_when_present() {
        assert_eq!(
            authorization_header(Some("s3cret")),
            Some("Bearer s3cret".to_string())
        );
        assert_eq!(authorization_header(None), None);
    }

    #[test]
    fn status_object_decodes_and_bridges_to_the_replay_type() {
        let suspended: ApiStatus = serde_json::from_str(
            r#"{"state":"suspended","reason":"need approval","input_schema":{"type":"object"}}"#,
        )
        .expect("decode suspended");
        assert_eq!(suspended.group(), StatusGroup::WaitingOnHuman);
        assert!(matches!(
            suspended.to_run_status(),
            RunStatus::Suspended { .. }
        ));

        let budget: ApiStatus = serde_json::from_str(
            r#"{"state":"budget_exceeded","budget":{"kind":"steps","limit":10.0},"observed":11.0}"#,
        )
        .expect("decode budget_exceeded");
        assert_eq!(budget.group(), StatusGroup::WaitingOnHuman);

        let completed: ApiStatus =
            serde_json::from_str(r#"{"state":"completed","output":{"ok":true}}"#)
                .expect("decode completed");
        assert_eq!(completed.group(), StatusGroup::Terminal);
    }

    #[test]
    fn run_summary_decodes_from_the_list_shape() {
        let summary: RunSummary = serde_json::from_str(
            r#"{"run":"6f","status":{"state":"running"},"event_count":10,
                "first_recorded_at":"2026-07-09T12:00:00Z",
                "last_recorded_at":"2026-07-09T12:05:00Z"}"#,
        )
        .expect("decode run summary");
        assert_eq!(summary.event_count, 10);
        assert_eq!(summary.group(), StatusGroup::InProgress);
    }

    #[test]
    fn reconciliation_error_surfaces_its_recorded_intent() {
        // The documented 409 needs_reconciliation envelope.
        let body = r#"{"error":{
            "code":"needs_reconciliation",
            "message":"run needs reconciliation",
            "details":{"intent":{
                "kind":"tool","seq":4,"tool":"charge","input":{"amount":10},
                "effect":"write","idempotency_key":null,"recorded_at":"2026-07-09T12:00:00Z"
            }}
        }}"#;
        let error = parse_error(409, body);
        let intent = error
            .reconciliation_intent()
            .expect("the intent is surfaced");
        match intent {
            ApiPending::Tool { tool, effect, .. } => {
                assert_eq!(tool, "charge");
                assert_eq!(*effect, Effect::Write);
            }
            ApiPending::Model { .. } => panic!("a write intent, not a model call"),
        }
    }

    #[test]
    fn a_non_envelope_error_body_is_still_a_server_error() {
        let error = parse_error(500, "gateway timeout");
        match error {
            ApiError::Server(server) => {
                assert_eq!(server.status, 500);
                assert_eq!(server.code, "unknown");
                assert!(server.message.contains("gateway timeout"));
            }
            _ => panic!("a server error"),
        }
    }
}

impl RunSummary {
    /// The attention group of this row, for sorting and the badge.
    #[must_use]
    pub fn group(&self) -> StatusGroup {
        self.status.group()
    }
}
