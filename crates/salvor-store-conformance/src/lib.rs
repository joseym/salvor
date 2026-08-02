//! A store-agnostic conformance kit for the
//! [`EventStore`](salvor_store::EventStore) trait.
//!
//! The trait's rustdoc states an implementor contract in prose: `(run_id, seq)`
//! uniqueness, ordering by sequence, run isolation, atomic and durable append,
//! round-trip fidelity of the wire form, and tamper evidence over recorded
//! rows. This crate turns that prose into executable checks. If you are writing
//! a new backend (Postgres, DynamoDB, a `FoundationDbStore` in your own
//! repository), you run this kit against it and a passing run is your evidence
//! the backend honors the contract the runtime relies on.
//!
//! # Wiring it into your backend's tests
//!
//! Add this crate as a dev-dependency:
//!
//! ```toml
//! [dev-dependencies]
//! salvor-store-conformance = "0.1"
//! tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
//! ```
//!
//! The kit drives a [`TamperHarness`]: your store, plus one test-only method
//! that rewrites a recorded row the way an attacker with storage access would.
//! Tamper evidence cannot be checked through the
//! [`EventStore`](salvor_store::EventStore) surface alone,
//! because that surface deliberately offers no way to modify a recorded event.
//! Most harnesses are a thin wrapper that delegates the three trait methods and
//! reaches around the store for the fourth:
//!
//! ```ignore
//! struct Harness { store: MyStore, admin: AdminConnection }
//!
//! #[async_trait]
//! impl EventStore for Harness { /* delegate append, read_log, list_runs */ }
//!
//! #[async_trait]
//! impl salvor_store_conformance::TamperHarness for Harness {
//!     async fn forge_recorded_envelope(&self, run: RunId, seq: SequenceNumber, json: &str) {
//!         self.admin.overwrite_row(run, seq, json).expect("forge");
//!     }
//! }
//! ```
//!
//! Then, in a test file, hand the [`conformance_tests!`] macro an expression
//! that builds a fresh harness. It generates one `#[tokio::test]` per check, so
//! failures point at the exact clause that broke:
//!
//! ```ignore
//! salvor_store_conformance::conformance_tests!(Harness::new().expect("open store"));
//! ```
//!
//! The expression is re-evaluated for each generated test, so every check gets
//! its own isolated store. It runs inside an `async fn`, so a backend that
//! connects asynchronously can end the expression in `.await`.
//!
//! For a single test that runs every check, or when you need a store whose
//! setup outlives the checks (a shared temporary directory, say), call
//! [`run_all`] with a factory closure instead:
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
//! async fn conformance() {
//!     salvor_store_conformance::run_all(|| async { MyStore::new().await }).await;
//! }
//! ```
//!
//! The concurrency check spawns racing tasks, so run it under a multi-threaded
//! runtime (`flavor = "multi_thread"`) to exercise genuine parallelism. The
//! macro already stamps that flavor on the concurrency test it generates.
//!
//! # What passing means
//!
//! A passing run demonstrates that, for the operations the kit drives, your
//! backend behaves as the trait specifies: duplicate positions are rejected
//! with the typed [`StoreError::Conflict`](salvor_store::StoreError::Conflict),
//! reads come back ordered and isolated per run, an unknown run reads empty,
//! `list_runs` reports each run once with correct aggregates, the wire form
//! survives a round trip across all twelve event kinds, a pack of writers
//! racing for one position produces exactly one winner with the log holding
//! exactly that one event, and a recorded row rewritten with valid JSON is
//! refused on read with
//! [`StoreError::TamperEvident`](salvor_store::StoreError::TamperEvident)
//! naming the run and position, while its untouched neighbors still read.
//!
//! It also demonstrates the call-commitment clauses, which are what let the
//! layer above promise that a tool call carrying an idempotency key happens
//! once across independent runs rather than once per run: an identity has
//! exactly one owner, a second run is told who that owner is, the owner may
//! re-claim its own unfinished call, a lookup reports all of that without
//! creating anything, a settlement lands its completion and its commitment
//! together or not at all, and a pack of runs racing for one identity produces
//! exactly one winner. A backend that gets any of this wrong lets the same
//! payment go out twice, which is why it is in the kit rather than in
//! SQLite's own tests.
//!
//! Tamper evidence is not optional here. It is in [`run_all`] and in the
//! macro, so a backend cannot claim conformance while serving whatever bytes
//! its storage happens to hold. What the kit checks is the guarantee, not a
//! particular implementation of it; the practical way to satisfy it is
//! `salvor_store::chain`, which is public so every backend chains to the same
//! bytes.
//!
//! # What the kit deliberately does not check
//!
//! - **Fsync-level durability.** The contract says a committed append has
//!   reached durable storage before `append` returns `Ok`. That is a promise
//!   about what survives a power loss or a kernel panic between the `Ok` and
//!   the next write, and it cannot be observed from inside the process making
//!   the call: the data has already been handed back as present. Proving it
//!   needs a fault-injection harness that cuts power (or intercepts `fsync`)
//!   and then reopens the store from a separate process. That is a
//!   backend-specific and platform-specific exercise, out of scope for a
//!   black-box trait kit. The kit verifies that a value read back after `Ok`
//!   matches what was written; it does not and cannot verify that the write
//!   would survive a crash. Treat durability as an argument you make about your
//!   backend's configuration (for SQLite, WAL with `synchronous=FULL`), not
//!   something this kit certifies.
//! - **Tampering that rebuilds or extends a chain.** The tamper checks prove a
//!   recorded row cannot be changed unnoticed. They do not, and cannot, prove
//!   anything against an attacker who rewrites every row of a run from its
//!   first event forward, or who chains fabricated events onto a run's current
//!   head: that attacker holds every input verification holds, because the
//!   chain is unkeyed. Detecting either needs an anchor outside the store,
//!   which is a deployment decision rather than a property of a backend. See
//!   `salvor_store::chain` for where such an anchor attaches.
//! - **Storage-level write guards.** Whether your backend refuses an `UPDATE`
//!   at the table, the bucket, or the filesystem is backend-specific and
//!   belongs in its own tests. The kit checks the guarantee that survives
//!   those guards being bypassed, since any of them can be.
//! - **Performance.** The kit asserts correctness, never latency or throughput.
//!   A per-append budget is a property of a specific backend on specific
//!   hardware, so it stays in that backend's own tests.
//! - **Behavior beyond the trait surface.** Connection pooling, schema
//!   migration, retention, and anything else a backend layers on top of the
//!   three trait methods are the backend's own concern.

