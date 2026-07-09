//! The error type every [`EventStore`](crate::EventStore) method returns.
//!
//! One typed enum, built with `thiserror`. The variant that matters most is
//! [`StoreError::Conflict`]: appending a `(run_id, seq)` pair that already
//! exists is a named, matchable outcome, never a panic and never a silent
//! overwrite. Crash-recovery correctness depends on that
//! conflict being detectable, so it earns its own variant.

use salvor_core::{RunId, SequenceNumber};
use thiserror::Error;

/// What can go wrong talking to an [`EventStore`](crate::EventStore).
///
/// No storage-backend type (for example, no `rusqlite` type) appears here.
/// Backend failures are flattened into [`StoreError::Backend`] as a message
/// string, so the backend can change without breaking this public API.
#[derive(Debug, Error)]
pub enum StoreError {
    /// An event already exists at this `(run_id, seq)` position.
    ///
    /// The `(run_id, seq)` pair is the primary key of the log, so a second
    /// append at the same position is rejected rather than overwriting the
    /// first. This is load-bearing: on resume, a re-attempted append lands
    /// here, which is how a duplicate write is caught instead of silently
    /// doubling history. Match this variant to handle the collision.
    #[error("event already recorded at run {run_id:?}, seq {seq:?}")]
    Conflict {
        /// The run whose log the collision happened in.
        run_id: RunId,
        /// The position that was already occupied.
        seq: SequenceNumber,
    },

    /// An envelope failed to serialize on the way in or deserialize on the way
    /// out.
    ///
    /// The `#[from]` attribute lets a `serde_json::Error` become a
    /// `StoreError` with the `?` operator. `serde_json` is already part of the
    /// event wire contract in `salvor-core`, so naming it here does not widen
    /// the storage-backend surface.
    #[error("serialize or deserialize event envelope: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The storage backend reported a failure that is not a conflict.
    ///
    /// The backend's own error is stringified into this variant so that no
    /// backend type leaks into the public API. A caller that needs to react to
    /// specific backend conditions is a sign a new typed variant should be
    /// added here on purpose.
    #[error("storage backend error: {0}")]
    Backend(String),
}
