//! `salvor anchor` and `salvor verify`, end to end through the real binary.
//!
//! The anchor exists for one attack the chain cannot see on its own: a writer
//! who opens the database, rewrites a run from its first event, and recomputes
//! every hash and the recorded head, so the store reads back perfectly and
//! says nothing. These tests do exactly that, with the store's own
//! `chain::row_hash`, and then ask `verify` whether the anchor notices.
//!
//! Everything is seeded straight into a store: no model, no MCP server, and no
//! network. The tampering goes around the store the way a real one would, by
//! opening the file again and writing, with the append-only triggers taken off
//! and put back so it is clear the triggers are not what is doing the
//! detecting.

mod common;

use std::path::Path;

use common::salvor;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::chain;
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};
use tempfile::tempdir;

/// The append-only triggers, restated here because the store keeps its copy
/// private. A tampering helper that puts them back afterwards is a helper that
/// leaves the store exactly as a real attacker would.
const GUARDS: &str = "CREATE TRIGGER IF NOT EXISTS events_refuse_update
     BEFORE UPDATE ON events
     BEGIN
         SELECT RAISE(ABORT, 'salvor: events is append-only, UPDATE refused');
     END;
     CREATE TRIGGER IF NOT EXISTS events_refuse_delete
     BEFORE DELETE ON events
     BEGIN
         SELECT RAISE(ABORT, 'salvor: events is append-only, DELETE refused');
     END;";

const DROP_GUARDS: &str = "DROP TRIGGER IF EXISTS events_refuse_update;
     DROP TRIGGER IF EXISTS events_refuse_delete;";

/// Appends `count` events to a fresh run in the store at `path` and returns
/// its id. The first is a `RunStarted` so the log is a plausible run; the rest
/// are cheap distinct payloads.
async fn seed_run(path: &Path, count: u64) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::new();
    for seq in 0..count {
        store
            .append(&envelope(run_id, seq, &format!("event {seq}")))
            .await
            .expect("seed append");
    }
    run_id
}

/// One envelope: `RunStarted` at seq 0, a distinct `RunFailed` after that.
fn envelope(run_id: RunId, seq: u64, tag: &str) -> EventEnvelope {
    let event = if seq == 0 {
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!("anchor me"),
            labels: None,
            driven_by: None,
        }
    } else {
        Event::RunFailed { error: tag.into() }
    };
    EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq),
        time::OffsetDateTime::UNIX_EPOCH,
        event,
    )
}

/// Opens the database directly, with the append-only triggers off, hands the
/// connection to `edit`, and puts the triggers back.
fn tamper(path: &Path, edit: impl FnOnce(&Connection)) {
    let conn = Connection::open(path).expect("a second connection opens");
    conn.execute_batch(DROP_GUARDS).expect("guards come off");
    edit(&conn);
    conn.execute_batch(GUARDS).expect("guards go back on");
}

/// Rebuilds a run's whole chain from whatever envelopes the rows now hold, and
/// moves the recorded head with it, exactly as the store would have if these
/// bytes had been what was appended.
///
/// This is the forgery the hash chain cannot see: afterwards the store's own
/// `read_log` verifies the run without complaint.
fn recompute_chain(conn: &Connection, run_id: RunId) {
    let uuid = run_id.as_uuid().to_string();
    let mut rows: Vec<(i64, i64, String)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT chain_idx, seq, envelope FROM events WHERE run_id = ?1 \
                 ORDER BY chain_idx ASC",
            )
            .expect("prepare");
        let mapped = stmt
            .query_map(params![uuid], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("query");
        for row in mapped {
            rows.push(row.expect("row"));
        }
    }

    let mut prev = chain::GENESIS_PREV_HASH.to_owned();
    for (chain_idx, seq, envelope) in &rows {
        let hash = chain::row_hash(
            &prev,
            run_id,
            SequenceNumber::new(*seq as u64),
            envelope.as_str(),
        );
        conn.execute(
            "UPDATE events SET prev_hash = ?1, row_hash = ?2 WHERE run_id = ?3 AND chain_idx = ?4",
            params![prev, hash, uuid, chain_idx],
        )
        .expect("rewrite the chain columns");
        prev = hash;
    }
    conn.execute(
        "UPDATE chain_heads SET chain_len = ?1, head_hash = ?2 WHERE run_id = ?3",
        params![rows.len() as i64, prev, uuid],
    )
    .expect("move the recorded head");
}

