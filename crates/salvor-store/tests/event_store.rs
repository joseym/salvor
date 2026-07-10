//! Integration tests for the `salvor-store` public API.
//!
//! Most of the contract is proven by consuming the store-agnostic
//! `salvor-store-conformance` kit: `SqliteStore` is its first consumer, exercised
//! both in-memory and file-backed. What stays here is the SQLite-specific
//! measurement the generic kit deliberately excludes (append overhead), plus
//! the small helpers that test needs.
//!
//! Driving `SqliteStore` through the kit is itself a test that the trait is
//! object-safe and async-usable, since the kit only ever names the public
//! `EventStore` surface, never `rusqlite`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::{EventStore, SqliteStore};
use time::OffsetDateTime;

/// The full conformance kit against an in-memory store, one `#[tokio::test]`
/// per check so a failure names the exact contract clause that broke.
mod in_memory {
    use salvor_store::SqliteStore;

    salvor_store_conformance::conformance_tests!(
        SqliteStore::in_memory().expect("open in-memory store")
    );
}

/// The full conformance kit against real file-backed WAL databases, the posture
/// where durability and locking actually cost something. A single temporary
/// directory outlives every check; each check gets its own database file inside
/// it, and the directory is removed when the test ends. The multi-threaded
/// runtime lets the kit's concurrency check contend for real.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conformance_kit_file_backed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let counter = AtomicU64::new(0);
    salvor_store_conformance::run_all(|| {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.path().join(format!("events-{n}.db"));
        async move { SqliteStore::open(&path).expect("open file-backed store") }
    })
    .await;
}

/// SQLite-specific, so it stays out of the store-agnostic kit: against a
/// file-backed WAL store, appending at least 100 events keeps the mean
/// per-append under 5ms. Prints the observed mean; see it with
/// `cargo test -- --nocapture`.
#[tokio::test]
async fn append_overhead_stays_under_five_ms() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("events.db");
    let store = SqliteStore::open(&path).expect("open file-backed store");
    let run = RunId::new();

    const APPENDS: u64 = 200;
    let start = Instant::now();
    for seq in 1..=APPENDS {
        store
            .append(&envelope(run, seq, fail("bench")))
            .await
            .expect("append");
    }
    let mean = start.elapsed() / APPENDS as u32;

    println!(
        "append overhead: {APPENDS} appends, mean {:.3} ms per append",
        mean.as_secs_f64() * 1000.0
    );
    assert!(
        mean.as_secs_f64() * 1000.0 < 5.0,
        "mean append {:.3} ms exceeded the 5ms target",
        mean.as_secs_f64() * 1000.0
    );
}

/// Wraps a payload in an envelope for `run` at `seq`, timestamped `seq` seconds
/// after the epoch so ordering is easy to reason about.
fn envelope(run: RunId, seq: u64, event: Event) -> EventEnvelope {
    let recorded_at =
        OffsetDateTime::from_unix_timestamp(1_000_000 + seq as i64).expect("timestamp in range");
    EventEnvelope::new(run, SequenceNumber::new(seq), recorded_at, event)
}

/// A cheap distinct payload: a `RunFailed` whose error string tags the event.
fn fail(tag: &str) -> Event {
    Event::RunFailed { error: tag.into() }
}
