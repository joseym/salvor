//! The per-run hash chain that makes a recorded log tamper-evident, and the
//! normative definition of what is hashed.
//!
//! A run's log is append-only, but "append-only" on its own is a property of
//! the code that writes it, not of the bytes on disk. Anything that can open
//! the database file can rewrite a row, and if the rewrite is well-formed
//! nothing downstream notices. This module is the mechanism that closes that
//! gap: every recorded row carries the hash of the row before it, so changing
//! any recorded byte breaks every link from that row forward, and
//! [`EventStore::read_log`](crate::EventStore::read_log) recomputes the whole
//! chain before it hands a log back.
//!
//! The chain lives here, in the storage layer, and never in the event wire
//! shape. [`EventEnvelope`](salvor_core::EventEnvelope) is frozen at
//! `SCHEMA_VERSION` 1 and gains no hash field; a log exported today is
//! byte-identical to one exported before the chain existed. What the store
//! keeps alongside each envelope is bookkeeping the store owns.
//!
//! # The normative definition
//!
//! Rows are chained **in append order**, not in sequence order. A store may
//! legitimately receive seq 3 before seq 1 (the trait only promises that
//! `read_log` *returns* them sorted), so append order is the only order that
//! is a fact about the writing rather than about the payload.
//!
//! For row *n* of a run, with `prev` the [`row_hash`] of row *n - 1* and
//! [`GENESIS_PREV_HASH`] for the first row:
//!
//! ```text
//! preimage = CHAIN_SPEC   || "\n"
//!         || prev         || "\n"
//!         || run_id       || "\n"
//!         || seq          || "\n"
//!         || envelope_json
//!
//! row_hash = lowercase_hex(sha256(preimage))
//! ```
//!
//! Field by field, exactly:
//!
//! - `CHAIN_SPEC` is the ASCII string [`CHAIN_SPEC`], a domain tag. It names
//!   both the construction and the digest, so a future chain that hashes
//!   something else cannot be confused with this one, and a hash of these
//!   bytes cannot be replayed as a hash of anything else in the system.
//! - `prev` is the previous row's `row_hash`, as its 64 lowercase hex
//!   characters, or [`GENESIS_PREV_HASH`] (64 `0`s) for a run's first row.
//! - `run_id` is the run's UUID rendered lowercase and hyphenated, the same
//!   36 characters the store keys rows by.
//! - `seq` is the sequence number in ASCII decimal, no padding, no sign.
//! - `envelope_json` is **the exact bytes the store recorded**, byte for byte,
//!   with nothing appended. Not a re-serialization, not a canonicalization:
//!   what is on disk is what is hashed, so a chain that verifies is a
//!   statement about the stored bytes rather than about a value that happens
//!   to parse the same way.
//! - The separator is a single `\n` (0x0A) between fields. Every field before
//!   the envelope is drawn from an alphabet that contains no newline (hex,
//!   hyphenated UUID, decimal digits), and `serde_json` emits compact,
//!   single-line JSON with any literal newline escaped, so the split points
//!   are unambiguous and no two distinct row inputs can produce the same
//!   preimage.
//!
//! The digest is SHA-256 and the output is 64 lowercase hex characters, never
//! prefixed. The algorithm is named by [`CHAIN_SPEC`], which the SQLite
//! backend also records in its metadata table, so an algorithm change is a new
//! spec string rather than a silent reinterpretation of old rows.
//!
//! # What verification proves, and what it does not
//!
//! [`verify`] recomputes a run's chain from genesis and compares every link
//! against what is recorded. It detects any modification, reordering,
//! insertion, or removal of recorded rows, including a rewrite that is
//! perfectly valid JSON, because the hash covers the stored bytes rather than
//! their meaning.
//!
//! Two things it does not detect, both for the same reason. It does not catch
//! an attacker who rewrites a run *in full*, from genesis forward, recomputing
//! every hash and the recorded head. And it does not catch one who *extends* a
//! run, computing correct links for fabricated events onto the current head
//! and moving [`ChainHead`] along with them, which is arithmetically identical
//! to an honest append. The reason in both cases is that this construction is
//! unkeyed on purpose: everything the verifier uses is sitting in the database
//! next to the rows, so anyone who can write the database holds every input
//! the verifier holds.
//!
//! What the chain does buy, unconditionally, is that *recorded history cannot
//! be quietly revised*. Changing what a run already recorded, reordering it,
//! splicing a row into the middle of it, or dropping rows out of it are all
//! refused on the next read.
//!
//! Closing the remaining two cases needs an anchor outside the store: a
//! signature over the head hash under a key the store does not have, or the
//! head hash published somewhere append-only, so that "the head moved" becomes
//! a claim someone outside can check. [`ChainHead`] is the seam that would
//! attach to: it is the single value per run that commits to the entire log,
//! so an external anchor signs 32 bytes per run rather than the log.
//!
//! # For backend implementors
//!
//! Every [`EventStore`](crate::EventStore) backend is expected to chain its
//! rows with these functions rather than roll its own, so that all backends
//! agree byte for byte on what a run's head hash is. The shape is:
//!
//! - On append, read the run's current head (or genesis), compute
//!   [`row_hash`], and store the envelope bytes, the previous hash, and the
//!   new hash as one atomic unit together with the new head.
//! - On read, collect the run's rows in append order and call [`verify`]
//!   before returning anything.
//!
//! `salvor-store-conformance` drives both halves against any backend, so a
//! backend that skips this fails the kit.