/// Takes an anchor over the store into `anchor.json` beside it, and returns
/// the path.
fn take_anchor(store_path: &Path, dir: &Path) -> std::path::PathBuf {
    let anchor_path = dir.join("anchor.json");
    salvor(store_path)
        .args(["anchor", "--out"])
        .arg(&anchor_path)
        .assert()
        .success();
    anchor_path
}

/// The anchor document as written, parsed.
fn read_anchor(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("anchor file reads"))
        .expect("anchor file is JSON")
}

/// An anchor over two runs has the documented shape, and verifying an
/// untouched store against it passes and names every run.
#[tokio::test]
async fn an_anchor_is_written_and_an_untouched_store_verifies() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let first = seed_run(&store_path, 3).await;
    let second = seed_run(&store_path, 2).await;

    // With no --out the document goes to stdout and the human line to stderr,
    // so a redirect gets the file and nothing else.
    let printed = salvor(&store_path).arg("anchor").assert().success();
    let stdout = String::from_utf8_lossy(&printed.get_output().stdout).into_owned();
    let stderr = String::from_utf8_lossy(&printed.get_output().stderr).into_owned();
    let document: Value = serde_json::from_str(&stdout).expect("stdout is the anchor JSON");
    assert!(
        stderr.contains("anchored 2 run(s)"),
        "the human line counts the runs: {stderr}"
    );

    assert_eq!(document["anchor"], "salvor.anchor.v1");
    assert_eq!(document["chain"], "salvor.chain.v1");
    assert_eq!(document["store"], store_path.display().to_string());
    let taken_at = document["taken_at"].as_str().expect("taken_at is a string");
    assert!(
        taken_at.ends_with('Z') && taken_at.contains('T'),
        "taken_at is RFC 3339 in UTC: {taken_at}"
    );

    let runs = document["runs"].as_array().expect("runs is an array");
    assert_eq!(runs.len(), 2);
    let ids: Vec<&str> = runs.iter().map(|r| r["run"].as_str().unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "runs come out ordered by run id");
    for run in runs {
        let hash = run["hash"].as_str().expect("hash is a string");
        assert_eq!(hash.len(), 64, "a chain hash is 64 hex characters: {hash}");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }
    let lengths: Vec<u64> = runs.iter().map(|r| r["len"].as_u64().unwrap()).collect();
    assert_eq!(lengths.iter().sum::<u64>(), 5, "three events and two");

    let anchor_path = take_anchor(&store_path, dir.path());
    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .success()
        .stdout(
            predicate::str::contains(format!("run {}: intact at 3 event(s)", first.as_uuid()))
                .and(predicate::str::contains(format!(
                    "run {}: intact at 2 event(s)",
                    second.as_uuid()
                )))
                .and(predicate::str::contains(
                    "2 run(s) anchored, 2 intact, 0 new since the anchor",
                )),
        );
}

/// A run that has grown since the anchor is intact: the anchor commits to the
/// prefix it saw, and ordinary appending is not a discrepancy. A run started
/// after the anchor is reported as new and fails nothing.
#[tokio::test]
async fn growth_since_the_anchor_is_intact_and_a_later_run_is_new() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let grown = seed_run(&store_path, 2).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    // Two more events on the anchored run, and a whole run the anchor never
    // saw.
    let store = SqliteStore::open(&store_path).expect("store opens");
    for seq in 2..4 {
        store
            .append(&envelope(grown, seq, &format!("later {seq}")))
            .await
            .expect("append");
    }
    drop(store);
    let later = seed_run(&store_path, 1).await;

    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .success()
        .stdout(
            predicate::str::contains(format!(
                "run {}: intact at 2 event(s), 2 recorded since the anchor",
                grown.as_uuid()
            ))
            .and(predicate::str::contains(format!(
                "run {}: new since the anchor, 1 event(s)",
                later.as_uuid()
            )))
            .and(predicate::str::contains(
                "1 run(s) anchored, 1 intact, 1 new since the anchor",
            )),
        );
}

