//! Which runs a waker picks up: `due_runs` over a store holding one of every
//! shape a run can be in.
//!
//! The logs here are written by hand rather than driven, because the question
//! is about the fold and a comparison, not about how a run got into a state.
//! One store holds them all at once, which is the arrangement a real waker
//! faces: a handful of sleeping runs among terminal ones, runs that never
//! slept, and runs whose deadline is still ahead.
//!
//! # The clock
//!
//! `due_runs` reads none. The caller passes the instant, so every case below is
//! "what is due at this moment" asked directly, with nothing sleeping in wall
//! time and nothing to make flaky.

use std::sync::Arc;

use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber};
use salvor_runtime::due_runs;
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

/// The instant every seeded run is recorded at, and the one `due_runs` is
/// asked about unless a case says otherwise.
const NOW: OffsetDateTime = datetime!(2026-08-07 12:00:00 UTC);

/// Appends `events` as one run's log, seq 0 upward, and returns its id.
async fn seed(store: &dyn EventStore, events: Vec<Event>) -> RunId {
    let run_id = RunId::new();
    for (index, event) in events.into_iter().enumerate() {
        let envelope = EventEnvelope::new(run_id, SequenceNumber::new(index as u64), NOW, event);
        store.append(&envelope).await.expect("append");
    }
    run_id
}

/// A plain `RunStarted` head, the opening of every agent run below.
fn started() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:wake-selection".to_owned(),
        input: json!("go"),
        labels: None,
        driven_by: None,
        caller: None,
    }
}

/// A fresh store per test. In memory: nothing here depends on durability
/// across processes, only on what one store answers when asked what is due.
fn store() -> Arc<dyn EventStore> {
    Arc::new(SqliteStore::in_memory().expect("store opens"))
}

/// The whole selection rule in one store: exactly the sleeping runs whose
/// recorded instant is at or before the reading come back, and nothing else
/// does, whatever else is in the store beside them.
#[tokio::test]
async fn only_sleeping_runs_past_their_deadline_are_due() {
    let store = store();

    // Due: the deadline passed an hour ago.
    let overdue = seed(
        store.as_ref(),
        vec![
            started(),
            Event::SleepStarted {
                wake_at: NOW - Duration::hours(1),
            },
        ],
    )
    .await;

    // Due, exactly: the edge is inclusive, the same edge `await_wake` applies,
    // so a run this reports is one the drive will actually wake rather than
    // send straight back to sleep.
    let exactly_now = seed(
        store.as_ref(),
        vec![started(), Event::SleepStarted { wake_at: NOW }],
    )
    .await;

    // Not due: sleeping, but the instant has not arrived.
    seed(
        store.as_ref(),
        vec![
            started(),
            Event::SleepStarted {
                wake_at: NOW + Duration::hours(1),
            },
        ],
    )
    .await;

    // Already woken: the log holds the completion, so the run is running
    // again and there is no timer left to fire.
    seed(
        store.as_ref(),
        vec![
            started(),
            Event::SleepStarted {
                wake_at: NOW - Duration::hours(2),
            },
            Event::SleepCompleted {},
        ],
    )
    .await;

    // Terminal after a sleep: long past its deadline, and finished. A waker
    // that woke this would be re-driving a completed run.
    seed(
        store.as_ref(),
        vec![
            started(),
            Event::SleepStarted {
                wake_at: NOW - Duration::days(9),
            },
            Event::SleepCompleted {},
            Event::RunCompleted {
                output: json!("done"),
            },
        ],
    )
    .await;

    // Never slept, and stopped mid-flight. Recoverable, but not by a waker:
    // nothing about it is waiting on the clock.
    seed(store.as_ref(), vec![started()]).await;

    let due = due_runs(store.as_ref(), NOW)
        .await
        .expect("selection reads");
    let ids: Vec<RunId> = due.iter().map(|run| run.run_id).collect();
    assert_eq!(
        ids,
        vec![overdue, exactly_now],
        "exactly the two sleeping runs at or past their deadline, oldest first"
    );
    assert_eq!(
        due[0].wake_at,
        NOW - Duration::hours(1),
        "the recorded deadline is carried, so a report need not fold the log again"
    );
    assert_eq!(due[1].wake_at, NOW);
}

/// A sleeping run whose instant has not arrived stays unselected until it has,
/// and then is selected with nothing about the run itself having changed. The
/// log is identical across both readings; only the instant asked about moved.
#[tokio::test]
async fn a_deadline_that_has_not_arrived_selects_nothing_until_it_does() {
    let store = store();
    let wake_at = NOW + Duration::days(7);
    let run_id = seed(
        store.as_ref(),
        vec![started(), Event::SleepStarted { wake_at }],
    )
    .await;

    assert!(
        due_runs(store.as_ref(), NOW)
            .await
            .expect("reads")
            .is_empty(),
        "a week out is not due now"
    );
    assert!(
        due_runs(store.as_ref(), wake_at - Duration::seconds(1))
            .await
            .expect("reads")
            .is_empty(),
        "one second short is still not due; the comparison has no slack in it"
    );

    let due = due_runs(store.as_ref(), wake_at).await.expect("reads");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].run_id, run_id);
    assert_eq!(due[0].wake_at, wake_at);
}

/// An empty store is not an error and not a special case: nothing is due, which
/// is what a waker on a quiet store must be told.
#[tokio::test]
async fn an_empty_store_has_nothing_due() {
    let store = store();
    assert!(
        due_runs(store.as_ref(), NOW)
            .await
            .expect("reads")
            .is_empty()
    );
}

/// The most overdue run comes first, so a waker works through the backlog in
/// the order the deadlines passed rather than in whatever order the store
/// happened to list them.
#[tokio::test]
async fn the_listing_is_ordered_by_deadline() {
    let store = store();
    let mut seeded = Vec::new();
    for hours in [1_i64, 9, 3] {
        let run_id = seed(
            store.as_ref(),
            vec![
                started(),
                Event::SleepStarted {
                    wake_at: NOW - Duration::hours(hours),
                },
            ],
        )
        .await;
        seeded.push((hours, run_id));
    }

    let due = due_runs(store.as_ref(), NOW).await.expect("reads");
    let hours: Vec<i64> = due
        .iter()
        .map(|run| (NOW - run.wake_at).whole_hours())
        .collect();
    assert_eq!(hours, vec![9, 3, 1], "oldest deadline first");
    assert_eq!(due.len(), seeded.len());
}
