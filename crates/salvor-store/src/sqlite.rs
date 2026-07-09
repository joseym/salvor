//! [`SqliteStore`]: the v0.1 [`EventStore`] backend, built on `rusqlite` with
//! a bundled SQLite.
//!
//! One `events` table, primary key `(run_id, seq)`, with the exact envelope
//! wire JSON stored alongside queryable `run_id`, `seq`, and `recorded_at`
//! columns. File-backed stores open in WAL mode with `synchronous=FULL` and a
//! busy timeout; an in-memory constructor exists for tests.
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
use salvor_core::{EventEnvelope, RunId};
use rusqlite::{Connection, ErrorCode, params};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreError;
use crate::store::{EventStore, RunSummary};

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

    /// Creates the `events` table if it does not exist and wraps the
    /// connection.
    ///
    /// The primary key `(run_id, seq)` is what enforces the uniqueness half of
    /// the trait contract: SQLite rejects a duplicate with a constraint
    /// violation, which [`append`](Self::append) turns into
    /// [`StoreError::Conflict`]. The `envelope` column holds the exact
    /// serialized wire JSON; `recorded_at` stores the timestamp as an integer
    /// count of nanoseconds since the Unix epoch so that `MIN`/`MAX` order it
    /// numerically.
    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 run_id      TEXT    NOT NULL,
                 seq         INTEGER NOT NULL,
                 recorded_at INTEGER NOT NULL,
                 envelope    TEXT    NOT NULL,
                 PRIMARY KEY (run_id, seq)
             );",
        )
        .map_err(backend)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

        let guard = self.conn()?;
        let result = guard.execute(
            "INSERT INTO events (run_id, seq, recorded_at, envelope) VALUES (?1, ?2, ?3, ?4)",
            params![run_id_text, seq_col, recorded_at, wire_json],
        );

        match result {
            Ok(_) => Ok(()),
            // A primary-key collision on (run_id, seq) is the load-bearing
            // conflict: report it as a typed variant, never overwrite.
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Conflict { run_id, seq })
            }
            Err(e) => Err(backend(e)),
        }
    }

    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        let run_id_text = run_id.as_uuid().to_string();

        let guard = self.conn()?;
        let mut stmt = guard
            .prepare("SELECT envelope FROM events WHERE run_id = ?1 ORDER BY seq ASC")
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![run_id_text], |row| row.get::<_, String>(0))
            .map_err(backend)?;

        let mut log = Vec::new();
        for row in rows {
            let wire_json = row.map_err(backend)?;
            let envelope: EventEnvelope = serde_json::from_str(&wire_json)?;
            log.push(envelope);
        }
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
            let uuid = Uuid::parse_str(&run_id_text)
                .map_err(|e| StoreError::Backend(format!("stored run_id is not a UUID: {e}")))?;
            summaries.push(RunSummary {
                run_id: RunId::from_uuid(uuid),
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

/// Rebuilds a UTC [`OffsetDateTime`] from a nanoseconds-since-epoch count.
fn nanos_to_datetime(nanos: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
        .map_err(|e| StoreError::Backend(format!("stored timestamp out of range: {e}")))
}
