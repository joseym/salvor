//! Salvor control plane: an HTTP and server-sent-events server over the
//! durable runtime.
//!
//! This crate is a thin network surface: submit an
//! agent definition, start a run, stream its events, resume or recover or
//! resolve it, list and inspect runs. Durability stays where it belongs, in
//! the one Rust process that owns the event store; the server holds handles
//! and constructs a [`salvor_runtime::Runtime`] per request, so nothing about
//! a run lives in the process that a restart would lose. Clients (the v0.3
//! Python and TypeScript SDKs, the dashboard) are thin because the guarantees
//! are not theirs to keep.
//!
//! The HTTP contract, every route and shape and the event framing, is the
//! deliverable those clients build against. It is specified in `API.md`
//! alongside this source, and each handler module documents its own routes.
//!
//! # Shape of the crate
//!
//! - [`AppState`] is the shared handle: the store, the agent registry, the
//!   run-driver bookkeeping, and the [`AgentFactory`] seam that turns a
//!   submitted definition into a live agent.
//! - [`build_router`] wires the routes and the auth layer; [`serve`] binds and
//!   runs them.
//! - [`dispatch`] holds the state-to-verb mapping shared with the CLI, so the
//!   two surfaces agree on what a run's state means.
//! - The handler modules ([`agents`](crate::agents), [`runs`](crate::runs),
//!   [`sse`](crate::sse)) own their endpoints; [`error::ApiError`] is the one
//!   error envelope they all return.
//!
//! # Auth
//!
//! One optional shared-secret bearer, a single-tenant posture.
//! With a token set on the state, every route requires
//! `Authorization: Bearer <token>`; without one, the server trusts its caller
//! and a reverse proxy owns auth. No user model, no RBAC.

#![warn(missing_docs)]

pub mod agents;
pub mod auth;
pub mod client_runs;
pub mod dispatch;
pub mod error;
pub mod executor;
pub mod graph;
pub mod json;
pub mod runs;
pub mod sse;
pub mod state;
pub mod tool_registry;
#[cfg(feature = "ui")]
pub mod ui;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use tokio::net::TcpListener;

pub use dispatch::{Disposition, ResumeKind, classify};
pub use error::ApiError;
pub use executor::{LlmModelExecutor, ModelExecutor, ModelStream};
pub use state::{
    AgentDefinition, AgentFactory, AppState, BuildFuture, BuiltAgent, ClientRunLease, DefFormat,
    RegisteredAgent,
};
pub use tool_registry::ToolRegistry;

/// Builds the control-plane router over `state`, with the bearer-auth layer in
/// front of every route.
///
/// With the `ui` feature on, the embedded dashboard is served from the router's
/// fallback, added after the auth layer so a browser can fetch the app shell
/// and its assets without a bearer token; the `/v1` API keeps the auth it
/// registered above. The fallback also holds the SPA rule: a non-API,
/// non-file GET returns `index.html` so a deep link cold-loads.
pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/agents", post(agents::register).get(agents::list))
        .route("/v1/agents/{hash}", get(agents::get))
        .route("/v1/runs", post(runs::start).get(runs::list))
        .route("/v1/runs/{id}", get(runs::get))
        .route("/v1/runs/{id}/replay", get(runs::replay))
        .route("/v1/runs/{id}/events", get(sse::stream))
        .route("/v1/runs/{id}/resume", post(runs::resume))
        .route("/v1/runs/{id}/resolve", post(runs::resolve))
        .route("/v1/runs/{id}/graph", get(graph::projection))
        .route("/v1/runs/{id}/fork", post(graph::fork))
        .route("/v1/runs/{id}/forks", get(graph::forks))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/graphs", post(graph::submit).get(graph::list))
        .route("/v1/graphs/validate", post(graph::validate_only))
        .route("/v1/graphs/{hash}", get(graph::get))
        .route("/v1/graph-runs", post(graph::start_run))
        .route("/v1/client-runs", post(client_runs::open))
        .route("/v1/client-runs/{id}/log", get(client_runs::get_log))
        .route("/v1/client-runs/{id}/events", post(client_runs::append))
        .route(
            "/v1/client-runs/{id}/model-step",
            post(client_runs::model_step),
        )
        .route(
            "/v1/client-runs/{id}/tool-step",
            post(client_runs::tool_step),
        )
        .route("/v1/client-runs/{id}/resolve", post(client_runs::resolve))
        .layer(from_fn_with_state(state.clone(), auth::require_bearer));

    // The dashboard fallback is added after the auth layer, so it sits outside
    // it: static assets and the SPA shell answer without a bearer token, while
    // every `/v1` route above keeps the auth it registered.
    #[cfg(feature = "ui")]
    let api = api.fallback(ui::static_handler);

    api.with_state(state)
}

/// `GET /v1/capabilities`: what this build of the control plane can do, for a
/// dashboard to probe before offering a capability-gated action (the Bridge
/// gates its fork UI on this). Additive and honest: a capability is advertised
/// only when the feature genuinely exists on this server, so a probe is never a
/// promise the server cannot keep. This build advertises `fork: true`, the fork
/// endpoint ([`graph::fork`]).
///
/// The sibling `server` object names the exact build serving the response, so
/// a dashboard can always show precisely what it is talking to:
/// `server.version` is `env!("CARGO_PKG_VERSION")`, compile-time-constant and
/// therefore always correct for the running binary; `server.commit` is the
/// short git hash `build.rs` shells out for at build time, present only when
/// that build had a `.git` to ask, omitted entirely (never a placeholder)
/// otherwise, and suffixed `-dirty` when the working tree carried uncommitted
/// changes at build time.
async fn capabilities() -> axum::response::Response {
    let mut server = serde_json::json!({ "version": env!("CARGO_PKG_VERSION") });
    if let Some(commit) = option_env!("SALVOR_SERVER_GIT_COMMIT") {
        server["commit"] = serde_json::Value::String(commit.to_owned());
    }
    axum::response::IntoResponse::into_response(axum::Json(serde_json::json!({
        "capabilities": { "fork": true },
        "server": server,
    })))
}

/// Serves the control plane on `listener` until the process ends.
///
/// The caller binds the listener (so it may choose `127.0.0.1:0` and read the
/// assigned port), then hands it here.
///
/// # Errors
///
/// Propagates any error from the underlying `axum::serve`.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, build_router(state)).await
}
