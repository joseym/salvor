//! [`RunSummary`]: the one-line-per-run projection of a store's log.
//!
//! The type lives here, with the event model, rather than in the store crate
//! whose trait returns it. It is a plain value (an id, a count, two
//! timestamps) with no storage machinery behind it, so a consumer that only
//! renders or transports a run listing can name it without linking a database
//! driver.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::id::RunId;

/// A one-line-per-run projection of the log, returned by
/// `EventStore::list_runs`.
///
/// Every field is a pure aggregate over the store's queryable columns
/// (`COUNT`, `MIN`, `MAX`), computed without parsing a single envelope or
/// replaying anything. That is the reason the field set is small and stops
/// where it does:
///
/// - [`run_id`](Self::run_id) names the run. Required.
/// - [`event_count`](Self::event_count) is how many events the run has, a
///   cheap `COUNT(*)` and a useful "how far along" signal.
/// - [`first_recorded_at`](Self::first_recorded_at) and
///   [`last_recorded_at`](Self::last_recorded_at) are the earliest and latest
///   recorded timestamps, a `MIN`/`MAX` that gives a run's start and its age
///   or most recent activity.
///
/// Run *status* (running, suspended, completed, and so on) is deliberately
/// absent. Status is a replay-time projection: you derive it by folding the
/// log, not by reading a column. Putting it here would either require the
/// store to parse and interpret events (which is the replay engine's job) or
/// to keep a denormalized status column in step with the log (a second source
/// of truth). Both break the rule that the log is the only state, so status
/// stays out of the store and lives in the replay layer that owns it.
///
/// The timestamps are reconstructed in UTC. Fidelity to the exact recorded
/// offset lives in the stored envelope JSON; this summary normalizes to UTC
/// because it exists for sorting and display, not for round-tripping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The run this summary describes.
    pub run_id: RunId,
    /// How many events the run's log holds.
    pub event_count: u64,
    /// The earliest recorded timestamp in the run, in UTC.
    #[serde(with = "time::serde::rfc3339")]
    pub first_recorded_at: OffsetDateTime,
    /// The latest recorded timestamp in the run, in UTC.
    #[serde(with = "time::serde::rfc3339")]
    pub last_recorded_at: OffsetDateTime,
}
