//! Salvor store: the [`EventStore`] trait and its SQLite (WAL mode)
//! implementation. Single-node is the v0.1 posture; the trait is the seam
//! for a Postgres backend in v0.2.
//!
//! A run is an append-only event log and nothing else is state, so this crate
//! is small on purpose: a durable place to [`append`](EventStore::append) one
//! event, [`read`](EventStore::read_log) a run's log back in order, and
//! [`list`](EventStore::list_runs) the runs it holds.
//!
//! # Layout
//!
//! - [`EventStore`] and [`RunSummary`] (module `store`) are the storage seam:
//!   the trait a future Postgres backend implements, plus the summary its
//!   `list_runs` returns. The trait's rustdoc is the implementor contract.
//! - [`SqliteStore`] (module `sqlite`) is the v0.1 backend. No `rusqlite` type
//!   appears in the public API, so the backend can change without a semver
//!   break.
//! - [`StoreError`] (module `error`) is the one error type every method
//!   returns, with a distinct [`StoreError::Conflict`] variant for a
//!   `(run_id, seq)` collision.

mod error;
mod sqlite;
mod store;

pub use error::StoreError;
pub use sqlite::SqliteStore;
pub use store::{EventStore, RunSummary};
