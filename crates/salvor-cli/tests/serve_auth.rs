//! `salvor serve --auth-token` and `--token-file`, end to end, exercised
//! through the real CLI flags (not `AppState` directly, which
//! `salvor-server`'s own `lifecycle.rs` and `auth_identity.rs` already
//! cover).
//!
//! The shared secret:
//!
//! - The named variable is unset or empty: `serve` refuses to start. This is
//!   the fail-closed behaviour a typo'd variable name used to defeat silently
//!   (a `tracing::warn!` and an unauthenticated server, distinguishable only
//!   by a log line nobody reads). Now a bad name is a refusal before any port
//!   is bound, not a quiet downgrade.
//! - The named variable holds fewer than 16 bytes: the same refusal, before a
//!   port is bound, naming the floor.
//! - The named variable is set to a real token: bearer auth is actually
//!   required end to end, proving the CLI wiring (env var in,
//!   `AppState::with_auth_token` set) still works, not just the middleware
//!   `lifecycle.rs` exercises directly.
//!
//! The token file:
//!
//! - A file readable by group or other is a refusal before a port is bound.
//! - A named token verifies, and the server logs the name it came in under.
//! - Rewriting the file adds a token and revokes one, both taking effect on
//!   the next request with the same process still serving.
//! - Repeated refusals from one source are logged and held longer each time.
//!
//! Each test spawns its own real `salvor` process on an OS-assigned port
//! (never a fixed one, so this can never collide with a developer's own
//! server), exactly like `serve_kill.rs` and `serve_demo_tools.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
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
    let (mut child, addr) = spawn_serve_with_token_set(
        &store,
        "SALVOR_TEST_SET_AUTH_TOKEN_VAR",
        "s3cret-token-of-real-length",
    );

    let (status, body) = get(&addr, "/v1/runs", None);
    assert_eq!(status, 401, "no bearer at all: {body}");
    assert_eq!(body["error"]["code"].as_str(), Some("unauthorized"));

    let (status, body) = get(&addr, "/v1/runs", Some("wrong-token"));
    assert_eq!(status, 401, "wrong bearer: {body}");

    let (status, body) = get(&addr, "/v1/runs", Some("s3cret-token-of-real-length"));
    assert_eq!(
        status, 200,
        "the token read from the env var is honoured: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn an_auth_token_under_the_entropy_floor_refuses_to_start() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let addr = free_addr();

    let child = spawn_serve_with_auth_token(
        &store,
        &addr,
        "SALVOR_TEST_SHORT_AUTH_TOKEN_VAR",
        Some("hunter2"),
    );
    let output = child.wait_with_output().expect("wait for salvor serve");

    assert!(
        !output.status.success(),
        "a token under the floor must exit nonzero: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--auth-token names $SALVOR_TEST_SHORT_AUTH_TOKEN_VAR"),
        "the refusal names the variable: {stderr}"
    );
    assert!(
        stderr.contains("7 bytes") && stderr.contains("16"),
        "the refusal names the length and the floor: {stderr}"
    );
    assert!(
        !stderr.contains("hunter2"),
        "and never the token itself: {stderr}"
    );
    assert!(
        TcpStream::connect(&addr).is_err(),
        "nothing should be listening on {addr} after the refusal"
    );
}

