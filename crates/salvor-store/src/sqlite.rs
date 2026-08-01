//! [`SqliteStore`]: the v0.1 [`EventStore`] backend, built on `rusqlite` with
//! a bundled SQLite.
//!
//! One `events` table, primary key `(run_id, seq)`, with the exact envelope
//! wire JSON stored alongside queryable `run_id`, `seq`, and `recorded_at`
//! columns, plus the three columns that carry this run's hash chain. File-
//! backed stores open in WAL mode with `synchronous=FULL` and a busy timeout;
//! an in-memory constructor exists for tests.
//!
//! # Schema
//!
//! ```text
//! events(run_id, seq, recorded_at, envelope, chain_idx, prev_hash, row_hash)
//!     PRIMARY KEY (run_id, seq)
//! chain_heads(run_id PRIMARY KEY, chain_len, head_hash)
//! store_meta(key PRIMARY KEY, value)
//! ```
//!
//! `chain_idx` is the row's position in its run's *append* order, which is the
//! order the chain was built in and the order [`read_log`](SqliteStore::read_log)
//! re-walks it in. `prev_hash` and `row_hash` are the chain values defined in
//! [`crate::chain`], which is the normative spec; nothing about them is
//! SQLite-specific. `chain_heads` holds the one value per run that commits to
//! the whole log, so removing rows from the end is detectable and an external
//! anchor has a single thing to sign. `store_meta` records `schema_version`
//! and the `chain_spec` the rows were hashed under.
//!
//! # Append-only guards
//!
//! Two `BEFORE` triggers on `events` refuse `UPDATE` and `DELETE` outright.
//! They are defense in depth against a casual edit through `sqlite3` or a
//! stray query, and nothing more: anyone who can write the file can also drop
//! a trigger. **The hash chain is the evidence mechanism; the triggers are a
//! locked door on a building with windows.** The one code path that is allowed
//! to write over `events` rows is the legacy backfill in
//! [`SqliteStore::open`], which drops the triggers, migrates, and puts them
//! back.
//!
//! # Blocking posture
//!
//! **The `async` methods on this type do their `rusqlite` work inline, on the
//! calling task, and block while they do it.** `salvor-store` stays
//! executor-agnostic (no `tokio` in its dependencies), so this type does not
//! move that work to a blocking-safe thread pool on your behalf. A caller
//! running on an asynchronous runtime owns that decision: wrap calls in
//! `tokio::task::spawn_blocking` (or the equivalent) if you must not block the
//! async worker. The v0.2 Postgres backend will be genuinely async; keeping
//! every `rusqlite` type out of the public API is what lets this blocking
//! posture change later without a semver break.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use salvor_core::{EventEnvelope, RunId, SequenceNumber};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::chain::{self, ChainHead, ChainRow};
use crate::error::StoreError;
use crate::store::{EventStore, RunSummary};

/// The schema this build writes, recorded in `store_meta`. Version 1 was the
/// four-column `events` table with no chain; version 2 adds the chain columns,
/// `chain_heads`, and this metadata table itself.
const SCHEMA_VERSION: &str = "2";

/// The tables, created on every open and unchanged if they already exist.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
         run_id      TEXT    NOT NULL,
         seq         INTEGER NOT NULL,
         recorded_at INTEGER NOT NULL,
         envelope    TEXT    NOT NULL,
         chain_idx   INTEGER NOT NULL,
         prev_hash   TEXT    NOT NULL,
         row_hash    TEXT    NOT NULL,
         PRIMARY KEY (run_id, seq)
     );
     CREATE TABLE IF NOT EXISTS chain_heads (
         run_id    TEXT    NOT NULL PRIMARY KEY,
         chain_len INTEGER NOT NULL,
         head_hash TEXT    NOT NULL
     );
     CREATE TABLE IF NOT EXISTS store_meta (
         key   TEXT NOT NULL PRIMARY KEY,
         value TEXT NOT NULL
     );";

/// The append-only guards. `RAISE(ABORT)` rolls back the statement and hands
/// the message back to the caller, so an accidental `UPDATE` fails loudly
/// instead of quietly rewriting history.
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

/// Takes the guards off, for the one code path allowed to rewrite `events`
/// rows: the legacy backfill.
const DROP_GUARDS: &str = "DROP TRIGGER IF EXISTS events_refuse_update;
     DROP TRIGGER IF EXISTS events_refuse_delete;";

