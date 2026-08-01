//! Integration tests for the `salvor-store` public API.
//!
//! Most of the contract is proven by consuming the store-agnostic
//! `salvor-store-conformance` kit: `SqliteStore` is its first consumer, exercised
//! both in-memory and file-backed. What stays here is what the generic kit
//! deliberately excludes because it is SQLite's own: the append overhead
//! measurement, the append-only triggers, and opening a database written before
//! the hash chain existed.
//!
//! Driving `SqliteStore` through the kit is itself a test that the trait is
//! object-safe and async-usable, since the kit only ever names the public
//! `EventStore` surface, never `rusqlite`.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use rusqlite::Connection;
use salvor_core::{Event, EventEnvelope, RunId, RunSummary, SequenceNumber};
use salvor_store::{EventStore, SqliteStore, StoreError};
use salvor_store_conformance::TamperHarness;
use time::OffsetDateTime;

/// `SqliteStore` plus a second connection straight to the same database: the
/// attacker the conformance kit needs to prove tamper evidence with.
///
/// The store has no method that modifies a recorded event, which is the whole
/// point, so the forgery goes around it exactly the way a real one would: open
/// the file again and write. This type is test-only and never ships.
struct SqliteHarness {
    store: SqliteStore,
    /// Also what keeps a shared-cache in-memory database alive for as long as
    /// the harness exists.
    attacker: Mutex<Connection>,
}

/// The triggers, restated here because the store keeps its copy private. A test
/// that puts them back after a forgery is a test that the guards are not what
/// is doing the detecting.
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

impl SqliteHarness {
    /// A harness over a real file-backed WAL database.
    fn file_backed(path: &Path) -> Self {
        let store = SqliteStore::open(path).expect("open file-backed store");
        let attacker = Connection::open(path).expect("open a second raw connection");
        Self {
            store,
            attacker: Mutex::new(attacker),
        }
    }

    /// A harness over an in-memory database that two connections can both see.
    ///
    /// `SqliteStore::in_memory` opens a private database nothing else can
    /// reach, which is right for the store and useless for an attacker, so this
    /// uses a shared-cache memory URI instead. The name is unique per harness,
    /// so parallel tests do not share one database, and the attacker connection
    /// is opened first and held: a shared-cache memory database lives only as
    /// long as some connection to it does.
    fn in_memory() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "file:salvor-conformance-{}-{}?mode=memory&cache=shared",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let attacker = Connection::open(&name).expect("open the shared in-memory database");
        let store = SqliteStore::open(&name).expect("open in-memory store");
        Self {
            store,
            attacker: Mutex::new(attacker),
        }
    }
}

#[async_trait]
impl EventStore for SqliteHarness {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        self.store.append(envelope).await
    }

    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        self.store.read_log(run_id).await
    }

    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        self.store.list_runs().await
    }

    async fn claim_call(
        &self,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<salvor_store::CallClaim, StoreError> {
        self.store.claim_call(claimant).await
    }

    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<salvor_store::CallCommitment>, StoreError> {
        self.store.lookup_call(tool, idempotency_key).await
    }

    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        self.store.append_settling_call(envelope, claimant).await
    }
}

#[async_trait]
impl TamperHarness for SqliteHarness {
    async fn forge_recorded_envelope(
        &self,
        run_id: RunId,
        seq: SequenceNumber,
        envelope_json: &str,
    ) {
        let conn = self.attacker.lock().expect("attacker connection lock");
        // Dropping the triggers is the first thing any real attacker does, and
        // is why they are documented as a locked door rather than as the
        // evidence. They go back on afterwards so the read that follows is
        // caught by the chain and nothing else.
        conn.execute_batch(DROP_GUARDS).expect("drop the guards");
        let changed = conn
            .execute(
                "UPDATE events SET envelope = ?1 WHERE run_id = ?2 AND seq = ?3",
                rusqlite::params![
                    envelope_json,
                    run_id.as_uuid().to_string(),
                    i64::try_from(seq.get()).expect("seq in range")
                ],
            )
            .expect("forge the row");
        assert_eq!(changed, 1, "the forgery must land on exactly one row");
        conn.execute_batch(GUARDS).expect("put the guards back");
    }
}