use salvor_core::{RunId, SequenceNumber};
use sha2::{Digest, Sha256};

use crate::error::StoreError;

/// The domain tag that opens every hashed preimage, naming this chain
/// construction and its digest.
///
/// A change to what is hashed, or to the digest, is a change to this string.
/// The SQLite backend records it in its metadata table so a store on disk
/// carries the name of the rule its hashes were built under.
pub const CHAIN_SPEC: &str = "salvor.chain.v1";

/// The previous-hash value for the first row of a run: 64 `0` characters.
///
/// A constant rather than a random or per-run value on purpose: the chain is
/// reproducible from the stored rows alone, so anyone with the log can
/// recompute it without a secret. The chain is evidence of tampering, not a
/// secret-keyed authentication code.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// One recorded row as [`verify`] reads it: the stored envelope bytes and the
/// two chain values recorded beside them.
///
/// Borrowed rather than owned so a backend can hand over slices of what it
/// just read without copying.
#[derive(Debug, Clone, Copy)]
pub struct ChainRow<'a> {
    /// The row's sequence number, as recorded.
    pub seq: SequenceNumber,
    /// The exact envelope bytes the store holds for this row.
    pub envelope_json: &'a str,
    /// The `row_hash` this row was chained onto, as recorded.
    pub prev_hash: &'a str,
    /// This row's own `row_hash`, as recorded.
    pub row_hash: &'a str,
}

/// A run's recorded chain head: how many rows the store believes it holds, and
/// the hash of the last one.
///
/// Kept separately from the rows themselves so that removing whole rows,
/// including the most recent ones, is detectable: the surviving rows would
/// still form a valid chain among themselves, but they would not reproduce
/// this head.
///
/// This is also the value an external anchor would sign; see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct ChainHead<'a> {
    /// The number of rows recorded for the run.
    pub len: u64,
    /// The `row_hash` of the run's last recorded row.
    pub hash: &'a str,
}

