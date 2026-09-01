//! Revoking a token ends the event streams that token opened.
//!
//! The auth layer checks a request once, on the way in. A stream outlives
//! that check, so the stream re-checks the credential it opened under at the
//! top of every poll pass. These tests run that at the poll interval the
//! harness sets, 10ms, which is the fastest cadence the handler offers:
//! `app_state` shortens the same interval the stream tails the log with.
//!
//! Both tests make the same edit to the same file, dropping the `ops` entry
//! and keeping `ci`. The stream opened under `ops` ends; the stream opened
//! under `ci` carries on. The edit changes the file's length, so the token
//! store's stat reads it whatever the filesystem's timestamp resolution is.
//!
//! The run under each stream hangs in its tool, so it never reaches a resting
//! point and no stream here ends on its own. A stream that ends in either
//! test ended because the re-check ended it.

mod common;

use std::io::Write;
use std::time::Duration;

use common::{
    CountBehavior, ScriptedModel, SseReader, TestServer, agent_factory, app_state, counter,
    get_json, memory_store, post_json, register_agent, sample_toml, tool_use_response,
};
use salvor_core::Effect;
use salvor_server::TokenStore;
use serde_json::json;

/// The token the run is started under, and the stream that must survive.
const KEPT: &str = "sv_ci_token_value";

/// The token revoked mid-stream.
const REVOKED: &str = "sv_ops_token_value";

/// Writes a token file at mode 0600 declaring each `(name, token)` pair by
/// the token's SHA-256, replacing whatever the file held.
fn write_token_file(path: &std::path::Path, entries: &[(&str, &str)]) {
    let mut text = String::new();
    for (name, token) in entries {
        let hash: String = salvor_server::tokens::digest(token)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        text.push_str(&format!("[tokens.{name}]\nhash = \"{hash}\"\n\n"));
    }
    let mut file = std::fs::File::create(path).expect("create the token file");
    file.write_all(text.as_bytes())
        .expect("write the token file");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

/// What a test holds while it watches a stream: the mock model (dropping it
/// would end the run), the server, a client, and the run's id.
struct Fixture {
    _model: wiremock::MockServer,
    server: TestServer,
    client: reqwest::Client,
    run_id: String,
}

/// A server whose tokens come from `path`, running one agent whose tool hangs,
/// plus a started run's id. The run stays in flight for as long as the test
/// holds the fixture.
async fn hanging_run(path: &std::path::Path) -> Fixture {
    let model = ScriptedModel::mount(vec![(
        1,
        tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
        None,
    )])
    .await;
    let factory = agent_factory(
        model.uri(),
        "record",
        Effect::Read,
        CountBehavior::Hang,
        counter(),
    );
    let state = app_state(memory_store(), factory)
        .with_token_file(TokenStore::load(path).expect("load the token file"));
    let server = TestServer::spawn(state).await;
    let client = reqwest::Client::new();
    let agent = register_agent(&client, &server.base, sample_toml(), Some(KEPT)).await;
    let (_, body) = post_json(
        &client,
        &format!("{}/v1/runs", server.base),
        json!({ "agent": agent, "input": "research otters" }),
        Some(KEPT),
    )
    .await;
    let run_id = body["run"].as_str().expect("run id").to_owned();
    Fixture {
        _model: model,
        server,
        client,
        run_id,
    }
}

/// Reads one frame, proving the stream is live before the token file changes.
async fn first_frame(reader: &mut SseReader) {
    let frame = tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .expect("the stream sends its first frame")
        .expect("the stream is open");
    assert!(!frame.is_end(), "the run is still in flight, not resting");
}

#[tokio::test]
async fn revoking_a_token_ends_the_stream_it_opened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tokens.toml");
    write_token_file(&path, &[("ci", KEPT), ("ops", REVOKED)]);

    let fixture = hanging_run(&path).await;
    let mut reader = SseReader::open(
        &fixture.client,
        &fixture.server.base,
        &fixture.run_id,
        None,
        None,
        Some(REVOKED),
    )
    .await;
    first_frame(&mut reader).await;

    // Revoke `ops` by deleting its table. Nothing else touches the server, so
    // the only thing that can read the new file is the stream's own re-check.
    write_token_file(&path, &[("ci", KEPT)]);

    let frames = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end())
        .await
        .expect("the revoked stream ends rather than hanging on the run");
    let end = frames.last().expect("a final frame");
    assert!(end.is_end(), "the stream ends with an `end` frame");
    assert_eq!(
        end.json()["reason"],
        "unauthorized",
        "the end frame names why: {}",
        end.data
    );
    assert!(
        end.json()["error"]
            .as_str()
            .expect("an error message")
            .contains("re-authenticate"),
        "the end frame says what to do: {}",
        end.data
    );
}

#[tokio::test]
async fn a_token_the_rewrite_kept_streams_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tokens.toml");
    write_token_file(&path, &[("ci", KEPT), ("ops", REVOKED)]);

    let fixture = hanging_run(&path).await;
    let mut reader = SseReader::open(
        &fixture.client,
        &fixture.server.base,
        &fixture.run_id,
        None,
        None,
        Some(KEPT),
    )
    .await;
    first_frame(&mut reader).await;

    // The same edit as the test above: `ops` goes, `ci` stays.
    write_token_file(&path, &[("ci", KEPT)]);

    // A hundred poll passes at the harness's 10ms interval, which is a hundred
    // re-checks, every one of them against the rewritten file.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), reader.next()).await {
            // Nothing to say: the run is hung in its tool, and the stream is
            // still open, which is the behaviour under test here.
            Err(_) => continue,
            Ok(None) => panic!("the stream closed under a token the rewrite kept"),
            Ok(Some(frame)) => assert!(
                !frame.is_end(),
                "the stream ended under a token the rewrite kept: {}",
                frame.data
            ),
        }
    }

    // And the rewrite did land: the token it dropped no longer verifies.
    let (status, _) = get_json(
        &fixture.client,
        &format!("{}/v1/runs", fixture.server.base),
        Some(REVOKED),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "the rewrite that the kept stream survived is the one that revoked `ops`"
    );
}
