//! `salvor abandon` and its receipt, end to end through the real binary.
//!
//! Abandonment is the operator's "we do not care about this run anymore" path.
//! It executes nothing and needs no agent, so these tests seed a run's log
//! directly and then drive the binary against it:
//!
//! - `salvor abandon` on a non-terminal run appends the terminal `RunAbandoned`
//!   and exits 0 with the receipt, carrying the recorded reason.
//! - abandoning a run parked at a dangling write is ALLOWED, and the receipt's
//!   honesty line names the write left unresolved.
//! - a second `salvor abandon` refuses, because the run is already terminal.

mod common;

use std::path::Path;

use common::salvor;
use predicates::prelude::*;
use salvor_core::{Effect, Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

/// Seeds a store at `path` with a run head (folding to `running`), returning its id.
async fn seed_running(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let envelope = EventEnvelope::new(
        run_id,
        SequenceNumber::new(0),
        time::OffsetDateTime::UNIX_EPOCH,
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!("research otters"),
            labels: None,
        },
    );
    store.append(&envelope).await.expect("seed append");
    run_id
}

/// Seeds a run whose log ends at a dangling write intent, returning its id.
async fn seed_dangling_write(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let events = vec![
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!("publish otters"),
            labels: None,
        },
        Event::ToolCallRequested {
            seq: SequenceNumber::new(1),
            tool: "publish".into(),
            input: json!({"doc": "otters"}),
            effect: Effect::Write,
            idempotency_key: None,
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
async fn abandon_retires_a_run_then_refuses_a_second() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_running(&store_path).await;
    let uuid = run_id.as_uuid().to_string();

    let receipt = salvor(&store_path)
        .args(["abandon", &uuid, "--reason", "husk is dead forever"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&receipt.get_output().stdout);
    let prose = common::flatten_wrapped_prose(&stdout);
    assert!(
        prose.contains("appended RunAbandoned at seq 1"),
        "abandon receipt: {stdout}"
    );
    assert!(
        prose.contains("Status now abandoned"),
        "abandon receipt: {stdout}"
    );

    // Exactly one terminal event was appended.
    let store = SqliteStore::open(&store_path).expect("store opens");
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(log.len(), 2, "abandon appends exactly one event");
    match &log[1].event {
        Event::RunAbandoned {
            reason,
            unresolved_write,
        } => {
            assert_eq!(reason.as_deref(), Some("husk is dead forever"));
            assert!(unresolved_write.is_none(), "no dangling write to record");
        }
        other => panic!("expected RunAbandoned, got {other:?}"),
    }

    // A second abandon refuses: the run is already terminal.
    salvor(&store_path)
        .args(["abandon", &uuid])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already terminal"));
}

#[tokio::test]
async fn abandon_of_needs_reconciliation_records_the_unresolved_write() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_dangling_write(&store_path).await;
    let uuid = run_id.as_uuid().to_string();

    // Abandoning a needs-reconciliation run is allowed, and the receipt names the
    // write left unresolved rather than claiming it settled.
    let receipt = salvor(&store_path)
        .args(["abandon", &uuid])
        .assert()
        .success()
        .stdout(predicate::str::contains("publish").and(predicate::str::contains("unresolved")));
    let stdout = String::from_utf8_lossy(&receipt.get_output().stdout);
    assert!(
        common::flatten_wrapped_prose(&stdout).contains("Status now abandoned"),
        "abandon receipt: {stdout}"
    );

    let store = SqliteStore::open(&store_path).expect("store opens");
    let log = store.read_log(run_id).await.expect("log reads");
    match &log[2].event {
        Event::RunAbandoned {
            unresolved_write, ..
        } => {
            let write = unresolved_write
                .as_ref()
                .expect("records the dangling write");
            assert_eq!(write.seq, SequenceNumber::new(1));
            assert_eq!(write.tool, "publish");
        }
        other => panic!("expected RunAbandoned, got {other:?}"),
    }
}