/// The full conformance kit against an in-memory store, one `#[tokio::test]`
/// per check so a failure names the exact contract clause that broke.
mod in_memory {
    use super::SqliteHarness;

    salvor_store_conformance::conformance_tests!(SqliteHarness::in_memory());
}

/// The full conformance kit against real file-backed WAL databases, the posture
/// where durability and locking actually cost something. A single temporary
/// directory outlives every check; each check gets its own database file inside
/// it, and the directory is removed when the test ends. The multi-threaded
/// runtime lets the kit's concurrency check contend for real.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_kit_file_backed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let counter = AtomicU64::new(0);
    salvor_store_conformance::run_all(|| {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.path().join(format!("events-{n}.db"));
        async move { SqliteHarness::file_backed(&path) }
    })
    .await;
}

/// The kit drives a shared-cache in-memory database, because its attacker needs
/// a second connection into the same one. `SqliteStore::in_memory` opens a
/// genuinely private database instead, so it gets its own small round trip here
/// rather than going uncovered.
#[tokio::test]
async fn a_private_in_memory_store_chains_and_reads_back() {
    let store = SqliteStore::in_memory().expect("open private in-memory store");
    let run = RunId::new();
    for seq in 1..=3 {
        store
            .append(&envelope(run, seq, fail(&format!("e{seq}"))))
            .await
            .expect("append");
    }
    let log = store.read_log(run).await.expect("chain verifies");
    assert_eq!(errors(&log), vec!["e1", "e2", "e3"]);
}

/// SQLite-specific, so it stays out of the store-agnostic kit: against a
/// file-backed WAL store, appending at least 100 events keeps the mean
/// per-append under 5ms. Prints the observed mean; see it with
/// `cargo test -- --nocapture`.
#[tokio::test]
async fn append_overhead_stays_under_five_ms() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open file-backed store");
    let run = RunId::new();

    const APPENDS: u64 = 200;
    let start = Instant::now();
    for seq in 1..=APPENDS {
        store
            .append(&envelope(run, seq, fail("bench")))
            .await
            .expect("append");
    }
    let mean = start.elapsed() / APPENDS as u32;

    println!(
        "append overhead: {APPENDS} appends, mean {:.3} ms per append",
        mean.as_secs_f64() * 1000.0
    );
    assert!(
        mean.as_secs_f64() * 1000.0 < 5.0,
        "mean append {:.3} ms exceeded the 5ms target",
        mean.as_secs_f64() * 1000.0
    );
}

/// Defense in depth: an `UPDATE` against the events table is refused by the
/// trigger, so a casual edit through `sqlite3` fails loudly instead of quietly
/// rewriting history. The row is still there and still readable afterwards.
#[tokio::test]
async fn sqlite_refuses_an_update_of_a_recorded_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open store");
    let run = RunId::new();
    store
        .append(&envelope(run, 1, fail("recorded")))
        .await
        .expect("append");

    let raw = Connection::open(&path).expect("raw connection");
    let refused = raw
        .execute(
            "UPDATE events SET envelope = ?1 WHERE run_id = ?2",
            rusqlite::params!["{}", run.as_uuid().to_string()],
        )
        .expect_err("the events table must refuse an UPDATE");
    assert!(
        refused.to_string().contains("append-only"),
        "the refusal should say why: {refused}"
    );

    let log = store.read_log(run).await.expect("log still reads");
    assert_eq!(errors(&log), vec!["recorded"], "the row is untouched");
}

