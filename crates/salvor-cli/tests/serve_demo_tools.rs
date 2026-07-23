//! `salvor serve --demo-tools`, end to end: the stock, no-flag path stays a
//! genuinely empty tool registry (a graph `tool` node is a clean
//! `unknown_tool`), and `--demo-tools` registers `salvor_cli::demo_tools`'s
//! three tools for real, dispatched through a real graph run over loopback
//! HTTP.
//!
//! No agent node, no model, no MCP server: a one-node `tool` graph is enough
//! to prove the registry wiring, so this stays hermetic and fast. Each test
//! spawns its own real `salvor` process on an OS-assigned port (never a fixed
//! one, so this can never collide with a developer's own server) and tears it
//! down with a direct `child.kill()` on its own captured child, exactly like
//! `serve_kill.rs` and `dev_serve.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

/// Spawns `salvor --store <store> serve --bind 127.0.0.1:0 [--demo-tools]`,
/// waits for the "listening on" banner on its piped stdout, and returns the
/// child (still running) plus the bound address.
fn spawn_serve(store: &std::path::Path, demo_tools: bool) -> (std::process::Child, String) {
    let mut args = vec![
        "--store".to_owned(),
        store.to_str().unwrap().to_owned(),
        "serve".to_owned(),
        "--bind".to_owned(),
        "127.0.0.1:0".to_owned(),
    ];
    if demo_tools {
        args.push("--demo-tools".to_owned());
    }
    let mut child = Command::new(SALVOR_BIN)
        .args(&args)
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor serve");
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
    // Keep draining stdout for the process's lifetime so a later write never
    // blocks on a full, unread pipe (see dev_serve.rs's read_banners for the
    // same reasoning).
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

/// A minimal blocking HTTP/1.1 request over a raw [`TcpStream`]: enough to
/// exercise the control plane's JSON API without a client dependency. Returns
/// the status code and the parsed JSON body (`Value::Null` if the body is
/// empty or not JSON).
fn http(addr: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let payload = body.map(|v| v.to_string()).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {payload}",
        payload.len()
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

/// A one-node graph document: a single `tool` node, no edges, so it runs
/// straight off the run's own input with nothing else to resolve. Used by
/// both tests, so the two paths (refused vs. dispatched) are directly
/// comparable — same document, same tool name, only the registry differs.
fn one_tool_node_graph() -> Value {
    json!({
        "schema_version": 1,
        "nodes": [
            {"kind": "tool", "payload": {"id": "n_lookup", "tool": "lookup_invoice"}},
        ],
        "edges": [],
    })
}

#[test]
fn stock_serve_ships_no_tools_a_graph_tool_node_is_refused() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let (mut child, addr) = spawn_serve(&store, false);

    let (status, submitted) = http(&addr, "POST", "/v1/graphs", Some(&one_tool_node_graph()));
    assert_eq!(
        status, 201,
        "the document itself is well-formed: {submitted}"
    );
    let hash = submitted["graph"].as_str().expect("graph hash in response");

    let (status, refusal) = http(
        &addr,
        "POST",
        "/v1/graph-runs",
        Some(&json!({"graph_hash": hash, "input": {"invoice_id": "inv_1001"}})),
    );
    assert_eq!(
        status, 404,
        "a stock server's empty registry must refuse the tool node: {refusal}"
    );
    assert_eq!(
        refusal["error"]["code"].as_str(),
        Some("unknown_tool"),
        "refused for the right reason: {refusal}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn demo_tools_flag_registers_and_dispatches_the_three_tools() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("ledger.txt");
    let (mut child, addr) = {
        // SALVOR_DEMO_TOOL_LEDGER must be set before the process spawns, so
        // `spawn_serve`'s own Command is built here instead, mirroring its
        // body with one added `.env`.
        let mut cmd = Command::new(SALVOR_BIN);
        cmd.args([
            "--store",
            store.to_str().unwrap(),
            "serve",
            "--bind",
            "127.0.0.1:0",
            "--demo-tools",
        ])
        .env("RUST_LOG", "off")
        .env("SALVOR_DEMO_TOOL_LEDGER", &ledger)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn salvor serve --demo-tools");
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
    };

    let (status, submitted) = http(&addr, "POST", "/v1/graphs", Some(&one_tool_node_graph()));
    assert_eq!(
        status, 201,
        "the document itself is well-formed: {submitted}"
    );
    let hash = submitted["graph"].as_str().expect("graph hash in response");

    let (status, started) = http(
        &addr,
        "POST",
        "/v1/graph-runs",
        Some(&json!({"graph_hash": hash, "input": {"invoice_id": "inv_1001"}})),
    );
    assert_eq!(
        status, 201,
        "--demo-tools registers lookup_invoice: {started}"
    );
    let run = started["run"].as_str().expect("run id in response");

    // The one-node graph has nothing left to drive once its sole tool node
    // exits, so the response above already reflects a completed run — but
    // poll briefly for it anyway rather than assume the exact response shape
    // of a synchronous drive.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut state = String::new();
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let (_, run_body) = http(&addr, "GET", &format!("/v1/runs/{run}"), None);
        state = run_body["status"]["state"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        last = run_body;
        if state == "completed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(state, "completed", "the run must complete: {last}");
    assert_eq!(
        last["status"]["output"]["invoice_id"].as_str(),
        Some("inv_1001"),
        "lookup_invoice's real output reached the run: {last}"
    );
    assert_eq!(
        last["status"]["output"]["amount_usd"].as_f64(),
        Some(128.5),
        "the canned invoice table resolved for real: {last}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