/// The whole reason the anchor exists: a run rewritten and re-chained so the
/// store verifies it happily, caught because the hash at the anchored length
/// is not the hash the anchor recorded.
#[tokio::test]
async fn a_rewritten_and_rechained_run_is_reported_rewritten() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_run(&store_path, 3).await;
    let anchor_path = take_anchor(&store_path, dir.path());
    let anchored = read_anchor(&anchor_path)["runs"][0]["hash"]
        .as_str()
        .expect("the anchored hash")
        .to_owned();

    // Rewrite the middle event with different, perfectly valid JSON, then
    // rebuild the chain over it exactly as the store would have.
    let forged = serde_json::to_string(&envelope(run_id, 1, "forged")).expect("serialize");
    tamper(&store_path, |conn| {
        conn.execute(
            "UPDATE events SET envelope = ?1 WHERE run_id = ?2 AND seq = 1",
            params![forged, run_id.as_uuid().to_string()],
        )
        .expect("rewrite the envelope");
        recompute_chain(conn, run_id);
    });

    // The store itself is satisfied: the forgery is internally consistent.
    let store = SqliteStore::open(&store_path).expect("store opens");
    let log = store
        .read_log(run_id)
        .await
        .expect("the store reads it back");
    assert_eq!(log.len(), 3, "the rewrite kept the run's length");
    drop(store);

    let refusal = salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains(format!("run {}: rewritten at event 3", run_id.as_uuid()))
                .and(predicate::str::contains(&anchored))
                .and(predicate::str::contains(
                    "1 run(s) anchored, 0 intact, 0 new since the anchor",
                ))
                .and(predicate::str::contains("backup that reads clean")),
        );
    let stdout = String::from_utf8_lossy(&refusal.get_output().stdout).into_owned();
    assert!(
        !stdout.contains("this store holds no event at that position"),
        "the store does hold an event there, with a different hash: {stdout}"
    );
}

/// A run the anchor recorded and the store no longer holds at all.
#[tokio::test]
async fn a_deleted_run_is_reported_missing() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let kept = seed_run(&store_path, 2).await;
    let deleted = seed_run(&store_path, 2).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    tamper(&store_path, |conn| {
        let uuid = deleted.as_uuid().to_string();
        conn.execute("DELETE FROM events WHERE run_id = ?1", params![uuid])
            .expect("delete the rows");
        conn.execute("DELETE FROM chain_heads WHERE run_id = ?1", params![uuid])
            .expect("delete the head");
    });

    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains(format!(
                "run {}: missing. The anchor recorded 2 event(s); this store holds none.",
                deleted.as_uuid()
            ))
            .and(predicate::str::contains(format!(
                "run {}: intact",
                kept.as_uuid()
            )))
            .and(predicate::str::contains(
                "2 run(s) anchored, 1 intact, 0 new since the anchor",
            )),
        );
}

/// A run whose last event was deleted and whose recorded head was moved back
/// to match: internally consistent, and one event short of what was anchored.
#[tokio::test]
async fn a_truncated_run_is_reported_shortened() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_run(&store_path, 3).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    tamper(&store_path, |conn| {
        conn.execute(
            "DELETE FROM events WHERE run_id = ?1 AND chain_idx = 2",
            params![run_id.as_uuid().to_string()],
        )
        .expect("delete the last row");
        recompute_chain(conn, run_id);
    });

    // The store reads the shortened run without complaint: the survivors chain
    // among themselves and the head was moved to match.
    let store = SqliteStore::open(&store_path).expect("store opens");
    assert_eq!(
        store.read_log(run_id).await.expect("reads back").len(),
        2,
        "the truncation is internally consistent"
    );
    drop(store);

    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .code(1)
        .stdout(predicate::str::contains(format!(
            "run {}: shortened. The anchor recorded 3 event(s); this store holds 2.",
            run_id.as_uuid()
        )));
}

