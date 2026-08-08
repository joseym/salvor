//! `salvor wake` through the real binary: which runs it picks up, what it
//! reports, and what it leaves alone.
//!
//! # Controlling the clock without one
//!
//! `wake` reads the real clock, as an operator's cron entry does. A test does
//! not move that clock; it moves the deadline instead. A run seeded with a
//! `wake_at` an hour in the past is due under any clock a test machine could
//! have, and one seeded a year out is due under none, so both cases are exact
//! and neither waits on wall time. The instant-by-instant rules (the inclusive
//! edge, a drive too early recording nothing) are proven against an injected
//! clock where they belong, in `salvor-runtime`'s own suite.
//!
//! # Why the logs are seeded by hand
//!
//! A seeded log states its deadline and nothing else, which is exactly what
//! these tests are about: selection, routing, reporting, and the exit codes.
//! What it costs is the one case they cannot stage, a run whose re-drive
//! genuinely continues to completion, since a loop replaying a sleep it never
//! issued is a divergence. That case is proven at the runtime tier, over flows
//! that really do sleep (`salvor-runtime`'s `sleep.rs` and `tool_sleep.rs`).

mod common;

use std::path::Path;

use common::run_salvor;
use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use tempfile::tempdir;
use time::{Duration, OffsetDateTime};

/// Writes a run whose log ends at a started sleep, so it folds to `sleeping`
/// with exactly this deadline. `head` opens the log: an agent run's
/// `RunStarted` or a graph run's `GraphRunStarted`, which is the only
/// difference between the two as far as a waker is concerned.
async fn seed_sleeping(store_path: &Path, head: Event, wake_at: OffsetDateTime) -> String {
    let store = SqliteStore::open(store_path).expect("store opens");
    let run_id = RunId::new();
    let recorded_at = OffsetDateTime::now_utc();
    for (seq, event) in [head, Event::SleepStarted { wake_at }]
        .into_iter()
        .enumerate()
    {
        let envelope =
            EventEnvelope::new(run_id, SequenceNumber::new(seq as u64), recorded_at, event);
        store.append(&envelope).await.expect("append");
    }
    run_id.as_uuid().to_string()
}

/// An agent run's head.
fn agent_head() -> Event {
    Event::RunStarted {
        agent_def_hash: "sha256:not-registered-anywhere".to_owned(),
        input: json!("go"),
        labels: None,
    }
}

/// A graph run's head. The hash is opaque here: what makes this a graph run
/// for every surface, `wake` included, is that the log opens with this event.
fn graph_head() -> Event {
    Event::GraphRunStarted {
        graph_hash: "sha256:not-supplied-anywhere".to_owned(),
        input: json!("go"),
        labels: None,
        forked_from: None,
    }
}

/// How many events a run's log holds, for proving a dry run drove nothing.
async fn log_len(store_path: &Path, uuid: &str) -> usize {
    let store = SqliteStore::open(store_path).expect("store opens");
    let run_id = RunId::from_uuid(uuid.parse().expect("a uuid"));
    store.read_log(run_id).await.expect("log reads").len()
}

/// A store with nothing due says so plainly and exits 0. Two ways to have
/// nothing due, and both read the same: no runs at all, and a run whose
/// deadline is still ahead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_due_is_reported_and_succeeds() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");

    // An empty store. `wake` creates it, finds nothing, and says so.
    let empty = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&empty.stdout);
    assert!(
        empty.status.success(),
        "nothing due is not a failure: {out}"
    );
    assert!(
        out.contains("nothing to wake"),
        "the report names the situation: {out}"
    );

    // A run sleeping a year out is present, folds to sleeping, and is still
    // not due, so it must not appear.
    let future = seed_sleeping(
        &store,
        agent_head(),
        OffsetDateTime::now_utc() + Duration::days(365),
    )
    .await;
    let later = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&later.stdout);
    assert!(later.status.success(), "still not a failure: {out}");
    assert!(out.contains("nothing to wake"), "still nothing due: {out}");
    assert!(
        !out.contains(&future),
        "a run whose deadline is ahead is not named: {out}"
    );
    assert_eq!(
        log_len(&store, &future).await,
        2,
        "and nothing was appended to it"
    );

    // The sleeping run is visible in `list` all the same, under its own label
    // and its own group: sleeping is motion, not a to-do item.
    let listed = run_salvor(&store, &["list", "--status", "sleeping"]).await;
    let out = String::from_utf8_lossy(&listed.stdout);
    assert!(out.contains(&future) && out.contains("sleeping"), "{out}");
    let grouped = run_salvor(&store, &["list", "--group", "progress"]).await;
    assert!(
        String::from_utf8_lossy(&grouped.stdout).contains(&future),
        "a sleeping run is in the progress group"
    );
}

