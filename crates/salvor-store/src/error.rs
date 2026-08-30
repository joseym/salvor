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

    /// A run's recorded head commits to a different number of rows than the
    /// log holds.
    ///
    /// Its own variant rather than a [`StoreError::TamperEvident`] carrying
    /// two counts, because that message reads "expected 99 recorded rows,
    /// found 10 recorded rows" *at a sequence number*, and mixing a position
    /// with a count is how an operator goes to the wrong line. There is no
    /// position to name here: no single row disagrees, the head disagrees with
    /// all of them.
    #[error(
        "run {} fails its recorded hash chain: the recorded head says {recorded} rows and the \
         log holds {held}",
        .run_id.as_uuid()
    )]
    HeadLength {
        /// The run whose head does not match its rows.
        run_id: RunId,
        /// How many rows the recorded head says the run holds.
        recorded: u64,
        /// How many rows the log actually holds.
        held: u64,
    },

    /// A run's rows are gone and its recorded head is still there.
    ///
    /// The shape a deletion takes, and its own variant because the general
    /// wording for it names the genesis hash: with no rows to chain, the
    /// recomputed head *is* genesis, so the refusal came out "expected
    /// <64 hex>, found 000...0 at seq 0", which reads as a corrupt first event
    /// rather than as an emptied run.
    #[error(
        "run {} fails its recorded hash chain: the run's events are gone and only its recorded \
         head remains ({recorded} rows recorded)",
        .run_id.as_uuid()
    )]
    HeadWithoutRows {
        /// The run whose rows are gone.
        run_id: RunId,
        /// How many rows the recorded head still says the run holds.
        recorded: u64,
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

impl StoreError {
    /// How a chain refusal reads once the run it names has already been
    /// printed: the position it failed at, when there is one, and the clause
    /// saying what disagreed with what. `None` for anything that is not a
    /// chain refusal.
    ///
    /// `salvor verify` prints the run id itself and then one of these, so the
    /// two halves are not stitched back out of the whole message. The three
    /// clauses are the tails of the three `#[error]` messages above, and
    /// `the_refusal_clause_is_the_tail_of_the_message` holds them to it.
    ///
    /// The position is `None` for the two head refusals, because neither has
    /// one: no single row disagrees. A report that prints a seq anyway is
    /// pointing at a line that is not the problem.
    #[must_use]
    pub fn chain_refusal(&self) -> Option<(Option<u64>, String)> {
        match self {
            StoreError::TamperEvident {
                seq,
                expected,
                found,
                ..
            } => Some((
                Some(seq.get()),
                format!("expected {expected}, found {found}"),
            )),
            StoreError::HeadLength { recorded, held, .. } => Some((
                None,
                format!("the recorded head says {recorded} rows and the log holds {held}"),
            )),
            StoreError::HeadWithoutRows { recorded, .. } => Some((
                None,
                format!(
                    "the run's events are gone and only its recorded head remains ({recorded} \
                     rows recorded)"
                ),
            )),
            _ => None,
        }
    }
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

    /// A head that disagrees with its rows has no position to blame, and the
    /// two ways it can disagree say so in their own words rather than dressing
    /// a count up as a sequence number or the genesis hash up as a finding.
    #[test]
    fn a_head_that_disagrees_with_its_rows_names_no_position() {
        let uuid = uuid::Uuid::parse_str("e95cc04e-0000-4000-8000-00000000abcd").expect("uuid");
        let run_id = RunId::from_uuid(uuid);

        let length = StoreError::HeadLength {
            run_id,
            recorded: 99,
            held: 10,
        };
        assert_eq!(
            length.to_string(),
            "run e95cc04e-0000-4000-8000-00000000abcd fails its recorded hash chain: the \
             recorded head says 99 rows and the log holds 10"
        );
        assert!(!length.to_string().contains("seq"), "no position to name");

        let gutted = StoreError::HeadWithoutRows {
            run_id,
            recorded: 3,
        };
        assert_eq!(
            gutted.to_string(),
            "run e95cc04e-0000-4000-8000-00000000abcd fails its recorded hash chain: the run's \
             events are gone and only its recorded head remains (3 rows recorded)"
        );
        assert!(
            !gutted.to_string().contains(&"0".repeat(64)),
            "the genesis hash is what the run was compared against, not what is wrong with it"
        );
    }

    /// `chain_refusal` is what `salvor verify` prints after it has named the
    /// run itself, so each clause has to be exactly the tail of the message
    /// the same error prints on its own. Two wordings that drift apart are two
    /// answers to one question.
    #[test]
    fn the_refusal_clause_is_the_tail_of_the_message() {
        let run_id = RunId::from_uuid(
            uuid::Uuid::parse_str("e95cc04e-0000-4000-8000-00000000abcd").expect("uuid"),
        );
        let refusals = [
            StoreError::TamperEvident {
                run_id,
                seq: SequenceNumber::new(7),
                expected: "a".repeat(64),
                found: "b".repeat(64),
            },
            StoreError::HeadLength {
                run_id,
                recorded: 99,
                held: 10,
            },
            StoreError::HeadWithoutRows {
                run_id,
                recorded: 3,
            },
        ];
        for error in refusals {
            let (_, clause) = error.chain_refusal().expect("a chain refusal");
            let message = error.to_string();
            assert!(
                message.ends_with(&clause),
                "`{clause}` is not the tail of `{message}`"
            );
        }
        assert!(
            StoreError::Backend("disk on fire".to_owned())
                .chain_refusal()
                .is_none(),
            "a backend failure is not a chain refusal"
        );
    }
}
