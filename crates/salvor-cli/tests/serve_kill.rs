//! `salvor serve --kill <pid>`, end to end: spawn a real `salvor serve` on an
//! OS-assigned port, kill it through the new code path, and prove the process
//! actually exited and the port was actually freed.
//!
//! This always targets by the exact pid this test spawned (`--kill <pid>`,
//! never a bare `--kill`), so the test can never discover and kill some other
//! `salvor serve` that happens to be running on the machine (a developer's own
//! server, or another test in the suite). Direct targeting is exactly the
//! code path meant for this: unambiguous, no prompt.

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_by_pid_stops_a_real_serve_process_and_frees_the_port() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");

    // Bind port 0: the OS assigns a free port, so this test never collides
    // with anything else already listening.
    let mut child = Command::new(SALVOR_BIN)
        .args(["--store", store_path.to_str().unwrap()])
        .args(["serve", "--bind", "127.0.0.1:0"])
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor serve");
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");

    // Read the "listening on http://host:port" line off a blocking thread,
    // so a slow-to-print child cannot stall the async test body.
    let listening_line = tokio::task::spawn_blocking(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read the listening line");
        line
    })
    .await
    .expect("blocking read joins");
    let addr = listening_line
        .trim()
        .strip_prefix("salvor control plane listening on http://")
        .unwrap_or_else(|| panic!("unexpected serve banner: {listening_line:?}"))
        .to_owned();

    // A second, independent `salvor` invocation kills the first by its exact
    // pid: the direct-target path, so this can never touch any other process.
    let pid_arg = pid.to_string();
    let kill = tokio::task::spawn_blocking(move || {
        Command::new(SALVOR_BIN)
            .args(["serve", "--kill", &pid_arg])
            .env("RUST_LOG", "off")
            .output()
            .expect("run salvor serve --kill")
    })
    .await
    .expect("blocking task joins");

    assert!(
        kill.status.success(),
        "salvor serve --kill exits 0: {kill:?}"
    );
    let kill_stdout = String::from_utf8_lossy(&kill.stdout);
    assert!(
        kill_stdout.contains(&format!("killed pid {pid}")),
        "kill output names the pid: {kill_stdout}"
    );
    // The bind shown is what was recovered from argv (the literal `--bind
    // 127.0.0.1:0` passed above), not the OS-resolved port from the banner:
    // discovery reads argv, never the live socket.
    assert!(
        kill_stdout.contains("bind 127.0.0.1:0"),
        "kill output names the argv bind: {kill_stdout}"
    );

    // The killed process actually exited.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the killed salvor serve process did not exit in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The port was actually freed: a fresh listener can bind the same
    // address (macOS/Linux both release a loopback port promptly once the
    // owning process has exited).
    let rebound = TcpListener::bind(&addr);
    assert!(
        rebound.is_ok(),
        "port {addr} should be free after the kill: {rebound:?}"
    );
}