/// `--dry-run` names every due run and how overdue it is, then exits 0 having
/// appended nothing, exactly as `salvor fork --dry-run` previews a fork it does
/// not create. Both an agent run and a graph run appear: what is due is a
/// question about the log's deadline, not about what kind of run it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_lists_what_is_due_and_drives_nothing() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let agent_run = seed_sleeping(&store, agent_head(), now - Duration::hours(2)).await;
    let graph_run = seed_sleeping(&store, graph_head(), now - Duration::days(3)).await;
    let not_due = seed_sleeping(&store, agent_head(), now + Duration::days(30)).await;

    let dry = run_salvor(&store, &["wake", "--dry-run"]).await;
    let out = String::from_utf8_lossy(&dry.stdout);
    assert!(dry.status.success(), "a dry run succeeds: {dry:?}");
    assert!(out.contains("2 run(s) due to wake"), "the count: {out}");
    assert!(out.contains(&agent_run), "the agent run is listed: {out}");
    assert!(out.contains(&graph_run), "the graph run is listed: {out}");
    assert!(!out.contains(&not_due), "the run not yet due is not: {out}");
    assert!(
        out.contains("overdue by 3d") && out.contains("overdue by 2h"),
        "each run says how far past its deadline it is: {out}"
    );
    assert!(
        out.contains("nothing was driven"),
        "and the report says so plainly: {out}"
    );

    for uuid in [&agent_run, &graph_run, &not_due] {
        assert_eq!(
            log_len(&store, uuid).await,
            2,
            "a dry run appends nothing to {uuid}"
        );
    }
}

/// A due agent run is routed straight into `resume`'s own path, so it inherits
/// that verb's validation: with no `--agent`, the refusal is the one `resume`
/// gives for a run it cannot rebuild, named against this run.
///
/// A run this invocation could not drive at all is a failed drive, so the
/// command exits non-zero: the sweep did not do its job and a cron entry should
/// hear about it. The run itself is untouched and still due, so re-running with
/// the file it names is all that is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_agent_run_needs_its_definition_and_says_which() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();
    let uuid = seed_sleeping(&store, agent_head(), now - Duration::hours(1)).await;

    let woke = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&woke.stdout);
    assert!(out.contains(&uuid), "the run is named: {out}");
    assert!(
        common::flatten_wrapped_prose(&out)
            .contains("resuming an agent run needs its definition; pass --agent <file>"),
        "the refusal is resume's own, unchanged: {out}"
    );
    assert!(
        !woke.status.success(),
        "a run that could not be driven is a failed drive"
    );
    assert_eq!(
        log_len(&store, &uuid).await,
        2,
        "and the run is left exactly as it was"
    );

    // Still due, so a later invocation with the file gets another chance.
    let again = run_salvor(&store, &["wake", "--dry-run"]).await;
    assert!(
        String::from_utf8_lossy(&again.stdout).contains(&uuid),
        "the run stays due"
    );
}

/// A due graph run needs its document for the same reason a resumed one does:
/// the log records only the graph's hash. The refusal is again `resume`'s, so
/// the two verbs cannot drift on what a graph run needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_graph_run_needs_its_document() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();
    let uuid = seed_sleeping(&store, graph_head(), now - Duration::hours(1)).await;

    let woke = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&woke.stdout);
    assert!(out.contains(&uuid), "the run is named: {out}");
    assert!(
        common::flatten_wrapped_prose(&out).contains("this is a graph run; pass --graph"),
        "the refusal is resume's own: {out}"
    );
    assert!(!woke.status.success());
    assert_eq!(log_len(&store, &uuid).await, 2);
}

