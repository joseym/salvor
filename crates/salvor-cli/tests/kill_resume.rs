//! The kill test (the demo in miniature): spawn `salvor run`, let it perform a
//! real write through the MCP fixture, `SIGKILL` the process, then
//! `salvor resume` and prove the run completes with zero duplicate side
//! effects.
//!
//! # How the kill is made deterministic
//!
//! An arbitrary `SIGKILL` could land between a write's recorded intent and its
//! completion, which is the one state that (correctly) refuses automatic
//! recovery. To land the kill in a recoverable state every time, the model's
//! final turn is held behind a long delay that a shared flag releases. The
//! timeline is then fixed: the run performs its write (the fixture appends one
//! line to the count file and the runtime records the completion), then blocks
//! in the delayed final model call. The test polls the store until the write's
//! completion is durable, kills the process while it is blocked, releases the
//! gate, and resumes. Recovery replays the completed write (never re-executing
//! it) and re-issues only the unrecorded final model call.
//!
//! The count file is the side-effect ledger: one line per real `record`
//! execution. Exactly one line after resume is the zero-duplicate proof.

mod common;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{GateModel, count_lines, run_salvor, text_response, tool_use_response, write_agent};
use salvor_core::{Event, RunId};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

#[tokio::test]
async fn kill_mid_run_then_resume_completes_with_no_duplicate_write() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    // Hold the final (3-message) turn behind a 30s delay until released.
    let released = Arc::new(AtomicBool::new(false));
    let model = GateModel::mount_gated(
        vec![
            (
                1,
                tool_use_response("tu_write", "record", json!({"line": "otters"}), 100, 20),
            ),
            (3, text_response("published", 150, 30)),
        ],
        Some(3),
        released.clone(),
        Duration::from_secs(30),
    )
    .await;
    let agent = write_agent(dir.path(), &model.uri(), &count_file, "");
    let agent_path = agent.to_str().unwrap().to_owned();

    // Spawn the run as a real child process we can kill.
    let mut child = Command::new(SALVOR_BIN)
        .args(["--store", store_path.to_str().unwrap()])
        .args([
            "run",
            "--agent",
            &agent_path,
            "--input",
            "\"publish otters\"",
        ])
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor run");

    // A read-side handle on the same store (WAL permits concurrent readers).
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteStore::open(&store_path).expect("test store opens"));

    // Discover the run id once RunStarted lands.
    let run_id: RunId = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let summaries = store.list_runs().await.expect("list runs");
            if let Some(summary) = summaries.into_iter().next() {
                break summary.run_id;
            }
            assert!(Instant::now() < deadline, "no run started in time");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    // Wait until the write's completion is durable, so the kill lands in a
    // recoverable state (not a dangling write intent).
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = store.read_log(run_id).await.expect("read log");
            if log
                .iter()
                .any(|env| matches!(env.event, Event::ToolCallCompleted { .. }))
            {
                break;
            }
            assert!(Instant::now() < deadline, "write did not complete in time");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert_eq!(
        count_lines(&count_file),
        1,
        "the write executed once before the kill"
    );

    // SIGKILL the process while it blocks in the delayed final model call.
    child.kill().expect("kill the run process");
    child.wait().expect("reap the killed process");

    // Release the gate so the resumed final model call returns at once.
    released.store(true, Ordering::SeqCst);

    // salvor resume: no --input, so it recovers the crashed run.
    let resume = run_salvor(
        &store_path,
        &[
            "resume",
            &run_id.as_uuid().to_string(),
            "--agent",
            &agent_path,
        ],
    )
    .await;
    let resume_out = String::from_utf8_lossy(&resume.stdout);
    assert!(
        resume.status.success(),
        "resume completes the run: {resume:?}"
    );
    assert!(
        resume_out.contains("published"),
        "final output: {resume_out}"
    );

    // The zero-duplicate proof: the write was never re-executed.
    assert_eq!(
        count_lines(&count_file),
        1,
        "resume must not re-execute the recorded write"
    );
}
