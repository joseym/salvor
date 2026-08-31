//! `salvor resolve` and the reconciliation report, end to end through the real
//! binary.
//!
//! A genuine dangling write (an intent whose completion never persisted) can
//! only be produced by a crash in a microsecond-wide window, which the runtime
//! tests force with a fault-injecting store. Here we seed that exact log state
//! directly (a `RunStarted` followed by an uncompleted `Write` intent) and then
//! drive the binary against it:
//!
//! - `salvor resume` on the seeded run refuses with the reconciliation report,
//!   which now names the recorded intent and the `salvor resolve` command.
//! - `salvor resolve` records the completion by hand and exits 0.
//! - a second `salvor resolve` refuses, because the run is no longer awaiting
//!   reconciliation.
//!
//! No model or MCP server is needed: `resolve` executes nothing and needs no
//! agent, and the reconciliation branch of `resume` returns before any agent is
//! built.

mod common;

use std::path::Path;

use common::salvor;
use predicates::prelude::*;
use salvor_core::{Effect, Event, EventEnvelope, Performer, RunId, SequenceNumber, SettledBy};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

/// Seeds a store at `path` with a run whose log ends at a dangling write
/// intent, and returns its id.
async fn seed_dangling_write(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let events = vec![
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!("publish otters"),
            labels: None,
            driven_by: None,
            caller: None,
        },
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "publish".into(),
            input: json!({"doc": "otters"}),
            effect: Effect::Write,
            idempotency_key: Some("k-otters".into()),
            performed_by: None,
        },
    ];
    for (i, event) in events.into_iter().enumerate() {
        let envelope = EventEnvelope::new(
            run_id,
            SequenceNumber::new(i as u64),
            time::OffsetDateTime::UNIX_EPOCH,
            event,
        );
        store.append(&envelope).await.expect("seed append");
    }
    run_id
}

#[tokio::test]
async fn resume_reports_then_resolve_records_the_completion() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_dangling_write(&store_path).await;
    let uuid = run_id.as_uuid().to_string();

    // `resume` refuses with the enriched reconciliation report. The
    // NeedsReconciliation branch returns before loading the agent, so the path
    // passed to --agent is never read.
    let refusal = salvor(&store_path)
        .args(["resume", &uuid, "--agent", "/does/not/exist.toml"])
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("recorded at:")
                .and(predicate::str::contains("publish"))
                .and(predicate::str::contains("k-otters"))
                .and(predicate::str::contains(format!("salvor resolve {uuid}"))),
        );
    let stdout = String::from_utf8_lossy(&refusal.get_output().stdout);
    assert!(
        common::flatten_wrapped_prose(&stdout).contains("needs reconciliation"),
        "reconciliation refusal: {stdout}"
    );

    // `resolve` records the completion by hand and exits 0. Given `--agent`,
    // its success report prints a COMPLETE resume command (the real path,
    // not a `--agent <FILE>` placeholder), matching what a graph run's own
    // parked report already prints.
    salvor(&store_path)
        .args([
            "resolve",
            &uuid,
            "--output",
            r#"{"published": true}"#,
            "--agent",
            "agents/writer.toml",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("resolved").and(predicate::str::contains(format!(
                "salvor resume {uuid} --store {} --agent agents/writer.toml\n",
                store_path.display()
            ))),
        );

    // Exactly one event was appended, and it is the correlated completion.
    let store = SqliteStore::open(&store_path).expect("store opens");
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 3, "resolve appends exactly one event");
    match &log[2].event {
        Event::ToolCallCompleted {
            seq,
            output,
            settled_by,
            ..
        } => {
            assert_eq!(*seq, SequenceNumber::new(1), "correlates to the intent");
            assert_eq!(*output, json!({"published": true}));
            // The one thing a hand-recorded completion says that an ordinary
            // one does not: a person put this output here, over the run's head,
            // and nothing in this process witnessed the call it closes.
            assert_eq!(
                *settled_by,
                Some(SettledBy::Operator),
                "the CLI's resolve names its settler too"
            );
        }
        other => panic!("expected a tool completion, got {other:?}"),
    }

    // A second resolve refuses: the run is no longer awaiting reconciliation.
    salvor(&store_path)
        .args(["resolve", &uuid, "--output", "1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("does not need reconciliation"));
}

/// Seeds a store at `path` with a client-driven run whose log ends at a dangling
/// write intent, and returns its id.
async fn seed_client_driven_dangling_write(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let events = vec![
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!("publish otters"),
            labels: None,
            driven_by: Some(Performer::Client),
            caller: None,
        },
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "publish".into(),
            input: json!({"doc": "otters"}),
            effect: Effect::Write,
            idempotency_key: Some("k-otters".into()),
            performed_by: None,
        },
    ];
    for (i, event) in events.into_iter().enumerate() {
        let envelope = EventEnvelope::new(
            run_id,
            SequenceNumber::new(i as u64),
            time::OffsetDateTime::UNIX_EPOCH,
            event,
        );
        store.append(&envelope).await.expect("seed append");
    }
    run_id
}

#[tokio::test]
async fn client_driven_resolve_prints_client_pickup_message() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_client_driven_dangling_write(&store_path).await;
    let uuid = run_id.as_uuid().to_string();

    // `resolve` records the completion by hand and exits 0. For a client-driven
    // run, the success report prints the client-pickup message instead of a
    // `salvor resume` command.
    let output = salvor(&store_path)
        .args([
            "resolve",
            &uuid,
            "--output",
            r#"{"published": true}"#,
            "--agent",
            "agents/writer.toml",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let flattened = common::flatten_wrapped_prose(&stdout);
    assert!(
        flattened.contains("this run is client-driven"),
        "should contain client-driven message: {stdout}"
    );
    assert!(
        flattened.contains("its client picks up the resolution on its next drive"),
        "should contain pickup message: {stdout}"
    );
    assert!(
        flattened.contains("for a LangChain thread"),
        "should mention LangChain: {stdout}"
    );
    assert!(
        !stdout.contains("salvor resume"),
        "should not contain salvor resume command for client-driven run: {stdout}"
    );

    // Exactly one event was appended, and it is the correlated completion.
    let store = SqliteStore::open(&store_path).expect("store opens");
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 3, "resolve appends exactly one event");
    match &log[2].event {
        Event::ToolCallCompleted { seq, output, .. } => {
            assert_eq!(*seq, SequenceNumber::new(1), "correlates to the intent");
            assert_eq!(*output, json!({"published": true}));
        }
        other => panic!("expected a tool completion, got {other:?}"),
    }
}