/// A single-node [`EventStore`] backed by an embedded SQLite database.
///
/// The connection lives behind a [`Mutex`] so the store is `Send + Sync` and
/// usable as `Arc<dyn EventStore>`. A `rusqlite::Connection` is `Send` but not
/// `Sync`, and the mutex serializes access to it. Each method holds the lock
/// only for the duration of its own SQL and never awaits while holding it, so
/// a plain [`std::sync::Mutex`] is the right tool rather than an async one.
///
/// See the module documentation for the blocking posture: the `async` methods
/// run their SQLite work inline.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens (or creates) a file-backed store at `path`.
    ///
    /// Enables WAL journaling for concurrent readers and crash safety, sets
    /// `synchronous=FULL` so a committed append has reached durable storage
    /// before `Ok` (the durability half of the trait contract), and sets a
    /// busy timeout so a briefly locked database waits rather than failing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] if the database cannot be opened,
    /// configured, or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\n\
             PRAGMA synchronous=FULL;\n\
             PRAGMA busy_timeout=5000;",
        )
        .map_err(backend)?;
        Self::init(conn)
    }

    /// Opens a private in-memory store, for tests.
    ///
    /// The database lives only as long as this value and is not shared with
    /// any other connection. WAL and durability pragmas do not apply to an
    /// in-memory database, so they are not set here.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] if the database cannot be opened or
    /// initialized.
    pub fn in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::init(conn)
    }

    /// Brings the database up to the current schema and wraps the connection.
    ///
    /// The primary key `(run_id, seq)` is what enforces the uniqueness half of
    /// the trait contract: SQLite rejects a duplicate with a constraint
    /// violation, which [`append`](Self::append) turns into
    /// [`StoreError::Conflict`]. The `envelope` column holds the exact
    /// serialized wire JSON, which is also exactly what the chain hashes;
    /// `recorded_at` stores the timestamp as an integer count of nanoseconds
    /// since the Unix epoch so that `MIN`/`MAX` order it numerically.
    fn init(conn: Connection) -> Result<Self, StoreError> {
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Creates or updates the schema, backfills the chain of a store written
    /// before the chain existed, and installs the append-only guards.
    ///
    /// # Opening an old store
    ///
    /// A database written by an earlier binary has a four-column `events`
    /// table and no chain at all. This adds the three chain columns, then
    /// walks every row it already holds exactly once, in `(run_id, seq)`
    /// order, computing the chain as if the rows had been appended in that
    /// order. Nothing is deleted and no envelope byte is touched, so a store
    /// created by the previous binary opens, keeps every event, and is
    /// verifiable from then on.
    ///
    /// Backfill was chosen over versioning the table and reading old rows
    /// unverified. A second, chainless table would leave every pre-existing
    /// run permanently outside the guarantee and split the read path in two
    /// forever, for a file format that is barely a release old. Backfill gives
    /// every run one code path and one promise.
    ///
    /// Two honest limits come with that choice, both restated in SECURITY.md.
    /// The backfilled chain attests to the rows **as of the migration**: it
    /// proves nothing changed afterwards and cannot say whether something had
    /// already been changed before. And the original append order of legacy
    /// rows is not recoverable from the old schema, so `(run_id, seq)` is the
    /// documented, deterministic stand-in; the same old file always backfills
    /// to the same hashes.
    ///
    /// The guards come off for the duration of the backfill, because writing
    /// the chain onto existing rows is the one legitimate `UPDATE` this table
    /// ever sees, and go back on before the store is handed out. A database
    /// that needs no migration never has its guards dropped.
    ///
    /// The whole thing, schema changes included, runs in one transaction.
    /// SQLite makes DDL transactional, and this migration needs it: a process
    /// that died between adding the columns and filling them would leave rows
    /// carrying placeholder hashes that no later open would recognize as
    /// unfinished, and the log would read as tampered with forever. All of it
    /// commits or none of it does.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        let legacy = Self::table_exists(conn, "events")?
            && !Self::column_exists(conn, "events", "chain_idx")?;

        let tx = conn.unchecked_transaction().map_err(backend)?;

        if legacy {
            tx.execute_batch(DROP_GUARDS).map_err(backend)?;
            // NOT NULL with a placeholder default is what makes ADD COLUMN
            // legal on a populated table; the backfill below replaces every
            // placeholder with a real chain value before this commits.
            tx.execute_batch(
                "ALTER TABLE events ADD COLUMN chain_idx INTEGER NOT NULL DEFAULT -1;
                 ALTER TABLE events ADD COLUMN prev_hash TEXT    NOT NULL DEFAULT '';
                 ALTER TABLE events ADD COLUMN row_hash  TEXT    NOT NULL DEFAULT '';",
            )
            .map_err(backend)?;
        }

        tx.execute_batch(SCHEMA).map_err(backend)?;

        if legacy {
            Self::backfill_chain(&tx)?;
        }

        tx.execute(
            "INSERT INTO store_meta (key, value) VALUES ('schema_version', ?1), ('chain_spec', ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION, chain::CHAIN_SPEC],
        )
        .map_err(backend)?;

        tx.execute_batch(GUARDS).map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(())
    }

    /// Whether a table of this name exists in the open database.
    fn table_exists(conn: &Connection, table: &str) -> Result<bool, StoreError> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        Ok(found.is_some())
    }

    /// Whether a table already has a column of this name, which is how a
    /// pre-chain database is told apart from a current one.
    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                params![table, column],
                |row| row.get(0),
            )
            .map_err(backend)?;
        Ok(count > 0)
    }

    /// Computes the chain over the rows a pre-chain database already holds.
    ///
    /// Runs inside the caller's migration transaction, never its own, so that
    /// the columns being filled and the values filling them commit together.
    ///
    /// Every row is read before any is written: SQLite makes no promise about
    /// a statement that walks a table while the same connection rewrites it,
    /// and the whole set is bounded by what already fit on disk.
    fn backfill_chain(tx: &Connection) -> Result<(), StoreError> {
        let mut existing: Vec<(String, i64, String)> = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT run_id, seq, envelope FROM events ORDER BY run_id ASC, seq ASC")
                .map_err(backend)?;
            let mapped = stmt
                .query_map(params![], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(backend)?;
            for row in mapped {
                existing.push(row.map_err(backend)?);
            }
        }

        let mut current_run: Option<String> = None;
        let mut prev_hash = chain::GENESIS_PREV_HASH.to_owned();
        let mut chain_idx: i64 = 0;

        for (run_id_text, seq_col, envelope) in existing {
            if current_run.as_deref() != Some(run_id_text.as_str()) {
                current_run = Some(run_id_text.clone());
                prev_hash = chain::GENESIS_PREV_HASH.to_owned();
                chain_idx = 0;
            }

            let run_id = parse_run_id(&run_id_text)?;
            let seq = SequenceNumber::new(seq_to_u64(seq_col)?);
            let row_hash = chain::row_hash(&prev_hash, run_id, seq, &envelope);

            tx.execute(
                "UPDATE events SET chain_idx = ?1, prev_hash = ?2, row_hash = ?3
                 WHERE run_id = ?4 AND seq = ?5",
                params![chain_idx, prev_hash, row_hash, run_id_text, seq_col],
            )
            .map_err(backend)?;
            tx.execute(
                "INSERT INTO chain_heads (run_id, chain_len, head_hash) VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET
                     chain_len = excluded.chain_len, head_hash = excluded.head_hash",
                params![run_id_text, chain_idx + 1, row_hash],
            )
            .map_err(backend)?;

            prev_hash = row_hash;
            chain_idx += 1;
        }

        Ok(())
    }

    /// Locks the connection, turning a poisoned lock into a backend error
    /// rather than a panic.
    fn conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Backend("connection mutex poisoned".to_owned()))
    }
}

