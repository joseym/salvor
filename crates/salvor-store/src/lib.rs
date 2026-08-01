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
//!   `(run_id, seq)` collision and a distinct [`StoreError::TamperEvident`]
//!   variant for a log whose recorded bytes no longer match their chain.
//! - [`chain`] is the per-run hash chain: the normative definition of what is
//!   hashed, and the two functions every backend builds and verifies its rows
//!   with. It is public because the guarantee belongs to the trait, not to
//!   SQLite, so a Postgres or DynamoDB backend chains its rows with the same
//!   bytes and arrives at the same head hash.
//!
//! # Tamper evidence
//!
//! "Append-only" describes the code that writes the log, not the bytes on
//! disk: anything that can open the database can rewrite a row, and a rewrite
//! that is still valid JSON parses like any other. So each recorded row also
//! carries the hash of the row before it, and
//! [`read_log`](EventStore::read_log) recomputes the whole chain before
//! returning a single event. A modified, reordered, inserted, or removed row
//! is refused with [`StoreError::TamperEvident`] naming the run and position.
//! See the [`chain`] module for what that does and does not prove, and
//! SECURITY.md for the posture around it.

pub mod chain;
mod error;
mod sqlite;
mod store;

pub use error::StoreError;
pub use sqlite::SqliteStore;
pub use store::{EventStore, RunSummary};