/// Computes one row's hash, per the definition in the module docs.
///
/// `envelope_json` must be the exact bytes the store recorded or will record;
/// hashing a re-serialization would make the chain a statement about a value
/// rather than about what is on disk.
#[must_use]
pub fn row_hash(
    prev_hash: &str,
    run_id: RunId,
    seq: SequenceNumber,
    envelope_json: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHAIN_SPEC.as_bytes());
    hasher.update(b"\n");
    hasher.update(prev_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(run_id.as_uuid().to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(seq.get().to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(envelope_json.as_bytes());
    hex(&hasher.finalize())
}

/// Recomputes a run's chain from genesis and checks it against what is
/// recorded.
///
/// `rows` must be in **append order**, the order the chain was built in. Each
/// row is checked twice: its recorded `prev_hash` must equal the running hash,
/// and its recorded `row_hash` must equal the hash recomputed from the stored
/// bytes. When `recorded_head` is present, the final running hash and the row
/// count must match it, which is what catches whole rows having been removed
/// from the end.
///
/// # Errors
///
/// Returns [`StoreError::TamperEvident`] naming the run and the sequence
/// number of the first row that does not agree with the chain. A run with no
/// rows and no recorded head verifies trivially, which is what makes an
/// unknown run read back as an empty log rather than an error.
pub fn verify(
    run_id: RunId,
    rows: &[ChainRow<'_>],
    recorded_head: Option<ChainHead<'_>>,
) -> Result<(), StoreError> {
    let mut running = GENESIS_PREV_HASH.to_owned();

    for row in rows {
        if row.prev_hash != running {
            return Err(StoreError::TamperEvident {
                run_id,
                seq: row.seq,
                expected: running,
                found: row.prev_hash.to_owned(),
            });
        }
        let computed = row_hash(&running, run_id, row.seq, row.envelope_json);
        if computed != row.row_hash {
            return Err(StoreError::TamperEvident {
                run_id,
                seq: row.seq,
                expected: row.row_hash.to_owned(),
                found: computed,
            });
        }
        running = computed;
    }

    if let Some(head) = recorded_head {
        // The seq to blame when the head disagrees is the last row that
        // survived, or position 0 when every row is gone.
        let seq = rows
            .last()
            .map_or_else(|| SequenceNumber::new(0), |row| row.seq);
        if head.hash != running {
            return Err(StoreError::TamperEvident {
                run_id,
                seq,
                expected: head.hash.to_owned(),
                found: running,
            });
        }
        let len = rows.len() as u64;
        if head.len != len {
            return Err(StoreError::TamperEvident {
                run_id,
                seq,
                expected: format!("{} recorded rows", head.len),
                found: format!("{len} recorded rows"),
            });
        }
    }

    Ok(())
}

/// Renders bytes as lowercase hex, the form every hash in this module takes.
fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use salvor_core::{Event, EventEnvelope};
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;

    /// A fixed run id, so the golden-vector test below is reproducible.
    fn run() -> RunId {
        RunId::from_uuid(Uuid::parse_str("6d1b7a7e-0000-4000-8000-00000000abcd").expect("uuid"))
    }

    fn envelope_json(seq: u64, tag: &str) -> String {
        let env = EventEnvelope::new(
            run(),
            SequenceNumber::new(seq),
            OffsetDateTime::from_unix_timestamp(1_000_000).expect("timestamp"),
            Event::RunFailed { error: tag.into() },
        );
        serde_json::to_string(&env).expect("serialize")
    }

    /// The hash of a fixed input is pinned here as a golden vector. If a change
    /// to the preimage layout ever lands without a matching change to
    /// [`CHAIN_SPEC`], this test is what fails: every store on disk would
    /// otherwise stop verifying with no explanation.
    ///
    /// The pinned value was computed outside this crate, from the module
    /// docs alone, so it checks the implementation against the written spec
    /// rather than against itself:
    ///
    /// ```sh
    /// printf 'salvor.chain.v1\n%s\n%s\n1\n{"pinned":"bytes"}' \
    ///   0000000000000000000000000000000000000000000000000000000000000000 \
    ///   6d1b7a7e-0000-4000-8000-00000000abcd | shasum -a 256
    /// ```
    #[test]
    fn row_hash_is_a_pinned_golden_vector() {
        let hash = row_hash(
            GENESIS_PREV_HASH,
            run(),
            SequenceNumber::new(1),
            r#"{"pinned":"bytes"}"#,
        );
        assert_eq!(
            hash, "a0bae6607964dc32cb46850ebe673640389fd66e4c498b30506b051563b10408",
            "the chain preimage changed; bump CHAIN_SPEC deliberately if that was intended"
        );
    }

    #[test]
    fn hash_is_sixty_four_lowercase_hex_characters() {
        let hash = row_hash(GENESIS_PREV_HASH, run(), SequenceNumber::new(1), "{}");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    /// Every input to the preimage moves the hash: the previous link, the run,
    /// the position, and the bytes.
    #[test]
    fn every_input_changes_the_hash() {
        let base = row_hash(GENESIS_PREV_HASH, run(), SequenceNumber::new(1), "{}");
        let other_prev = row_hash(&"1".repeat(64), run(), SequenceNumber::new(1), "{}");
        let other_run = row_hash(
            GENESIS_PREV_HASH,
            RunId::from_uuid(Uuid::nil()),
            SequenceNumber::new(1),
            "{}",
        );
        let other_seq = row_hash(GENESIS_PREV_HASH, run(), SequenceNumber::new(2), "{}");
        let other_bytes = row_hash(GENESIS_PREV_HASH, run(), SequenceNumber::new(1), "{ }");
        for (name, hash) in [
            ("prev", other_prev),
            ("run", other_run),
            ("seq", other_seq),
            ("bytes", other_bytes),
        ] {
            assert_ne!(base, hash, "changing {name} must change the hash");
        }
    }

    /// A chain built the way a backend builds it verifies, and every recorded
    /// value is checked: flipping one stored byte of one envelope breaks it,
    /// and the error names the row.
    #[test]
    fn a_well_built_chain_verifies_and_a_rewritten_row_does_not() {
        let bodies: Vec<String> = (1..=3).map(|n| envelope_json(n, "real")).collect();
        let mut prev = GENESIS_PREV_HASH.to_owned();
        let mut hashes = Vec::new();
        let mut prevs = Vec::new();
        for (index, body) in bodies.iter().enumerate() {
            let hash = row_hash(&prev, run(), SequenceNumber::new(index as u64 + 1), body);
            prevs.push(prev.clone());
            hashes.push(hash.clone());
            prev = hash;
        }
        let head = ChainHead {
            len: 3,
            hash: &prev,
        };
        let rows: Vec<ChainRow<'_>> = (0..3)
            .map(|i| ChainRow {
                seq: SequenceNumber::new(i as u64 + 1),
                envelope_json: &bodies[i],
                prev_hash: &prevs[i],
                row_hash: &hashes[i],
            })
            .collect();
        verify(run(), &rows, Some(head)).expect("an untouched chain verifies");

        // Row two rewritten with different, still perfectly valid, JSON.
        let forged = envelope_json(2, "forged");
        let mut tampered = rows.clone();
        tampered[1].envelope_json = &forged;
        match verify(run(), &tampered, Some(head)) {
            Err(StoreError::TamperEvident { run_id, seq, .. }) => {
                assert_eq!(run_id, run());
                assert_eq!(
                    seq,
                    SequenceNumber::new(2),
                    "the error names the rewritten row"
                );
            }
            other => panic!("expected TamperEvident, got {other:?}"),
        }

        // Dropping the last row keeps the survivors self-consistent, so only
        // the recorded head catches it.
        let truncated = &rows[..2];
        verify(run(), truncated, None).expect("survivors chain among themselves");
        assert!(
            verify(run(), truncated, Some(head)).is_err(),
            "the recorded head must catch a removed trailing row"
        );
    }

    #[test]
    fn an_empty_run_verifies() {
        verify(run(), &[], None).expect("no rows and no head is not tampering");
    }
}