#[async_trait]
impl EventStore for SqliteStore {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        let run_id = envelope.run_id;
        let seq = envelope.seq;

        let run_id_text = run_id.as_uuid().to_string();
        let seq_col = i64::try_from(seq.get())
            .map_err(|_| StoreError::Backend("sequence number exceeds i64 range".to_owned()))?;
        let recorded_at = i64::try_from(envelope.recorded_at.unix_timestamp_nanos())
            .map_err(|_| StoreError::Backend("recorded_at timestamp out of range".to_owned()))?;
        let wire_json = serde_json::to_string(envelope)?;

        let mut guard = self.conn()?;
        // The row and the run's new chain head move together or not at all: a
        // committed row whose head was left behind would read back as a
        // tampered log, so this is one transaction rather than two statements.
        let tx = guard.transaction().map_err(backend)?;

        let head: Option<(i64, String)> = tx
            .query_row(
                "SELECT chain_len, head_hash FROM chain_heads WHERE run_id = ?1",
                params![run_id_text],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(backend)?;
        let (chain_idx, prev_hash) = match head {
            Some((len, hash)) => (len, hash),
            None => (0, chain::GENESIS_PREV_HASH.to_owned()),
        };
        // Hashed over the exact bytes about to be stored, so a chain that
        // verifies is a statement about what is on disk.
        let row_hash = chain::row_hash(&prev_hash, run_id, seq, &wire_json);

        let inserted = tx.execute(
            "INSERT INTO events (run_id, seq, recorded_at, envelope, chain_idx, prev_hash, row_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run_id_text,
                seq_col,
                recorded_at,
                wire_json,
                chain_idx,
                prev_hash,
                row_hash
            ],
        );
        match inserted {
            Ok(_) => {}
            // A primary-key collision on (run_id, seq) is the load-bearing
            // conflict: report it as a typed variant, never overwrite. Dropping
            // the transaction unwritten rolls the attempt back whole, so a
            // rejected append leaves the chain exactly where it was.
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                return Err(StoreError::Conflict { run_id, seq });
            }
            Err(e) => return Err(backend(e)),
        }

        tx.execute(
            "INSERT INTO chain_heads (run_id, chain_len, head_hash) VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET
                 chain_len = excluded.chain_len, head_hash = excluded.head_hash",
            params![run_id_text, chain_idx + 1, row_hash],
        )
        .map_err(backend)?;

        tx.commit().map_err(backend)?;
        Ok(())
    }

    /// Reads a run's log, verifying its hash chain before returning anything.
    ///
    /// Rows come back in `chain_idx` order, the order they were appended in
    /// and therefore the order the chain was built in, are checked link by
    /// link against the recorded head (see [`crate::chain`]), and only then
    /// are deserialized and sorted into sequence order for the caller. A run
    /// whose bytes no longer match its chain is refused with
    /// [`StoreError::TamperEvident`] rather than served, including when the
    /// rewritten row is perfectly valid JSON.
    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        let run_id_text = run_id.as_uuid().to_string();

        let guard = self.conn()?;
        let mut stored: Vec<(SequenceNumber, String, String, String)> = Vec::new();
        {
            let mut stmt = guard
                .prepare(
                    "SELECT seq, envelope, prev_hash, row_hash FROM events
                     WHERE run_id = ?1 ORDER BY chain_idx ASC",
                )
                .map_err(backend)?;
            let rows = stmt
                .query_map(params![run_id_text], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(backend)?;
            for row in rows {
                let (seq_col, envelope, prev_hash, row_hash) = row.map_err(backend)?;
                stored.push((
                    SequenceNumber::new(seq_to_u64(seq_col)?),
                    envelope,
                    prev_hash,
                    row_hash,
                ));
            }
        }

        let recorded_head: Option<(i64, String)> = guard
            .query_row(
                "SELECT chain_len, head_hash FROM chain_heads WHERE run_id = ?1",
                params![run_id_text],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(backend)?;

        let rows: Vec<ChainRow<'_>> = stored
            .iter()
            .map(|(seq, envelope, prev_hash, row_hash)| ChainRow {
                seq: *seq,
                envelope_json: envelope,
                prev_hash,
                row_hash,
            })
            .collect();
        let head = match &recorded_head {
            Some((len, hash)) => Some(ChainHead {
                len: seq_to_u64(*len)?,
                hash,
            }),
            // A run with rows always has a head, because `append` writes both
            // in one transaction. A head that has gone missing is someone
            // clearing the way to shorten the log, so it is refused rather
            // than treated as "nothing to check against".
            None if !rows.is_empty() => {
                return Err(StoreError::TamperEvident {
                    run_id,
                    seq: rows[0].seq,
                    expected: "a recorded chain head".to_owned(),
                    found: "none".to_owned(),
                });
            }
            None => None,
        };
        chain::verify(run_id, &rows, head)?;

        let mut log = Vec::with_capacity(rows.len());
        for row in &rows {
            let envelope: EventEnvelope = serde_json::from_str(row.envelope_json)?;
            log.push(envelope);
        }
        log.sort_by_key(|envelope| envelope.seq);
        Ok(log)
    }

    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        let guard = self.conn()?;
        let mut stmt = guard
            .prepare(
                "SELECT run_id, COUNT(*), MIN(recorded_at), MAX(recorded_at)
                 FROM events
                 GROUP BY run_id
                 ORDER BY MIN(recorded_at) ASC",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(backend)?;

        let mut summaries = Vec::new();
        for row in rows {
            let (run_id_text, count, first_nanos, last_nanos) = row.map_err(backend)?;
            summaries.push(RunSummary {
                run_id: parse_run_id(&run_id_text)?,
                event_count: u64::try_from(count).unwrap_or(0),
                first_recorded_at: nanos_to_datetime(first_nanos)?,
                last_recorded_at: nanos_to_datetime(last_nanos)?,
            });
        }
        Ok(summaries)
    }
}

/// Flattens a `rusqlite::Error` into [`StoreError::Backend`], keeping the
/// backend type out of the public API.
fn backend(error: rusqlite::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// Reads a stored `INTEGER` back as the unsigned count it was written from.
///
/// SQLite integers are signed, so a negative value here means the column was
/// written by something other than this store; that is a backend error rather
/// than a panic.
fn seq_to_u64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("stored count is negative: {value}")))
}

/// Parses a stored run id back into a [`RunId`].
fn parse_run_id(text: &str) -> Result<RunId, StoreError> {
    Uuid::parse_str(text)
        .map(RunId::from_uuid)
        .map_err(|e| StoreError::Backend(format!("stored run_id is not a UUID: {e}")))
}

/// Rebuilds a UTC [`OffsetDateTime`] from a nanoseconds-since-epoch count.
fn nanos_to_datetime(nanos: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
        .map_err(|e| StoreError::Backend(format!("stored timestamp out of range: {e}")))
}
