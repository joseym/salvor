//! Which sleeping runs are due: the one question both wakers ask, answered
//! once.
//!
//! A run parked on a durable timer is passive data. Nothing in the process
//! holds it, and nothing fires when its instant arrives; the only way it moves
//! again is for something to re-drive it, at which point
//! [`RunCtx::await_wake`](crate::RunCtx::await_wake) reads the injected clock
//! and either records the wake or reports the run still asleep. So a waker
//! needs exactly two things: a list of runs whose deadline has passed, and the
//! ordinary resume path. This module is the first; the second already exists.
//!
//! # Why this lives in the runtime crate
//!
//! Both wakers are elsewhere (`salvor wake` in the CLI, the `serve` sweeper in
//! the server), and neither may own the answer, or the two would drift on what
//! "due" means. The question is about the store and the fold, not about a
//! terminal or an HTTP route, and this is the lowest crate that sees both a
//! [`EventStore`] and the sleep primitives that put a run into this state. The
//! pure-renderer crate (`salvor-cli-core`) cannot hold it: it compiles for
//! `wasm32-unknown-unknown` and names no store.
//!
//! # It drives nothing
//!
//! [`due_runs`] reads. It appends no event, builds no agent, and calls no
//! clock of its own: the caller passes the instant to measure against, so a
//! test asks "what is due at this moment" without moving a real clock, and
//! both wakers measure against the same clock their drive will use.

use salvor_core::{RunId, RunStatus, derive_state};
use salvor_store::{EventStore, StoreError};
use time::OffsetDateTime;

/// A run whose durable timer has come due.
///
/// The run id is what a caller re-drives; `wake_at` is the deadline the log
/// recorded, carried so a report can say how overdue the run is without
/// folding it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueRun {
    /// The sleeping run.
    pub run_id: RunId,
    /// The instant its recorded `SleepStarted` said it may continue at.
    pub wake_at: OffsetDateTime,
}

/// Every run in `store` whose status folds to
/// [`RunStatus::Sleeping`](salvor_core::RunStatus::Sleeping) with a `wake_at`
/// at or before `now`, oldest deadline first.
///
/// The comparison is `wake_at <= now`, the same inclusive edge
/// [`RunCtx::await_wake`](crate::RunCtx::await_wake) applies, so a run this
/// function reports as due is one that drive will actually wake rather than
/// send straight back to sleep.
///
/// # Why every log is read
///
/// Status is a replay-time projection, not a stored column, so there is
/// nothing to select on until each log has been folded; this walks
/// [`EventStore::list_runs`] and folds each one, exactly as `salvor list`
/// does. That makes the cost linear in the store, which is the honest cost of
/// keeping status out of the schema, and it is why a caller sweeps on an
/// interval rather than in a tight loop.
///
/// A run whose log fails to read (a broken chain) is skipped rather than
/// failing the whole selection: one damaged run must not stop every other due
/// run from waking, and the damage surfaces the moment anything asks for that
/// log by name.
///
/// # Errors
///
/// [`StoreError`] when the run listing itself cannot be read.
pub async fn due_runs(
    store: &dyn EventStore,
    now: OffsetDateTime,
) -> Result<Vec<DueRun>, StoreError> {
    let mut due = Vec::new();
    for summary in store.list_runs().await? {
        let Ok(log) = store.read_log(summary.run_id).await else {
            tracing::warn!(
                run_id = %summary.run_id.as_uuid(),
                "skipping a run whose log will not read while selecting due timers"
            );
            continue;
        };
        if let RunStatus::Sleeping { wake_at } = derive_state(&log).status
            && wake_at <= now
        {
            due.push(DueRun {
                run_id: summary.run_id,
                wake_at,
            });
        }
    }
    // Oldest deadline first, so the most overdue run is driven first and a
    // sweep that runs out of time leaves the least-overdue behind. The run id
    // breaks ties, so the order is total and a report is reproducible.
    due.sort_by(|a, b| {
        a.wake_at
            .cmp(&b.wake_at)
            .then_with(|| a.run_id.as_uuid().cmp(&b.run_id.as_uuid()))
    });
    Ok(due)
}
