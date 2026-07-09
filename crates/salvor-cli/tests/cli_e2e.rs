//! End to end through the real `salvor` binary: a multi-step run against a
//! scripted model and the counting MCP fixture, then `list`, `history`, and
//! `replay --dry-run` asserting on substance (status, kinds, usage), not just
//! exit codes.

mod common;

use common::{GateModel, count_lines, run_salvor, text_response, tool_use_response, write_agent};
use serde_json::{Value, json};
use tempfile::tempdir;

/// Pulls the run id out of `run`'s first stdout line (`run <uuid>`).
fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("run "))
        .expect("run prints its id first")
        .trim()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_then_list_history_replay() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    // Turn 1 (1 message): call `record`. Turn 2 (3 messages): final text.
    let model = GateModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
        ),
        (3, text_response("all done", 150, 30)),
    ])
    .await;

    let agent = write_agent(dir.path(), &model.uri(), &count_file, "");

    // salvor run
    let output = run_salvor(
        &store,
        &[
            "run",
            "--agent",
            agent.to_str().unwrap(),
            "--input",
            "\"research otters\"",
        ],
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "run exits 0: {output:?}");
    assert!(
        stdout.contains("all done"),
        "final output printed: {stdout}"
    );
    let run_id = run_id_from(&stdout);
    assert_eq!(count_lines(&count_file), 1, "record executed exactly once");

    // salvor list: the run shows as completed with its event count.
    let list = run_salvor(&store, &["list"]).await;
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success());
    assert!(list_out.contains(&run_id), "list names the run: {list_out}");
    assert!(
        list_out.contains("completed"),
        "list shows status: {list_out}"
    );

    // salvor history: the kinds and the tool's name and effect are visible.
    let history = run_salvor(&store, &["history", &run_id]).await;
    let history_out = String::from_utf8_lossy(&history.stdout);
    assert!(history.status.success());
    for expected in [
        "RunStarted",
        "ModelCallRequested",
        "ModelCallCompleted",
        "ToolCallRequested",
        "record",
        "Write",
        "ToolCallCompleted",
        "RunCompleted",
    ] {
        assert!(
            history_out.contains(expected),
            "history should contain `{expected}`: {history_out}"
        );
    }

    // salvor history --json: a parseable array of envelopes.
    let history_json = run_salvor(&store, &["history", &run_id, "--json"]).await;
    let json_out = String::from_utf8_lossy(&history_json.stdout);
    let parsed: Value = serde_json::from_str(&json_out).expect("history --json is JSON");
    assert!(parsed.is_array(), "history --json is an array");
    assert!(json_out.contains("ModelCallCompleted"));

    // salvor replay --dry-run: the folded state, with usage totals.
    let replay = run_salvor(&store, &["replay", &run_id, "--dry-run"]).await;
    let replay_out = String::from_utf8_lossy(&replay.stdout);
    assert!(replay.status.success());
    assert!(
        replay_out.contains("completed"),
        "replay status: {replay_out}"
    );
    // Usage is 100+150 in, 20+30 out (the scripted numbers).
    assert!(
        replay_out.contains("250"),
        "input token total: {replay_out}"
    );
    assert!(
        replay_out.contains("50"),
        "output token total: {replay_out}"
    );
    assert!(replay_out.contains("pending:     none"));
}

/// Progress reaches stderr with run-id and seq correlation fields, and stdout
/// stays clean (only the id line and the final output).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_emits_progress_on_stderr() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");
    let model = GateModel::mount(vec![(1, text_response("done", 10, 5))]).await;
    let agent = write_agent(dir.path(), &model.uri(), &count_file, "");

    let store2 = store.clone();
    let agent_path = agent.to_str().unwrap().to_owned();
    // RUST_LOG=info so the progress events are shown.
    let output = tokio::task::spawn_blocking(move || {
        let mut command = assert_cmd::Command::cargo_bin("salvor").expect("salvor binary builds");
        command
            .arg("--store")
            .arg(&store2)
            .env("RUST_LOG", "info")
            .args(["run", "--agent", &agent_path, "--input", "\"hi\""]);
        command.output().expect("salvor runs")
    })
    .await
    .expect("join");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("starting run"),
        "lifecycle logged: {stderr}"
    );
    assert!(
        stderr.contains("seq="),
        "seq correlation field present: {stderr}"
    );
    assert!(
        stderr.contains("RunStarted"),
        "per-event progress: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("run "));
    assert!(stdout.contains("done"));
}
