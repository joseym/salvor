//! The agent-definition endpoints: register a definition, list registered
//! ones, and read one back.
//!
//! # Why a server-side registry keyed by the definition hash
//!
//! Under the single built-in loop an agent is pure data, and pure data has a
//! content hash: `agent_def_hash`, already recorded in every `RunStarted`
//! event. The control plane leans on that. A client submits a definition once
//! to `POST /v1/agents`; the server builds it (which validates it and computes
//! the hash), stores the raw definition under that hash, and returns the hash
//! as the agent's id. Starting a run then references the agent by hash and
//! carries only the input, so the start payload stays tiny and the same
//! definition drives every start, resume, and recover without the client
//! resubmitting it.
//!
//! The alternative, making the client pass the full definition on every start,
//! was declined: it would put the definition on the wire repeatedly, give the
//! server no stable id to talk about an agent by, and separate the id a run
//! records (`agent_def_hash`) from the id the API uses. Registering once and
//! referencing by the same hash the log already speaks keeps those aligned.
//!
//! Registration is also the definition's validation point: building the agent
//! spawns and immediately closes its MCP sessions, so a definition that cannot
//! build is rejected here with a `400` rather than failing later at the first
//! start.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use serde_json::json;

use crate::error::ApiError;
use crate::state::{AgentDefinition, AppState, DefFormat, RegisteredAgent};

/// Reads the definition format from the `Content-Type` header. TOML and JSON
/// are the two the definition is accepted in; anything else is a `400`.
fn format_from_headers(headers: &HeaderMap) -> Result<DefFormat, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.contains("toml") {
        Ok(DefFormat::Toml)
    } else if content_type.contains("json") {
        Ok(DefFormat::Json)
    } else {
        Err(ApiError::BadRequest(format!(
            "unsupported Content-Type `{content_type}`; send application/toml or application/json"
        )))
    }
}

/// `POST /v1/agents`: register (and validate) an agent definition.
///
/// The body is the definition in the format named by `Content-Type`. The
/// response is `{ "agent": "<agent_def_hash>", "created": <bool> }`, where
/// `created` is `false` when the identical definition was already registered.
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let format = format_from_headers(&headers)?;
    let definition = AgentDefinition {
        format,
        body: body.to_vec(),
    };

    // Building validates the definition and yields its content hash. The MCP
    // sessions the build opened are closed at once: registration only needs
    // the hash, and each start reopens fresh sessions.
    let built = state
        .build_agent(definition.clone())
        .await
        .map_err(ApiError::BadRequest)?;
    let agent_hash = built.agent.def_hash().to_owned();
    for server in built.servers {
        if let Err(error) = server.close().await {
            tracing::warn!(%error, "MCP session did not close cleanly after registration");
        }
    }

    let created = state.agent(&agent_hash).is_none();
    state.register_agent(RegisteredAgent {
        definition,
        agent_hash: agent_hash.clone(),
    });

    Ok((
        StatusCode::CREATED,
        Json(json!({ "agent": agent_hash, "created": created })),
    ))
}

/// `GET /v1/agents`: list the registered agent ids.
pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let agents: Vec<_> = state
        .agent_hashes()
        .into_iter()
        .map(|hash| json!({ "agent": hash }))
        .collect();
    Json(json!({ "agents": agents }))
}

/// `GET /v1/agents/{hash}`: read one registered definition back.
///
/// The response echoes the id, the format the definition was submitted in, and
/// the definition body as text.
pub async fn get(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let registered = state
        .agent(&hash)
        .ok_or_else(|| ApiError::UnknownAgent(format!("no agent registered under `{hash}`")))?;
    let format = match registered.definition.format {
        DefFormat::Toml => "toml",
        DefFormat::Json => "json",
    };
    let body = String::from_utf8_lossy(&registered.definition.body).into_owned();
    Ok(Json(json!({
        "agent": registered.agent_hash,
        "format": format,
        "definition": body,
    })))
}
