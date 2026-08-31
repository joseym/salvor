//! The caller name the auth layer attaches to a request it lets through.
//!
//! `lifecycle.rs` covers the gate itself: with a bearer configured, a request
//! without one is a `401`. This covers the other half, which no `/v1` route
//! reads yet: a request that verified carries a
//! [`salvor_server::Caller`](salvor_server::Caller) naming the token it came
//! in under, so a handler can read it with `Extension<Caller>`.
//!
//! The router here is this test's own: one probe route behind the real
//! `auth::require_bearer` layer, over a real `AppState`. That is the whole
//! surface under test, and building it here rather than adding a route to
//! `build_router` keeps the identity seam testable without putting a
//! test-only endpoint on the shipped API.

use std::io::Write;
use std::sync::Arc;

use axum::Router;
use axum::extract::Extension;
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use salvor_server::state::{AgentFactory, AppState};
use salvor_server::{Caller, TokenStore};
use salvor_store::{EventStore, SqliteStore};
use tokio::net::TcpListener;

/// A factory that builds nothing: this suite never starts a run.
fn no_agents() -> AgentFactory {
    Arc::new(|_definition| Box::pin(async { Err("this suite registers no agents".to_owned()) }))
}

/// An in-memory store, enough to construct an `AppState`.
fn store() -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::in_memory().expect("open an in-memory store"))
}

/// Answers with the caller name the auth layer attached, or `anonymous` when
/// no bearer is configured and there is no caller to name.
async fn whoami(caller: Option<Extension<Caller>>) -> String {
    match caller {
        Some(Extension(caller)) => caller.name().to_owned(),
        None => "anonymous".to_owned(),
    }
}

/// Binds a loopback port and serves one probe route behind the auth layer,
/// returning the base URL.
async fn spawn(state: AppState) -> String {
    let app = Router::new()
        .route("/whoami", get(whoami))
        .layer(from_fn_with_state(
            state.clone(),
            salvor_server::auth::require_bearer,
        ))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    format!("http://{addr}")
}

/// Writes a token file at mode 0600 declaring each `(name, token)` pair by
/// the token's SHA-256.
fn write_token_file(path: &std::path::Path, entries: &[(&str, &str)]) {
    let mut text = String::new();
    for (name, token) in entries {
        let hash: String = salvor_server::tokens::digest(token)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        text.push_str(&format!("[tokens.{name}]\nhash = \"{hash}\"\n\n"));
    }
    let mut file = std::fs::File::create(path).expect("create the token file");
    file.write_all(text.as_bytes()).expect("write it");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// A GET with an optional bearer, returning the status and the body text.
async fn get_whoami(client: &reqwest::Client, base: &str, token: Option<&str>) -> (u16, String) {
    let mut request = client.get(format!("{base}/whoami"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("send the probe request");
    let status = response.status().as_u16();
    let body = response.text().await.expect("read the probe body");
    (status, body)
}

#[tokio::test]
async fn a_named_token_reaches_the_handler_as_its_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tokens.toml");
    write_token_file(
        &path,
        &[("ci", "sv_ci_token_value"), ("ops", "sv_ops_token")],
    );

    let state = AppState::new(store(), no_agents())
        .with_token_file(TokenStore::load(&path).expect("load the token file"));
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = get_whoami(&client, &base, Some("sv_ci_token_value")).await;
    assert_eq!(status, 200, "the named token verifies: {body}");
    assert_eq!(body, "ci", "the handler reads the name it came in under");

    let (status, body) = get_whoami(&client, &base, Some("sv_ops_token")).await;
    assert_eq!(status, 200, "the other named token verifies: {body}");
    assert_eq!(body, "ops", "and is told apart from the first");
}

#[tokio::test]
async fn the_shared_secret_names_its_caller_token() {
    let state = AppState::new(store(), no_agents()).with_auth_token("a-secret-of-real-length");
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    let (status, body) = get_whoami(&client, &base, Some("a-secret-of-real-length")).await;
    assert_eq!(status, 200, "the shared secret verifies: {body}");
    assert_eq!(
        body,
        salvor_server::auth::SINGLE_TOKEN_CALLER,
        "the env-var secret has no name of its own, so it gets this one"
    );
}

#[tokio::test]
async fn a_file_token_and_the_shared_secret_both_verify() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tokens.toml");
    write_token_file(&path, &[("ci", "sv_ci_token_value")]);

    let state = AppState::new(store(), no_agents())
        .with_auth_token("a-secret-of-real-length")
        .with_token_file(TokenStore::load(&path).expect("load the token file"));
    let base = spawn(state).await;
    let client = reqwest::Client::new();

    assert_eq!(
        get_whoami(&client, &base, Some("sv_ci_token_value")).await,
        (200, "ci".to_owned()),
        "the file's token verifies"
    );
    assert_eq!(
        get_whoami(&client, &base, Some("a-secret-of-real-length")).await,
        (200, salvor_server::auth::SINGLE_TOKEN_CALLER.to_owned()),
        "and so does the shared secret; the two union"
    );
    let (status, _) = get_whoami(&client, &base, Some("neither-of-them")).await;
    assert_eq!(status, 401, "and nothing else does");
}

#[tokio::test]
async fn a_pass_through_server_names_no_caller() {
    let base = spawn(AppState::new(store(), no_agents())).await;
    let client = reqwest::Client::new();
    assert_eq!(
        get_whoami(&client, &base, None).await,
        (200, "anonymous".to_owned()),
        "with no bearer configured there is no caller to name"
    );
}
