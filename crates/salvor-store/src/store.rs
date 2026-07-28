//! The [`EventStore`] trait: the storage seam every backend implements, plus
//! the [`RunSummary`] projection [`EventStore::list_runs`] returns.
//!
//! This trait is the specification a future Postgres backend is written
//! against, so its contract is stated in prose here, not left implicit in the
//! SQLite code that happens to satisfy it today.
//!
//! [`RunSummary`] itself is defined one layer down, in the pure `salvor-replay`
//! crate that `salvor-core` re-exports, and is re-exported from here so every
//! `salvor_store::RunSummary` path keeps resolving. It is a plain value (an id,
//! a count, two timestamps) with no storage machinery behind it, so a consumer
//! that only renders a run listing can name it without linking SQLite.

use async_trait::async_trait;
use salvor_core::{EventEnvelope, RunId};

use crate::error::StoreError;

pub use salvor_core::RunSummary;

/// Durable, append-only storage for run event logs.
///
/// This is the seam that lets the backend vary: v0.1 ships one implementation
/// ([`SqliteStore`](crate::SqliteStore)); v0.2 adds a Postgres backend behind
/// the same trait. The trait is written to be consumed as
/// `Arc<dyn EventStore>` from asynchronous code, so the runtime can hold a
/// store handle without knowing which backend is behind it. That is why the
/// methods are `async` and why the trait requires `Send + Sync`: a handle
/// shared across tasks and threads needs both.
///
/// The methods use `async fn` in a trait via the `async-trait` crate, which
/// rewrites each one to return a `Pin<Box<dyn Future + Send>>`. That boxing is
/// what keeps the trait object-safe: a native `async fn` in a trait is not
/// dyn-compatible today, so `Arc<dyn EventStore>` would not compile without
/// it. The boxed future is the pragmatic cost of the `dyn` handle the runtime
/// design calls for.
///
/// # Implementor contract
///
/// This is the specification the Postgres backend must also satisfy. An
/// implementation is correct only if all of the following hold:
///
/// - **`(run_id, seq)` uniqueness.** At most one event exists at a given
///   `(run_id, seq)`. A second [`append`](Self::append) at an occupied
///   position must return [`StoreError::Conflict`], never overwrite and never
///   panic. This is what makes a duplicated append detectable during
///   crash recovery.
/// - **Ordering.** [`read_log`](Self::read_log) returns a run's events in
///   ascending [`SequenceNumber`](salvor_core::SequenceNumber) order, always,
///   regardless of the order they were appended in.
/// - **Isolation.** [`read_log`](Self::read_log) for one run returns only that
///   run's events. Interleaving appends across runs does not mix their logs.
/// - **Atomic and durable append.** When [`append`](Self::append) returns
///   `Ok`, the event is committed as one indivisible unit and has reached
///   durable storage. A crash immediately after `Ok` must not lose it and
///   must not leave a partial record. When it returns `Err`, nothing was
///   written.
/// - **Round-trip fidelity.** An envelope read back equals the envelope
///   appended. The exact serialized wire form is preserved.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Appends one event envelope to its run's log.
    ///
    /// Returns [`StoreError::Conflict`] if an event already occupies this
    /// envelope's `(run_id, seq)` position. On `Ok`, the append is atomic and
    /// durable per the trait contract.
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError>;

    /// Reads a run's full log, ordered by ascending sequence number.
    ///
    /// Returns only the named run's events. An unknown run yields an empty
    /// vector, not an error.
    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError>;

    /// Lists every run known to the store, each exactly once, as a
    /// [`RunSummary`].
    ///
    /// This is a summary projection, not a full log read; see [`RunSummary`]
    /// for the fields and why the set is small.
    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError>;
}
