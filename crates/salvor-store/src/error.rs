//! The error type every [`EventStore`](crate::EventStore) method returns.
//!
//! One typed enum, built with `thiserror`. Two variants carry the weight.
//! [`StoreError::Conflict`]: appending a `(run_id, seq)` pair that already
//! exists is a named, matchable outcome, never a panic and never a silent
//! overwrite. Crash-recovery correctness depends on that
//! conflict being detectable, so it earns its own variant.
//! [`StoreError::TamperEvident`]: a run whose recorded rows no longer match
//! their hash chain is refused on read rather than served, and it is a
//! separate variant from [`StoreError::Serialization`] because the dangerous
//! case is exactly the one that still parses.

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
    #[error("event already recorded at run {}, seq {}", .run_id.as_uuid(), .seq.get())]
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

    /// A run's recorded log does not match its hash chain: something rewrote,
    /// reordered, inserted, or removed a row after it was recorded.
    ///
    /// This is the variant that makes the log tamper-*evident* rather than
    /// merely append-only-by-convention. Every recorded row carries the hash
    /// of the row before it (see [`crate::chain`] for the normative
    /// definition), and [`read_log`](crate::EventStore::read_log) recomputes
    /// the whole chain before returning anything, so a modified row is refused
    /// instead of served. It is deliberately distinct from
    /// [`StoreError::Serialization`]: a row rewritten with *valid* JSON
    /// deserializes perfectly and would otherwise be indistinguishable from
    /// what was recorded. Getting this error means the bytes changed, whatever
    /// they now say.
    ///
    /// Treat it as an integrity incident, not a transient failure. Retrying
    /// the read produces the same error, because the stored bytes are the
    /// problem.
    #[error(
        "run {} fails its recorded hash chain at seq {}: expected {expected}, found {found}",
        .run_id.as_uuid(),
        .seq.get()
    )]
    TamperEvident {
        /// The run whose log failed verification.
        run_id: RunId,
        /// The position of the first row that does not agree with the chain.
        seq: SequenceNumber,
        /// The value the chain requires at that point, normally a 64-character
        /// hex hash.
        expected: String,
        /// The value the store actually holds there.
        found: String,
    },

    /// The storage backend reported a failure that is not a conflict.
    ///
    /// The backend's own error is stringified into this variant so that no
    /// backend type leaks into the public API. A caller that needs to react to
    /// specific backend conditions is a sign a new typed variant should be
    /// added here on purpose.
    #[error("storage backend error: {0}")]
    Backend(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every identifier in a message an operator reads prints as the thing it
    /// names: a run id is the bare UUID they can paste into `salvor history`,
    /// and a position is the bare number `history` prints beside the event.
    /// Rust's derived `Debug` wraps both in their type name, which turns a
    /// copyable identifier into something that has to be edited first.
    #[test]
    fn a_message_names_a_run_and_a_position_the_way_a_person_would_type_them() {
        let uuid = uuid::Uuid::parse_str("e95cc04e-0000-4000-8000-00000000abcd").expect("uuid");
        let error = StoreError::TamperEvident {
            run_id: RunId::from_uuid(uuid),
            seq: SequenceNumber::new(7),
            expected: "a".repeat(64),
            found: "b".repeat(64),
        };
        let message = error.to_string();
        assert_eq!(
            message,
            format!(
                "run e95cc04e-0000-4000-8000-00000000abcd fails its recorded hash chain at seq \
                 7: expected {}, found {}",
                "a".repeat(64),
                "b".repeat(64)
            )
        );
        assert!(!message.contains("RunId("), "{message}");
        assert!(!message.contains("SequenceNumber("), "{message}");

        // The sibling that was already right stays right.
        let conflict = StoreError::Conflict {
            run_id: RunId::from_uuid(uuid),
            seq: SequenceNumber::new(3),
        };
        assert_eq!(
            conflict.to_string(),
            "event already recorded at run e95cc04e-0000-4000-8000-00000000abcd, seq 3"
        );
    }
}
