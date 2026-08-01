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
use salvor_core::{EventEnvelope, RunId, SequenceNumber};

use crate::error::StoreError;

pub use salvor_core::RunSummary;

/// One run's bid to be the single executor of a tool call, identified by the
/// tool's name and the call's idempotency key.
///
/// The identity a call commits under is the pair `(tool, idempotency_key)` and
/// nothing else. The run and the position of its intent ride along so the
/// commitment can point back at the log entry that owns it, and so a claim can
/// tell "I already hold this" apart from "someone else does".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallClaimant<'a> {
    /// The tool's name, the first half of the identity.
    pub tool: &'a str,
    /// The call's idempotency key, the second half of the identity.
    pub idempotency_key: &'a str,
    /// The run making the claim.
    pub run_id: RunId,
    /// The position of that run's `ToolCallRequested` for this call, which is
    /// also the correlation sequence its completion will carry.
    pub intent_seq: SequenceNumber,
}

/// The store's record of which run owns a `(tool, idempotency_key)` identity,
/// and whether that run's call has finished.
///
/// `completion_seq` is the whole distinction that matters to a second caller.
/// `Some` means a completion for this identity is committed in the named run's
/// log and can be copied; `None` means the owning call is still in flight, or
/// died in flight, and nobody may assume anything about whether its effect
/// happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallCommitment {
    /// The run that owns the identity.
    pub run_id: RunId,
    /// The position of that run's intent for this call, which is also the
    /// correlation sequence of its completion.
    pub intent_seq: SequenceNumber,
    /// The log position the settling completion was appended at, or `None`
    /// while the owning call is unfinished.
    pub completion_seq: Option<SequenceNumber>,
}

/// The answer to a claim: either the claimant now owns the identity, or
/// somebody already did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallClaim {
    /// The claimant owns the identity and is the one execution of this call.
    /// Also the answer when the claimant already held it, so re-entering the
    /// same call after a crash is not mistaken for a rival.
    Claimed,
    /// A different run owns the identity. Inspect the commitment: settled means
    /// its output may be copied, unsettled means nothing may be assumed.
    Held(CallCommitment),
}

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
/// - **Tamper evidence.** A recorded event that is later modified, reordered,
///   inserted, or removed by anything other than [`append`](Self::append) must
///   be detected on the next [`read_log`](Self::read_log) and reported as
///   [`StoreError::TamperEvident`], naming the run and the position. Detection
///   must not depend on the change being malformed: a row rewritten with
///   perfectly valid envelope JSON is the case that matters, and the case a
///   parse check misses. Implement this by chaining rows with
///   [`crate::chain`], which defines the hash normatively so that every
///   backend arrives at the same head hash for the same log, and verify the
///   whole chain before returning any event. Storage-level guards (a trigger,
///   a read-only bucket policy) are welcome as defense in depth but do not
///   satisfy this clause on their own, because anything that can write the
///   storage can usually remove the guard too.
/// - **Exclusive call commitment.** [`claim_call`](Self::claim_call) is the
///   arbiter that lets exactly one run execute a given
///   `(tool, idempotency_key)`. It must be atomic: concurrent claims for one
///   identity produce exactly one [`CallClaim::Claimed`], and every other claim
///   returns [`CallClaim::Held`] naming that same winner. A claimant that
///   already holds the identity gets `Claimed` again, so a run re-entering its
///   own call after a crash is never told it lost to itself. A claim is
///   permanent: nothing releases it, because releasing it would be a licence to
///   execute the same effect a second time.
/// - **Atomic settlement.** [`append_settling_call`](Self::append_settling_call)
///   appends its envelope under every clause above *and* marks the claimant's
///   commitment settled at that envelope's position, as one indivisible unit.
///   Splitting the two would let a crash leave a committed completion whose
///   commitment still read unsettled, which every later run would then have to
///   treat as an effect in flight, forever. It must fail without appending if
///   the claimant does not hold the identity.
/// - **Lookup without side effects.** [`lookup_call`](Self::lookup_call)
///   reports the commitment for an identity and changes nothing, in particular
///   never creating one. It exists because a run recovering a dangling intent
///   must learn who owns the identity *without* acquiring it.
///
/// Note what those three clauses deliberately do not do: none of them serves an
/// event. A commitment holds a pointer (a run and a position), never a recorded
/// output, so anything that wants a deduplicated call's result goes through
/// [`read_log`](Self::read_log) and takes the chain verification with it. There
/// is no path to a recorded value that skips tamper evidence.
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
    ///
    /// This is the enforcement point for tamper evidence: the log's chain is
    /// verified here, before any event is handed back, so a run whose recorded
    /// bytes no longer match is refused with
    /// [`StoreError::TamperEvident`] rather than partially served. Anything
    /// that reads the underlying storage directly instead of calling this is
    /// not an audit path.
    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError>;

    /// Lists every run known to the store, each exactly once, as a
    /// [`RunSummary`].
    ///
    /// This is a summary projection, not a full log read; see [`RunSummary`]
    /// for the fields and why the set is small. Because it is a projection and
    /// not a read of any log, it does **not** verify chains: a run whose log
    /// has been tampered with still appears in the listing, and only
    /// [`read_log`](Self::read_log) refuses it. That is deliberate, so one
    /// damaged run does not blank a whole listing, but it means a listing is
    /// not evidence about integrity.
    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError>;

    /// Claims the sole right to execute the call identified by
    /// `(tool, idempotency_key)`, on behalf of one run's recorded intent.
    ///
    /// This is the arbiter between two live runs that reached the same call at
    /// the same moment. Whichever claim the store commits first wins and gets
    /// [`CallClaim::Claimed`]; the other gets [`CallClaim::Held`] naming the
    /// winner. Re-claiming an identity this same claimant already holds returns
    /// `Claimed` again, which is what lets a run resume its own unfinished call
    /// without deadlocking against itself.
    ///
    /// A claim is never released. "This identity has been executed, or is being
    /// executed, by that run" stays true forever, and forgetting it would be
    /// permission to execute the effect twice.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] when the claim cannot be committed.
    async fn claim_call(&self, claimant: CallClaimant<'_>) -> Result<CallClaim, StoreError>;

    /// Reports the commitment recorded for `(tool, idempotency_key)`, or `None`
    /// if the identity has never been claimed.
    ///
    /// Read-only, and specifically not a claim: a run that is working out what
    /// happened to a dangling intent must be able to see who owns an identity
    /// without becoming its owner.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] when the lookup fails.
    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<CallCommitment>, StoreError>;

    /// Appends a call's completion and settles the claimant's commitment on it,
    /// atomically.
    ///
    /// The envelope is appended under every rule [`append`](Self::append)
    /// follows, including the chain, and in the same indivisible unit the
    /// commitment for `claimant`'s identity is marked settled at this
    /// envelope's position. After this returns `Ok`, any other run that claims
    /// the identity is told the call is committed and where to read it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] if an event already occupies the envelope's
    /// `(run_id, seq)` position, and [`StoreError::Backend`] if `claimant` does
    /// not hold the identity (settling a commitment one does not own is a bug,
    /// not a race) or the write cannot be committed. On any error nothing is
    /// appended and no commitment changes.
    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: CallClaimant<'_>,
    ) -> Result<(), StoreError>;
}