/// The other half of the guard: a `DELETE` is refused too, so history cannot be
/// quietly shortened by hand.
#[tokio::test]
async fn sqlite_refuses_a_delete_of_a_recorded_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open store");
    let run = RunId::new();
    store
        .append(&envelope(run, 1, fail("recorded")))
        .await
        .expect("append");

    let raw = Connection::open(&path).expect("raw connection");
    let refused = raw
        .execute(
            "DELETE FROM events WHERE run_id = ?1",
            rusqlite::params![run.as_uuid().to_string()],
        )
        .expect_err("the events table must refuse a DELETE");
    assert!(
        refused.to_string().contains("append-only"),
        "the refusal should say why: {refused}"
    );

    let log = store.read_log(run).await.expect("log still reads");
    assert_eq!(errors(&log), vec!["recorded"], "the row is still there");
}

/// The guards can be dropped by anyone holding the file, so the chain has to
/// carry the weight: with the triggers removed and the last event deleted, the
/// surviving rows still chain to each other perfectly, and the recorded head is
/// what catches the removal.
#[tokio::test]
async fn a_deleted_trailing_event_is_caught_by_the_recorded_head() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open store");
    let run = RunId::new();
    for seq in 1..=3 {
        store
            .append(&envelope(run, seq, fail("recorded")))
            .await
            .expect("append");
    }

    {
        let raw = Connection::open(&path).expect("raw connection");
        raw.execute_batch(DROP_GUARDS).expect("drop the guards");
        let removed = raw
            .execute(
                "DELETE FROM events WHERE run_id = ?1 AND seq = 3",
                rusqlite::params![run.as_uuid().to_string()],
            )
            .expect("delete the last event with the guards off");
        assert_eq!(removed, 1);
        raw.execute_batch(GUARDS).expect("put the guards back");
    }

    match store.read_log(run).await {
        Err(StoreError::TamperEvident { run_id, seq, .. }) => {
            assert_eq!(run_id, run);
            assert_eq!(
                seq,
                SequenceNumber::new(2),
                "the last surviving row is where the chain stops agreeing"
            );
        }
        other => panic!("expected TamperEvident, got {other:?}"),
    }
}

/// The recorded head is not optional evidence: deleting a run's `chain_heads`
/// row, which no trigger protects because `append` legitimately rewrites it, is
/// itself refused rather than read as "nothing to check against".
#[tokio::test]
async fn a_missing_chain_head_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open store");
    let run = RunId::new();
    for seq in 1..=2 {
        store
            .append(&envelope(run, seq, fail("recorded")))
            .await
            .expect("append");
    }

    {
        let raw = Connection::open(&path).expect("raw connection");
        let removed = raw
            .execute(
                "DELETE FROM chain_heads WHERE run_id = ?1",
                rusqlite::params![run.as_uuid().to_string()],
            )
            .expect("delete the head");
        assert_eq!(removed, 1);
    }

    match store.read_log(run).await {
        Err(StoreError::TamperEvident { run_id, .. }) => assert_eq!(run_id, run),
        other => panic!("expected TamperEvident, got {other:?}"),
    }
}