mod checks;

pub use checks::{
    TamperHarness, call_claim_is_exclusive, concurrent_call_claims_have_one_winner,
    concurrent_single_position_has_one_winner, duplicate_append_conflicts,
    list_runs_reports_each_run_once, ordering_independent_of_append_order,
    recorded_log_verifies_when_untouched, round_trip_all_event_kinds, runs_are_isolated,
    settling_append_is_atomic, tamper_is_confined_to_its_run, unknown_run_reads_empty,
    usable_as_arc_dyn_event_store, valid_json_tamper_is_detected,
};

use core::future::Future;

/// Runs every conformance check in sequence, each against a store the factory
/// produces fresh.
///
/// `new_store` is called once per check and awaited, so each check runs in
/// isolation and a backend that connects asynchronously is handled. Use this
/// when you want a single test that covers the whole contract, or when the
/// store depends on setup (a temporary directory, a connection pool) that must
/// outlive the individual checks.
///
/// Run it under a multi-threaded runtime so the concurrency check contends for
/// real; on a single-threaded runtime the check still passes but the racers do
/// not run in parallel.
pub async fn run_all<S, F, Fut>(new_store: F)
where
    S: TamperHarness,
    F: Fn() -> Fut,
    Fut: Future<Output = S>,
{
    round_trip_all_event_kinds(new_store().await).await;
    ordering_independent_of_append_order(new_store().await).await;
    runs_are_isolated(new_store().await).await;
    duplicate_append_conflicts(new_store().await).await;
    unknown_run_reads_empty(new_store().await).await;
    list_runs_reports_each_run_once(new_store().await).await;
    usable_as_arc_dyn_event_store(new_store().await).await;
    concurrent_single_position_has_one_winner(new_store().await).await;
    recorded_log_verifies_when_untouched(new_store().await).await;
    valid_json_tamper_is_detected(new_store().await).await;
    tamper_is_confined_to_its_run(new_store().await).await;
    call_claim_is_exclusive(new_store().await).await;
    settling_append_is_atomic(new_store().await).await;
    concurrent_call_claims_have_one_winner(new_store().await).await;
}

/// Generates one `#[tokio::test]` per conformance check against a fresh store.
///
/// The single argument is an expression that builds a store; it is placed
/// inside each generated test's `async fn` body and re-evaluated per test, so
/// every check runs against its own isolated instance and the expression may
/// end in `.await` for a backend that connects asynchronously.
///
/// ```ignore
/// mod in_memory {
///     use my_backend::MyStore;
///     salvor_store_conformance::conformance_tests!(MyStore::new().expect("open"));
/// }
/// ```
///
/// The generated concurrency test is stamped with the multi-threaded runtime
/// flavor, so your dev-dependency on `tokio` must enable its `rt-multi-thread`
/// feature.
#[macro_export]
macro_rules! conformance_tests {
    ($factory:expr $(,)?) => {
        #[::tokio::test]
        async fn round_trip_all_event_kinds() {
            $crate::round_trip_all_event_kinds($factory).await;
        }

        #[::tokio::test]
        async fn ordering_independent_of_append_order() {
            $crate::ordering_independent_of_append_order($factory).await;
        }

        #[::tokio::test]
        async fn runs_are_isolated() {
            $crate::runs_are_isolated($factory).await;
        }

        #[::tokio::test]
        async fn duplicate_append_conflicts() {
            $crate::duplicate_append_conflicts($factory).await;
        }

        #[::tokio::test]
        async fn unknown_run_reads_empty() {
            $crate::unknown_run_reads_empty($factory).await;
        }

        #[::tokio::test]
        async fn list_runs_reports_each_run_once() {
            $crate::list_runs_reports_each_run_once($factory).await;
        }

        #[::tokio::test]
        async fn usable_as_arc_dyn_event_store() {
            $crate::usable_as_arc_dyn_event_store($factory).await;
        }

        #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_single_position_has_one_winner() {
            $crate::concurrent_single_position_has_one_winner($factory).await;
        }

        #[::tokio::test]
        async fn recorded_log_verifies_when_untouched() {
            $crate::recorded_log_verifies_when_untouched($factory).await;
        }

        #[::tokio::test]
        async fn valid_json_tamper_is_detected() {
            $crate::valid_json_tamper_is_detected($factory).await;
        }

        #[::tokio::test]
        async fn tamper_is_confined_to_its_run() {
            $crate::tamper_is_confined_to_its_run($factory).await;
        }

        #[::tokio::test]
        async fn call_claim_is_exclusive() {
            $crate::call_claim_is_exclusive($factory).await;
        }

        #[::tokio::test]
        async fn settling_append_is_atomic() {
            $crate::settling_append_is_atomic($factory).await;
        }

        #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn concurrent_call_claims_have_one_winner() {
            $crate::concurrent_call_claims_have_one_winner($factory).await;
        }
    };
}