/// The SHA-256 of `token`, hex, as a token file's `hash` key carries it.
fn hash_of(token: &str) -> String {
    salvor_server::tokens::digest(token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Writes a token file declaring each `(name, token)` pair, at `mode`.
fn write_token_file(path: &Path, entries: &[(&str, &str)], mode: u32) {
    let mut text = String::new();
    for (name, token) in entries {
        text.push_str(&format!(
            "[tokens.{name}]\nhash = \"{}\"\n\n",
            hash_of(token)
        ));
    }
    std::fs::write(path, text).expect("write the token file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
    let _ = mode;
}

/// Spawns `salvor serve --token-file <path>`, waits for the banner, and
/// returns the child, the bound address, and a buffer the child's stderr is
/// drained into so a test can read what the server logged.
///
/// `RUST_LOG` names the debug level for `salvor_server`, which is where the
/// accepted-bearer line carrying the caller name is written; the refusal line
/// is a `WARN` and would arrive at any level.
fn spawn_serve_with_token_file(
    store: &std::path::Path,
    token_file: &Path,
) -> (Child, String, Arc<Mutex<String>>) {
    let mut child = Command::new(SALVOR_BIN)
        .args([
            "--store",
            store.to_str().unwrap(),
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .env("RUST_LOG", "salvor_server=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn salvor serve --token-file");

    let log = Arc::new(Mutex::new(String::new()));
    let sink = log.clone();
    let stderr = child.stderr.take().expect("piped stderr");
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            sink.lock().expect("log lock").push_str(&line);
        }
    });

    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let addr = loop {
        line.clear();
        assert!(
            Instant::now() < deadline,
            "salvor serve never printed its banner; log so far: {}",
            log.lock().expect("log lock")
        );
        reader.read_line(&mut line).expect("read banner line");
        if let Some(rest) = line
            .trim()
            .strip_prefix("salvor control plane listening on http://")
        {
            break rest.to_owned();
        }
    };
    std::thread::spawn(move || {
        let mut sink = String::new();
        loop {
            sink.clear();
            if reader.read_line(&mut sink).unwrap_or(0) == 0 {
                break;
            }
        }
    });
    (child, addr, log)
}

/// Kills a spawned server and waits for it, so no test leaves a process
/// holding a port.
fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads what the server has logged so far.
fn logged(log: &Arc<Mutex<String>>) -> String {
    log.lock().expect("log lock").clone()
}

#[test]
#[cfg(unix)]
fn a_token_file_readable_by_group_refuses_to_start() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let tokens = dir.path().join("tokens.toml");
    let addr = free_addr();
    write_token_file(&tokens, &[("ci", "sv_ci_token")], 0o644);

    let output = Command::new(SALVOR_BIN)
        .args([
            "--store",
            store.to_str().unwrap(),
            "serve",
            "--bind",
            &addr,
            "--token-file",
            tokens.to_str().unwrap(),
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("run salvor serve --token-file");

    assert!(
        !output.status.success(),
        "a group-readable token file must exit nonzero: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0644"),
        "the refusal names the mode: {stderr}"
    );
    assert!(
        stderr.contains("chmod 0600"),
        "the refusal says what to do: {stderr}"
    );
    assert!(
        TcpStream::connect(&addr).is_err(),
        "nothing should be listening on {addr} after the refusal"
    );
}

#[test]
#[cfg(unix)]
fn a_named_token_is_accepted_and_the_caller_name_is_logged() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let tokens = dir.path().join("tokens.toml");
    write_token_file(&tokens, &[("ci", "sv_ci_token")], 0o600);
    let (child, addr, log) = spawn_serve_with_token_file(&store, &tokens);

    let (status, body) = get(&addr, "/v1/runs", Some("sv_ci_token"));
    assert_eq!(status, 200, "the named token verifies: {body}");

    let (status, _) = get(&addr, "/v1/runs", Some("sv_not_a_token"));
    assert_eq!(status, 401, "and nothing else does");

    let text = logged(&log);
    assert!(
        text.contains("caller=ci"),
        "the accepted request is logged under its token name: {text}"
    );
    assert!(
        text.contains("outcome=unknown_token"),
        "and the refusal names its outcome: {text}"
    );
    assert!(
        !text.contains("sv_ci_token") && !text.contains("sv_not_a_token"),
        "and no log line carries a presented token: {text}"
    );
    stop(child);
}

#[test]
#[cfg(unix)]
fn a_rewrite_adds_and_revokes_a_token_with_no_restart() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let tokens = dir.path().join("tokens.toml");
    write_token_file(&tokens, &[("ci", "sv_ci_token")], 0o600);
    let (child, addr, log) = spawn_serve_with_token_file(&store, &tokens);

    assert_eq!(get(&addr, "/v1/runs", Some("sv_ci_token")).0, 200);
    assert_eq!(
        get(&addr, "/v1/runs", Some("sv_ops_token")).0,
        401,
        "a token the file does not declare"
    );

    // Add `ops` alongside `ci`, same process still serving.
    write_token_file(
        &tokens,
        &[("ci", "sv_ci_token"), ("ops", "sv_ops_token")],
        0o600,
    );
    assert_eq!(
        get(&addr, "/v1/runs", Some("sv_ops_token")).0,
        200,
        "an added token verifies with no restart"
    );
    assert_eq!(get(&addr, "/v1/runs", Some("sv_ci_token")).0, 200);

    // Revoke `ci` by rewriting the file without it.
    write_token_file(&tokens, &[("ops", "sv_ops_token")], 0o600);
    assert_eq!(
        get(&addr, "/v1/runs", Some("sv_ci_token")).0,
        401,
        "a revoked token fails closed with no restart"
    );
    assert_eq!(
        get(&addr, "/v1/runs", Some("sv_ops_token")).0,
        200,
        "and the one still declared is unaffected"
    );

    let text = logged(&log);
    assert!(
        text.contains("added=[\"ops\"]"),
        "the reload records what it added: {text}"
    );
    assert!(
        text.contains("removed=[\"ci\"]"),
        "and what it removed: {text}"
    );
    stop(child);
}

#[test]
#[cfg(unix)]
fn repeated_refusals_from_one_source_are_held_longer_each_time() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let tokens = dir.path().join("tokens.toml");
    write_token_file(&tokens, &[("ci", "sv_ci_token")], 0o600);
    let (child, addr, log) = spawn_serve_with_token_file(&store, &tokens);

    let mut elapsed = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let (status, _) = get(&addr, "/v1/runs", Some("sv_wrong"));
        assert_eq!(status, 401);
        elapsed.push(started.elapsed());
    }

    // The delay ordering, not a wall-clock number: the first refusal is held
    // at least the first delay, and the third is held longer than the first.
    assert!(
        elapsed[0] >= Duration::from_millis(100),
        "the first refusal is held: {elapsed:?}"
    );
    assert!(
        elapsed[2] > elapsed[0],
        "and each one after it is held longer: {elapsed:?}"
    );

    // A token that verifies drops the count, so the next mistake starts over.
    assert_eq!(get(&addr, "/v1/runs", Some("sv_ci_token")).0, 200);
    let started = Instant::now();
    assert_eq!(get(&addr, "/v1/runs", Some("sv_wrong")).0, 401);
    let after_success = started.elapsed();
    assert!(
        after_success < elapsed[2],
        "a verified token clears the count: {after_success:?} vs {:?}",
        elapsed[2]
    );

    let text = logged(&log);
    assert!(
        text.contains("delay_ms=100") && text.contains("delay_ms=200"),
        "each refusal logs how long it was held: {text}"
    );
    assert!(
        text.contains("source=127.0.0.1"),
        "and where it came from: {text}"
    );
    stop(child);
}