/// A database written by the previous binary, which had no chain at all, opens
/// without losing anything: every event reads back, the chain is backfilled
/// deterministically, appends continue the chain from there, and a forgery
/// against a migrated row is caught like any other.
///
/// The old store is built here with raw SQL against the exact pre-chain schema,
/// so this is a real old file rather than a simulation of one.
#[tokio::test]
async fn a_store_written_before_the_chain_opens_reads_and_verifies() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("legacy.db");

    let run_a = RunId::new();
    let run_b = RunId::new();
    let legacy: Vec<EventEnvelope> = vec![
        envelope(run_a, 1, fail("a1")),
        envelope(run_a, 2, fail("a2")),
        envelope(run_a, 3, fail("a3")),
        envelope(run_b, 1, fail("b1")),
    ];

    {
        // Verbatim the schema `SqliteStore::init` created before the chain
        // existed: four columns, no triggers, no metadata table.
        let old = Connection::open(&path).expect("create the old database");
        old.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 run_id      TEXT    NOT NULL,
                 seq         INTEGER NOT NULL,
                 recorded_at INTEGER NOT NULL,
                 envelope    TEXT    NOT NULL,
                 PRIMARY KEY (run_id, seq)
             );",
        )
        .expect("old schema");
        for env in &legacy {
            old.execute(
                "INSERT INTO events (run_id, seq, recorded_at, envelope) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    env.run_id.as_uuid().to_string(),
                    i64::try_from(env.seq.get()).expect("seq in range"),
                    i64::try_from(env.recorded_at.unix_timestamp_nanos()).expect("ts in range"),
                    serde_json::to_string(env).expect("serialize"),
                ],
            )
            .expect("insert an old row");
        }
    }

    let store = SqliteStore::open(&path).expect("the old store opens");

    {
        // The migrated file says which rule its hashes were built under, so a
        // future chain version is a visible fact about the database rather
        // than something a reader has to infer.
        let raw = Connection::open(&path).expect("raw connection");
        let version: String = raw
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version recorded");
        let spec: String = raw
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'chain_spec'",
                [],
                |row| row.get(0),
            )
            .expect("chain_spec recorded");
        assert_eq!(version, "3");
        assert_eq!(spec, "salvor.chain.v1");
    }

    let log_a = store.read_log(run_a).await.expect("old run a reads");
    let log_b = store.read_log(run_b).await.expect("old run b reads");
    assert_eq!(log_a, legacy[..3], "no event was lost or altered");
    assert_eq!(log_b, legacy[3..], "the second run survived too");
    assert_eq!(store.list_runs().await.expect("list").len(), 2);

    // The backfill is deterministic: opening the same file again recomputes
    // nothing and still verifies.
    drop(store);
    let store = SqliteStore::open(&path).expect("reopen");
    assert_eq!(store.read_log(run_a).await.expect("reread"), legacy[..3]);

    // Appends land on top of the backfilled chain.
    store
        .append(&envelope(run_a, 4, fail("a4")))
        .await
        .expect("append after migration");
    let log_a = store.read_log(run_a).await.expect("read after append");
    assert_eq!(errors(&log_a), vec!["a1", "a2", "a3", "a4"]);

    // And a migrated row is now as tamper-evident as a freshly written one.
    let harness = SqliteHarness::file_backed(&path);
    let forged = serde_json::to_string(&envelope(run_a, 2, fail("forged"))).expect("serialize");
    harness
        .forge_recorded_envelope(run_a, SequenceNumber::new(2), &forged)
        .await;
    match harness.read_log(run_a).await {
        Err(StoreError::TamperEvident { run_id, seq, .. }) => {
            assert_eq!(run_id, run_a);
            assert_eq!(seq, SequenceNumber::new(2));
        }
        other => panic!("expected TamperEvident on a migrated row, got {other:?}"),
    }
}

/// Wraps a payload in an envelope for `run` at `seq`, timestamped `seq` seconds
/// after the epoch so ordering is easy to reason about.
fn envelope(run: RunId, seq: u64, event: Event) -> EventEnvelope {
    let recorded_at =
        OffsetDateTime::from_unix_timestamp(1_000_000 + seq as i64).expect("timestamp in range");
    EventEnvelope::new(run, SequenceNumber::new(seq), recorded_at, event)
}

/// A cheap distinct payload: a `RunFailed` whose error string tags the event.
fn fail(tag: &str) -> Event {
    Event::RunFailed { error: tag.into() }
}

/// The `RunFailed` error tags of a log, in order.
fn errors(log: &[EventEnvelope]) -> Vec<String> {
    log.iter()
        .map(|e| match &e.event {
            Event::RunFailed { error } => error.clone(),
            other => panic!("expected RunFailed, got {other:?}"),
        })
        .collect()
}
