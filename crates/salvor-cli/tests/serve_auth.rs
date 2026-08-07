//! `salvor serve --auth-token`, end to end, exercised through the real CLI
//! flag (not `AppState` directly, which `salvor-server`'s own `lifecycle.rs`
//! already covers).
//!
//! Two paths:
//!
//! - The named variable is unset or empty: `serve` refuses to start. This is
//!   the fail-closed behaviour a typo'd variable name used to defeat silently
//!   (a `tracing::warn!` and an unauthenticated server, distinguishable only
//!   by a log line nobody reads). Now a bad name is a refusal before any port
//!   is bound, not a quiet downgrade.
//! - The named variable is set: bearer auth is actually required end to end,
//!   proving the CLI wiring (env var in, `AppState::with_auth_token` set)
//!   still works, not just the middleware `lifecycle.rs` exercises directly.
//!
//! Each test spawns its own real `salvor` process on an OS-assigned port
//! (never a fixed one, so this can never collide with a developer's own
//! server), exactly like `serve_kill.rs` and `serve_demo_tools.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

/// An address on loopback the OS assigned and then released: enough to name a
/// `--bind` port that is free at spawn time without ever touching a fixed
/// one. Used only by the refusal test, which must prove nothing ends up
/// listening there; the healthy-path test lets `--bind 127.0.0.1:0` pick its
/// own port and reads the real one off the banner, same as the other serve
/// tests in this suite.
fn free_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read local addr");
    drop(listener);
    addr.to_string()
}

/// Spawns `salvor --store <store> serve --bind <addr> --auth-token <env_name>`
/// with the named variable removed from the child's environment (and,
/// optionally, set to `env_value`), capturing stdout and stderr for the
/// caller to inspect after the process exits.
fn spawn_serve_with_auth_token(
    store: &std::path::Path,
    addr: &str,
    env_name: &str,
    env_value: Option<&str>,
) -> Child {
    let mut cmd = Command::new(SALVOR_BIN);
    cmd.args([
        "--store",
        store.to_str().unwrap(),
        "serve",
        "--bind",
        addr,
        "--auth-token",
        env_name,
    ])
    .env("RUST_LOG", "off")
    .env_remove(env_name)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if let Some(value) = env_value {
        cmd.env(env_name, value);
    }
    cmd.spawn().expect("spawn salvor serve")
}

#[test]
fn auth_token_naming_an_unset_variable_refuses_to_start() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let addr = free_addr();

    let child =
        spawn_serve_with_auth_token(&store, &addr, "SALVOR_TEST_UNSET_AUTH_TOKEN_VAR", None);
    let output = child.wait_with_output().expect("wait for salvor serve");

    assert!(
        !output.status.success(),
        "an unset --auth-token variable must exit nonzero: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--auth-token names $SALVOR_TEST_UNSET_AUTH_TOKEN_VAR"),
        "the refusal names the variable: {stderr}"
    );
    assert!(
        stderr.contains("unset or empty"),
        "the refusal says why: {stderr}"
    );
    assert!(
        stderr.contains("export $SALVOR_TEST_UNSET_AUTH_TOKEN_VAR"),
        "the refusal says what to do: {stderr}"
    );
    assert!(
        stderr.contains("drop --auth-token"),
        "the refusal names the other way out: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("listening on"),
        "the refusal must happen before a port is ever bound: {stdout}"
    );

    // Nothing is listening on the address afterward: the refusal happened
    // before `TcpListener::bind` was ever reached.
    assert!(
        TcpStream::connect(&addr).is_err(),
        "nothing should be listening on {addr} after the refusal"
    );
}

#[test]
fn auth_token_naming_an_empty_variable_also_refuses_to_start() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let addr = free_addr();

    let child =
        spawn_serve_with_auth_token(&store, &addr, "SALVOR_TEST_EMPTY_AUTH_TOKEN_VAR", Some(""));
    let output = child.wait_with_output().expect("wait for salvor serve");

    assert!(
        !output.status.success(),
        "an empty --auth-token variable must exit nonzero, same as unset: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--auth-token names $SALVOR_TEST_EMPTY_AUTH_TOKEN_VAR"),
        "the refusal names the variable: {stderr}"
    );
    assert!(
        TcpStream::connect(&addr).is_err(),
        "nothing should be listening on {addr} after the refusal"
    );
}

/// Spawns `salvor serve --auth-token <env_name>` with the variable set to a
/// real token, waits for the "listening on" banner, and returns the child
/// (still running) plus the bound address. Models `serve_demo_tools.rs`'s
/// `spawn_serve`, with the added `--auth-token`/env wiring.
fn spawn_serve_with_token_set(
    store: &std::path::Path,
    env_name: &str,
    env_value: &str,
) -> (Child, String) {
    let mut child = Command::new(SALVOR_BIN)
        .args([
            "--store",
            store.to_str().unwrap(),
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--auth-token",
            env_name,
        ])
        .env("RUST_LOG", "off")
        .env(env_name, env_value)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor serve --auth-token");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let addr = loop {
        line.clear();
        assert!(
            Instant::now() < deadline,
            "salvor serve never printed its banner"
        );
        reader.read_line(&mut line).expect("read banner line");
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("salvor control plane listening on http://") {
            break rest.to_owned();
        }
    };
    // Keep draining stdout for the process's lifetime, same reasoning as
    // `serve_demo_tools.rs`'s `spawn_serve`.
    std::thread::spawn(move || {
        let mut sink = String::new();
        loop {
            sink.clear();
            if reader.read_line(&mut sink).unwrap_or(0) == 0 {
                break;
            }
        }
    });
    (child, addr)
}

/// A minimal blocking HTTP/1.1 GET, with an optional bearer token, over a raw
/// [`TcpStream`]. Returns the status code and the parsed JSON body
/// (`Value::Null` if the body is empty or not JSON). Models
/// `serve_demo_tools.rs`'s `http`, narrowed to GET and given an `auth` slot.
fn get(addr: &str, path: &str, auth: Option<&str>) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let auth_header = auth
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         {auth_header}\
         Connection: close\r\n\
         \r\n"
    )
    .expect("write the request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");
    let text = String::from_utf8_lossy(&raw);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let parsed = serde_json::from_str(rest.trim()).unwrap_or(Value::Null);
    (status, parsed)
}

#[test]
fn auth_token_naming_a_set_variable_requires_the_bearer() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let (mut child, addr) =
        spawn_serve_with_token_set(&store, "SALVOR_TEST_SET_AUTH_TOKEN_VAR", "s3cret-token");

    let (status, body) = get(&addr, "/v1/runs", None);
    assert_eq!(status, 401, "no bearer at all: {body}");
    assert_eq!(body["error"]["code"].as_str(), Some("unauthorized"));

    let (status, body) = get(&addr, "/v1/runs", Some("wrong-token"));
    assert_eq!(status, 401, "wrong bearer: {body}");

    let (status, body) = get(&addr, "/v1/runs", Some("s3cret-token"));
    assert_eq!(
        status, 200,
        "the token read from the env var is honoured: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