/// One run failing to drive does not end the sweep: every due run gets its
/// turn, each is reported, and the exit code is decided once at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_run_that_will_not_drive_does_not_stop_the_rest() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let first = seed_sleeping(&store, agent_head(), now - Duration::days(2)).await;
    let second = seed_sleeping(&store, graph_head(), now - Duration::hours(6)).await;

    let woke = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&woke.stdout);
    assert!(out.contains(&first), "the first run is reported: {out}");
    assert!(out.contains(&second), "and so is the second: {out}");
    assert!(
        out.contains("2 of 2 due run(s) could not be driven"),
        "the tally is reported: {out}"
    );
    assert!(!woke.status.success(), "a failed drive exits non-zero");

    // Ordered by deadline: the run that has been waiting longest is driven
    // first, so a sweep works through a backlog oldest-first.
    let first_at = out.find(&first).expect("the first run appears");
    let second_at = out.find(&second).expect("the second run appears");
    assert!(first_at < second_at, "the most overdue run is driven first");
}

/// `resume` refuses a run whose deadline is ahead, naming the instant and what
/// is left of the wait, and exits 1. The run is untouched, so nothing about
/// asking early costs it anything.
///
/// A run whose deadline has passed is not early, and the refusal must not
/// reach it: driving one is what waking is, and `wake` gets there by calling
/// this very command. So the due run below falls through to the drive and
/// fails for the ordinary reason a run with no `--agent` does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_refuses_a_run_that_is_not_due_yet() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    // Two hours and a minute out, so the coarsest unit the shared formatter
    // reports is a stable `2h` however long the binary takes to start.
    let early = seed_sleeping(
        &store,
        agent_head(),
        now + Duration::hours(2) + Duration::minutes(1),
    )
    .await;
    let refused = run_salvor(&store, &["resume", &early]).await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&refused.stdout));
    assert!(
        out.contains("is sleeping until") && out.contains("will not resume for another 2h"),
        "the refusal names the instant and the remaining time: {out}"
    );
    assert!(
        out.contains("salvor wake"),
        "and the command that drives what is due: {out}"
    );
    assert_eq!(
        refused.status.code(),
        Some(1),
        "an early resume is a refusal, not a success"
    );
    assert_eq!(
        log_len(&store, &early).await,
        2,
        "and it records nothing against the run"
    );

    // Past its deadline, the same command takes the drive path instead. The
    // refusal it meets there is an error, so it is on stderr, where this
    // report is not: the two are told apart by which stream they arrive on as
    // much as by what they say.
    let due = seed_sleeping(&store, agent_head(), now - Duration::hours(2)).await;
    let driven = run_salvor(&store, &["resume", &due]).await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&driven.stdout));
    let errors = common::flatten_wrapped_prose(&String::from_utf8_lossy(&driven.stderr));
    assert!(
        !out.contains("is sleeping until"),
        "a due run is never refused as sleeping: {out}"
    );
    assert!(
        errors.contains("resuming an agent run needs its definition"),
        "it reached the drive and failed for the ordinary reason: {errors}"
    );
}

/// `wake` is a real verb of the CLI, not a hidden one: it is in the root help
/// with an explanation, and its own help page names the flags it borrows from
/// `resume`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_is_documented_in_the_help() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");

    let root = run_salvor(&store, &["--help"]).await;
    let out = String::from_utf8_lossy(&root.stdout);
    assert!(out.contains("wake"), "the verb is listed: {out}");

    let page = run_salvor(&store, &["wake", "--help"]).await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&page.stdout));
    assert!(out.contains("--agent"), "the agent flag: {out}");
    assert!(out.contains("--graph"), "the graph flag: {out}");
    assert!(out.contains("--dry-run"), "the dry-run flag: {out}");
}
