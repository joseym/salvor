//! Live progress: prove `salvor run` streams per-step progress to stderr *as
//! the run drives*, not in one burst after it finishes.
//!
//! Progress emission lives in `salvor-runtime`, which logs one
//! record at each persist. To show it is genuinely live, this test holds the
//! run's final model turn behind a long delay (a gated `wiremock` response)
//! and reads the child's stderr while it is blocked there. By that point the
//! run has already performed and recorded several steps, so their progress
//! lines (through the write's `ToolCallCompleted`) must already be on stderr,
//! while `RunCompleted` (which only fires once the gated final turn returns)
//! must not be. That ordering is only possible if progress streams during the
//! drive; a post-drive walk would print nothing until the whole run returned.
//!
//! # Why this is deterministic
//!
//! The gate keeps the process blocked in the final model call, so there is a
//! wide window in which "several mid-run step lines present, terminal line
//! absent" is the only possible state. The test asserts that state, then kills
//! the still-blocked process, exactly as the sibling kill test does.

mod common;

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{GateModel, count_lines, text_response, tool_use_response, write_agent};
use serde_json::json;
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

/// Drains a child pipe into a shared string on its own OS thread, so the async
/// test can inspect stderr while the process is still running.
fn drain(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let buffer = Arc::new(Mutex::new(String::new()));
    let sink = buffer.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 1024];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    buffer
}

#[tokio::test]
async fn run_streams_progress_to_stderr_during_the_drive() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    // Hold the final (3-message) turn behind a 30s delay, never released, so
    // the process stays blocked while the test inspects stderr.
    let never = Arc::new(AtomicBool::new(false));
    let model = GateModel::mount_gated(
        vec![
            (
                1,
                tool_use_response("tu_write", "record", json!({"line": "otters"}), 100, 20),
            ),
            (3, text_response("published", 150, 30)),
        ],
        Some(3),
        never,
        Duration::from_secs(30),
    )
    .await;
    let agent = write_agent(dir.path(), &model.uri(), &count_file, "");
    let agent_path = agent.to_str().unwrap().to_owned();

    // RUST_LOG=info so the runtime's info-level progress reaches stderr.
    let mut child = Command::new(SALVOR_BIN)
        .args(["--store", store_path.to_str().unwrap()])
        .args([
            "run",
            "--agent",
            &agent_path,
            "--input",
            "\"publish otters\"",
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn salvor run");

    let stderr = drain(child.stderr.take().expect("piped stderr"));

    // Wait until the write's completion has streamed to stderr. The process is
    // now blocked in the gated final model call.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if stderr.lock().unwrap().contains("ToolCallCompleted") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the write's progress line never streamed to stderr: {}",
            stderr.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The proof: several distinct per-step lines are already on stderr while
    // the run is still driving, in order, and the terminal line is not there.
    // A post-drive walk would have emitted nothing yet.
    {
        let captured = stderr.lock().unwrap();
        for step in [
            "RunStarted",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
        ] {
            assert!(
                captured.contains(step),
                "expected mid-run step `{step}` on stderr, got: {captured}"
            );
        }
        assert!(
            !captured.contains("RunCompleted"),
            "RunCompleted must not appear while the final turn is still gated: {captured}"
        );
        assert!(
            captured.contains("seq="),
            "progress lines carry the seq correlation field: {captured}"
        );
    }
    // The write really ran: this is progress for work that actually happened.
    assert_eq!(count_lines(&count_file), 1, "the write executed once");

    // Kill the still-blocked process; the point was made without waiting out
    // the gate.
    child.kill().expect("kill the run process");
    child.wait().expect("reap the killed process");
}
