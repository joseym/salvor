//! Identity types for runs and the positions of events within a run.
//!
//! These are newtypes, not aliases: [`RunId`] and [`SequenceNumber`] are
//! distinct types the compiler will not let you mix, even though one wraps a
//! `Uuid` and the other a `u64`. A function that wants a run identifier cannot
//! be handed a sequence number by mistake.

use core::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a single run.
///
/// Backed by a version 4 (random) UUID. Serializes as its UUID string, so a
/// recorded run identity reads the same in the event log as it does anywhere
/// else the UUID appears.
///
/// # Purity
///
/// [`RunId::new`] draws randomness to mint a fresh UUID, so it is an explicit
/// constructor the caller invokes on purpose, and it sits behind the default-on
/// `rng` feature. Nothing else in this crate draws randomness; the event types
/// never generate identity on their own. A build with `--no-default-features`
/// (the wasm32 build) drops `new` entirely, which is how the crate proves it
/// contains no randomness source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(Uuid);

#[cfg(feature = "rng")]
impl RunId {
    /// Mints a fresh, random run identifier.
    ///
    /// This draws from the system random source, so treat it as a deliberate
    /// side effect at the edge of the runtime, never something the pure event
    /// or replay paths reach for. It lives behind the `rng` feature because the
    /// underlying `getrandom` source does not build for
    /// `wasm32-unknown-unknown`.
    // `new` here is a randomness-drawing constructor, not a cheap default, so a
    // `Default` impl would misrepresent it.
    #[allow(clippy::new_without_default)]
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl RunId {
    /// Wraps an existing UUID as a run identifier.
    ///
    /// Use this to reconstruct a `RunId` read back from storage, where the
    /// UUID already exists and must not be regenerated.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

/// A monotonic position within a run's event log.
///
/// The envelope carries one to order events; some event payloads carry one to
/// correlate a completion with the request that opened it. Ordering is the
/// point of the type, so it derives [`Ord`]: a smaller number happened earlier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SequenceNumber(u64);

impl SequenceNumber {
    /// Constructs a sequence number from a raw position.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next position after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SequenceNumber {
    /// Displays as the bare position (`7`), so error messages and logs can
    /// name a log position without the newtype wrapper showing through.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
