//! Bearer-token auth, the whole of it.
//!
//! The posture is single-tenant: either a shared secret
//! guards every endpoint, or the server trusts its caller and a reverse proxy
//! owns auth. There is no user model and no RBAC.
//!
//! When [`AppState::auth_token`](crate::AppState::auth_token) is set, this
//! middleware requires `Authorization: Bearer <that token>` on every request
//! and answers anything else with a `401` carrying the standard error
//! envelope. When it is unset, the middleware is a pass-through.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

/// Rejects a request whose bearer token is missing or wrong, when a token is
/// configured; otherwise passes it straight through.
pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.auth_token() else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if presented == Some(&format!("Bearer {expected}")) {
        next.run(request).await
    } else {
        ApiError::Unauthorized.into_response()
    }
}