/// A row edited and left un-chained: the store refuses its own log, and that
/// refusal is a finding rather than a crash.
#[tokio::test]
async fn a_row_edited_without_rechaining_is_reported_broken() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_run(&store_path, 3).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    let forged = serde_json::to_string(&envelope(run_id, 2, "forged")).expect("serialize");
    tamper(&store_path, |conn| {
        conn.execute(
            "UPDATE events SET envelope = ?1 WHERE run_id = ?2 AND seq = 2",
            params![forged, run_id.as_uuid().to_string()],
        )
        .expect("rewrite the envelope");
    });

    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains(format!(
                "run {}: broken. This store refuses its own log at seq 2",
                run_id.as_uuid()
            ))
            .and(predicate::str::contains("expected")),
        );
}

/// An anchor written under a spec this binary does not know is refused before
/// a single run is read, naming both specs.
#[tokio::test]
async fn an_anchor_under_a_foreign_spec_is_refused() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    seed_run(&store_path, 2).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    // A foreign anchor spec.
    let mut document = read_anchor(&anchor_path);
    document["anchor"] = json!("someone.else.anchor.v1");
    std::fs::write(
        &anchor_path,
        serde_json::to_string_pretty(&document).expect("serialize"),
    )
    .expect("write");
    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("someone.else.anchor.v1")
                .and(predicate::str::contains("salvor.anchor.v1")),
        );

    // A foreign chain spec: the hashes were built under a rule this binary
    // does not implement, so nothing is comparable.
    let mut document = read_anchor(&anchor_path);
    document["anchor"] = json!("salvor.anchor.v1");
    document["chain"] = json!("salvor.chain.v9");
    std::fs::write(
        &anchor_path,
        serde_json::to_string_pretty(&document).expect("serialize"),
    )
    .expect("write");
    salvor(&store_path)
        .args(["verify", "--against"])
        .arg(&anchor_path)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("salvor.chain.v9")
                .and(predicate::str::contains("salvor.chain.v1")),
        );
}

/// `--json` prints the same result as a structured document, with the same
/// exit code.
#[tokio::test]
async fn the_json_result_names_the_same_findings() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");
    let run_id = seed_run(&store_path, 2).await;
    let anchor_path = take_anchor(&store_path, dir.path());

    let passing = salvor(&store_path)
        .args(["verify", "--json", "--against"])
        .arg(&anchor_path)
        .assert()
        .success();
    let result: Value =
        serde_json::from_slice(&passing.get_output().stdout).expect("stdout is JSON");
    assert_eq!(result["verify"], "salvor.verify.v1");
    assert_eq!(result["store"], store_path.display().to_string());
    assert_eq!(result["anchored"], 1);
    assert_eq!(result["intact"], 1);
    assert_eq!(result["new"], 0);
    assert_eq!(result["ok"], true);
    assert_eq!(result["runs"][0]["run"], run_id.as_uuid().to_string());
    assert_eq!(result["runs"][0]["finding"], "intact");
    assert_eq!(result["runs"][0]["anchored_len"], 2);
    assert_eq!(result["runs"][0]["events_since"], 0);

    tamper(&store_path, |conn| {
        let forged = serde_json::to_string(&envelope(run_id, 1, "forged")).expect("serialize");
        conn.execute(
            "UPDATE events SET envelope = ?1 WHERE run_id = ?2 AND seq = 1",
            params![forged, run_id.as_uuid().to_string()],
        )
        .expect("rewrite the envelope");
        recompute_chain(conn, run_id);
    });

    let failing = salvor(&store_path)
        .args(["verify", "--json", "--against"])
        .arg(&anchor_path)
        .assert()
        .code(1);
    let result: Value =
        serde_json::from_slice(&failing.get_output().stdout).expect("stdout is JSON");
    assert_eq!(result["runs"][0]["finding"], "rewritten");
    assert_eq!(result["ok"], false);
    assert_ne!(
        result["runs"][0]["anchored_hash"], result["runs"][0]["found_hash"],
        "the two hashes are what the finding is about"
    );
}
