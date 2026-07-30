//! Parking and refusal UX through the real binary: a run that parks on a
//! budget prints a report plus the exact resume command, and `resume`
//! continues it; a run that needs reconciliation is refused with evidence and
//! a non-zero exit.

mod common;

use common::{GateModel, count_lines, run_salvor, text_response, tool_use_response, write_agent};
use serde_json::json;
use tempfile::tempdir;

use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::{EventStore, SqliteStore};
use time::OffsetDateTime;

fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("run "))
        .expect("run prints its id first")
        .trim()
        .to_owned()
}

/// A steps budget of 1 parks the run after one model call; the report names
/// the crossing and prints the extend command, and `resume` with an extension
/// completes it without re-running the recorded write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_park_reports_and_resume_completes() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    let model = GateModel::mount(vec![
        (
            1,
            tool_use_response("tu_1", "record", json!({"line": "otters"}), 100, 20),
        ),
        (3, text_response("finished", 150, 30)),
    ])
    .await;
    let agent = write_agent(
        dir.path(),
        &model.uri(),
        &count_file,
        "[budgets]\nsteps = 1",
    );
    let agent_path = agent.to_str().unwrap();

    // salvor run: parks on the steps budget after the first model call.
    let run = run_salvor(&store, &["run", "--agent", agent_path, "--input", "\"go\""]).await;
    let run_out = String::from_utf8_lossy(&run.stdout);
    assert!(run.status.success(), "parking is not a failure: {run:?}");
    assert!(
        common::flatten_wrapped_prose(&run_out).contains("parked: budget exceeded (steps)"),
        "parked report: {run_out}"
    );
    assert!(
        run_out.contains("salvor resume") && run_out.contains("\"extend\""),
        "report prints the resume command with an extension: {run_out}"
    );
    let run_id = run_id_from(&run_out);
    assert_eq!(
        count_lines(&count_file),
        1,
        "the write ran once before parking"
    );

    // salvor list: the parked status is visible.
    let list = run_salvor(&store, &["list"]).await;
    assert!(String::from_utf8_lossy(&list.stdout).contains("budget-exceeded"));

    // Resuming without input is refused (the run is parked awaiting input).
    let no_input = run_salvor(&store, &["resume", &run_id, "--agent", agent_path]).await;
    assert!(!no_input.status.success(), "parked resume needs input");
    assert!(String::from_utf8_lossy(&no_input.stderr).contains("--input"));

    // salvor resume with an extension: the run completes, write not re-run.
    let resume = run_salvor(
        &store,
        &[
            "resume",
            &run_id,
            "--agent",
            agent_path,
            "--input",
            "{\"extend\": {\"steps\": 5}}",
        ],
    )
    .await;
    let resume_out = String::from_utf8_lossy(&resume.stdout);
    assert!(resume.status.success(), "resume completes: {resume:?}");
    assert!(
        resume_out.contains("finished"),
        "final output: {resume_out}"
    );
    assert_eq!(
        count_lines(&count_file),
        1,
        "the write is not re-executed on resume"
    );

    // The run now reads as completed.
    let replay = run_salvor(&store, &["replay", &run_id, "--dry-run"]).await;
    assert!(String::from_utf8_lossy(&replay.stdout).contains("completed"));
}

/// A run whose log ends at an uncompleted write intent derives to
/// needs-reconciliation; `resume` refuses it, prints the recorded intent as
/// evidence, and exits non-zero without touching the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_refuses_a_reconciliation_run() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();

    // Build a log by hand that ends at a dangling write intent.
    {
        let store = SqliteStore::open(&store_path).expect("store opens");
        let now = OffsetDateTime::now_utc();
        let started = EventEnvelope::new(
            run_id,
            SequenceNumber::new(0),
            now,
            Event::RunStarted {
                agent_def_hash: "sha256:test".to_owned(),
                input: json!("charge the card"),
                labels: None,
            },
        );
        let write_intent = EventEnvelope::new(
            run_id,
            SequenceNumber::new(1),
            now,
            Event::ToolCallRequested {
                seq: SequenceNumber::new(1),
                tool: "charge_card".to_owned(),
                input: json!({"amount": 4200}),
                effect: Effect::Write,
                idempotency_key: None,
                performed_by: None,
            },
        );
        store.append(&started).await.expect("append started");
        store.append(&write_intent).await.expect("append intent");
    }

    // A minimal (unused) agent file: resume refuses before loading it.
    let agent = dir.path().join("agent.toml");
    std::fs::write(&agent, "model = \"test-model\"\n").expect("write agent");

    let output = run_salvor(
        &store_path,
        &["resume", &uuid, "--agent", agent.to_str().unwrap()],
    )
    .await;
    assert!(
        !output.status.success(),
        "reconciliation refusal exits non-zero"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        common::flatten_wrapped_prose(&stdout).contains("needs reconciliation"),
        "refusal explained: {stdout}"
    );
    assert!(
        stdout.contains("charge_card"),
        "tool named as evidence: {stdout}"
    );
    assert!(stdout.contains("4200"), "input shown as evidence: {stdout}");
}

/// Resuming a completed run reports that and does nothing (exit 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_reports_a_completed_run() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");
    let model = GateModel::mount(vec![(1, text_response("all set", 10, 5))]).await;
    let agent = write_agent(dir.path(), &model.uri(), &count_file, "");
    let agent_path = agent.to_str().unwrap();

    let run = run_salvor(&store, &["run", "--agent", agent_path, "--input", "\"go\""]).await;
    let run_id = run_id_from(&String::from_utf8_lossy(&run.stdout));

    let resume = run_salvor(&store, &["resume", &run_id, "--agent", agent_path]).await;
    assert!(resume.status.success());
    assert!(String::from_utf8_lossy(&resume.stdout).contains("already completed"));
}
