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
//!
//! # The tests here that run no binary
//!
//! What a sweep says about a drive that came back with an error (a genuine
//! failure of this invocation, or a run another driver got to first) turns on
//! a decision that cannot be staged from out here: it takes two wakers racing
//! at one run, down to the millisecond. The decision is a pure function of the
//! error and of what the run looked like before and after, so it is tested as
//! one, against the error a losing writer gets and the states a winning driver
//! leaves behind. What that decision then gets PRINTED as is its own pure
//! function of the folded status alone, tested the same way.

mod common;

use std::path::Path;

use common::run_salvor;
use salvor_cli::commands::{
    FailedWake, ReadTiming, classify_failed_wake, describe_taken, describe_unreadable,
};
use salvor_core::{Event, EventEnvelope, RunId, RunStatus, SequenceNumber};
use salvor_engine::EngineError;
use salvor_graph::Graph;
use salvor_runtime::RuntimeError;
use salvor_store::{EventStore, SqliteStore, StoreError};
use serde_json::json;
use tempfile::tempdir;
use time::{Duration, OffsetDateTime};

/// A single-gate graph document: it needs neither a model nor a tool, so a run
/// seeded against it can be checked against the real file entirely offline.
const GATE_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "gate", "payload": { "id": "approve", "approval_schema": {
      "type": "object",
      "properties": { "approved": { "type": "boolean" } }
    } } }
  ],
  "edges": []
}"#;

/// A different valid graph, so a `--graph` pointing at it is a real document
/// that hashes to something else: the mismatch under test is the hash, not a
/// parse failure.
const OTHER_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "gate", "payload": { "id": "sign-off", "approval_schema": {
      "type": "object",
      "properties": { "approved": { "type": "boolean" } }
    } } }
  ],
  "edges": []
}"#;

/// A single-tool graph: no model in its own loop, so the tool it names can
/// only come from an agent's tool set, which is exactly what a bare
/// `wake --dry-run` (no `--agent` at all) has none of to check it against.
const TOOL_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [ { "kind": "tool", "payload": { "id": "step", "tool": "missing" } } ],
  "edges": []
}"#;

/// The hash a run records for a graph document, computed with the engine's own
/// function so a seeded log and a real file on disk agree the way the binary
/// makes them agree.
fn hash_of(document: &str) -> String {
    let graph: Graph = serde_json::from_str(document).expect("a valid graph document");
    salvor_engine::graph_hash(&graph).expect("the graph hashes")
}

/// Writes `document` into `dir` under `name` and returns the path.
fn write_graph(dir: &Path, name: &str, document: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, document).expect("write");
    path
}

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
    graph_head_recording("sha256:not-supplied-anywhere")
}

/// A graph run's head recording a particular document hash, for the cases that
/// hand `wake` a real file and ask whether it is the one the run started with.
fn graph_head_recording(graph_hash: &str) -> Event {
    Event::GraphRunStarted {
        graph_hash: graph_hash.to_owned(),
        input: json!("go"),
        labels: None,
        forked_from: None,
    }
}

/// Runs the `salvor` binary with a chosen `RUST_LOG`, so a test can read the
/// progress log it writes to stderr. Otherwise exactly `common::run_salvor`,
/// which quiets that log to keep it out of the way.
async fn run_salvor_logging(store: &Path, level: &str, args: &[&str]) -> std::process::Output {
    let store = store.to_owned();
    let level = level.to_owned();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let mut command = common::salvor(&store);
        command.env("RUST_LOG", level);
        command.args(&args);
        command.output().expect("salvor runs")
    })
    .await
    .expect("blocking task joins")
}

/// An ordinary drive failure: nothing about it says a second writer is
/// involved, so the run's own state has to answer for it.
fn drive_error() -> anyhow::Error {
    anyhow::anyhow!("the model call failed after the client's own retries")
}

/// The store refusing this drive's append because the position was already
/// taken, wearing the two coats a real one arrives in: the runtime's
/// `Store` variant (which carries the store error by value, not as a source)
/// under a layer of anyhow context.
fn position_conflict() -> anyhow::Error {
    anyhow::Error::new(RuntimeError::Store(StoreError::Conflict {
        run_id: RunId::new(),
        seq: SequenceNumber::new(3),
    }))
    .context("re-driving the run")
}

/// The same refusal as a GRAPH drive hands it up: one more coat, the engine's
/// `Runtime` variant, which is `#[error(transparent)]` and therefore hides
/// both itself and the runtime error from a plain walk of the `source` chain.
fn graph_position_conflict() -> anyhow::Error {
    anyhow::Error::from(EngineError::Runtime(RuntimeError::Store(
        StoreError::Conflict {
            run_id: RunId::new(),
            seq: SequenceNumber::new(8),
        },
    )))
}

/// How many events a run's log holds, for proving a dry run drove nothing.
async fn log_len(store_path: &Path, uuid: &str) -> usize {
    let store = SqliteStore::open(store_path).expect("store opens");
    let run_id = RunId::from_uuid(uuid.parse().expect("a uuid"));
    store.read_log(run_id).await.expect("log reads").len()
}

/// A store with nothing sleeping in it at all says so plainly and exits 0.
/// This is the one case `nothing to wake` can call "no timers": a bare store
/// has no run to name a deadline for.
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
        out.contains(&format!(
            "nothing to wake: no run in {} is sleeping",
            store.display()
        )),
        "no run at all is told apart from a run not yet due: {out}"
    );
}

/// A store where nothing is DUE, but something IS sleeping, says which run and
/// when it comes due instead of the "no run is sleeping" wording: that is the
/// one thing an operator staring at a quiet cron entry actually wants to know.
/// Exit stays 0, because a deadline not yet reached is not a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_due_names_the_next_deadline_when_a_run_is_sleeping_ahead() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    // Three hours and a minute out, so the coarsest unit the shared formatter
    // reports is a stable `3h`, however long the binary takes to start; see
    // `resume_refuses_a_run_that_is_not_due_yet` for why only that prefix is
    // pinned rather than the whole `3h Nm` span.
    let later = seed_sleeping(
        &store,
        agent_head(),
        now + Duration::hours(3) + Duration::minutes(1),
    )
    .await;
    // A second run, sleeping further out still, so the message names the
    // EARLIEST deadline, not merely some sleeping run's.
    seed_sleeping(&store, graph_head(), now + Duration::days(30)).await;

    let woke = run_salvor(&store, &["wake"]).await;
    let out = String::from_utf8_lossy(&woke.stdout);
    assert!(woke.status.success(), "not due is not a failure: {out}");
    assert!(
        out.contains(&format!(
            "nothing to wake: the next run in {} is due at",
            store.display()
        )),
        "the store and the situation are named: {out}"
    );
    assert!(
        out.contains("(in 3h"),
        "and how long is left, against the earlier of the two: {out}"
    );
    assert_eq!(
        log_len(&store, &later).await,
        2,
        "asking merely reports the deadline; nothing was appended"
    );

    // The sleeping run is visible in `list` all the same, under its own label
    // and its own group: sleeping is motion, not a to-do item.
    let listed = run_salvor(&store, &["list", "--status", "sleeping"]).await;
    let out = String::from_utf8_lossy(&listed.stdout);
    assert!(out.contains(&later) && out.contains("sleeping"), "{out}");
    let grouped = run_salvor(&store, &["list", "--group", "progress"]).await;
    assert!(
        String::from_utf8_lossy(&grouped.stdout).contains(&later),
        "a sleeping run is in the progress group"
    );
}

/// `--dry-run` names every due run, how overdue it is, what the log says it is,
/// and whether the files given would wake it, then appends nothing, exactly as
/// `salvor fork --dry-run` previews a fork it does not create. Both an agent
/// run and a graph run appear: what is due is a question about the log's
/// deadline, not about what kind of run it is.
///
/// No `--agent` and no `--graph` are passed here, so neither run could be
/// woken and the dry run exits 1: the whole point of previewing a crontab line
/// is that running it says whether the line works.
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
    let flat = common::flatten_wrapped_prose(&out);
    assert!(out.contains("2 run(s) due as of"), "the count: {out}");
    assert!(out.contains(&agent_run), "the agent run is listed: {out}");
    assert!(out.contains(&graph_run), "the graph run is listed: {out}");
    assert!(!out.contains(&not_due), "the run not yet due is not: {out}");
    assert!(
        out.contains("overdue by 3d") && out.contains("overdue by 2h"),
        "each run says how far past its deadline it is: {out}"
    );
    assert!(
        flat.contains("agent run, recorded definition sha256:not-registered-anywhere"),
        "an agent run says what it is and what it recorded: {out}"
    );
    assert!(
        flat.contains("graph run, recorded document sha256:not-supplied-anywhere"),
        "and so does a graph run: {out}"
    );
    assert!(
        flat.contains(
            "cannot be woken with these files: resuming an agent run needs its definition"
        ),
        "with no --agent, the agent run reports the refusal a real wake gives: {out}"
    );
    assert!(
        flat.contains("cannot be woken with these files: this is a graph run; pass --graph"),
        "and with no --graph, so does the graph run: {out}"
    );
    assert!(
        out.contains("nothing was driven"),
        "and the report says so plainly: {out}"
    );
    assert_eq!(
        dry.status.code(),
        Some(1),
        "neither due run could be woken with the files given: {out}"
    );

    for uuid in [&agent_run, &graph_run, &not_due] {
        assert_eq!(
            log_len(&store, uuid).await,
            2,
            "a dry run appends nothing to {uuid}"
        );
    }
}

/// The point of `--dry-run` is that it answers the question a real wake asks,
/// against the files an operator is about to put in a crontab, without driving
/// anything. So a due graph run is checked against the `--graph` given, and the
/// three answers are the three a real wake would give, in a real wake's words:
/// no document at all, the wrong document, and the one the run recorded.
///
/// The exit code carries the same answer, so `salvor wake --dry-run ...` is a
/// smoke test for the line: 1 while a due run could not be woken with these
/// files, 0 once every one of them could.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_checks_a_due_graph_run_against_the_document_given() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let recorded = hash_of(GATE_GRAPH);
    let graph = write_graph(dir.path(), "gate.json", GATE_GRAPH);
    let other = write_graph(dir.path(), "other.json", OTHER_GRAPH);
    let uuid = seed_sleeping(
        &store,
        graph_head_recording(&recorded),
        now - Duration::hours(1),
    )
    .await;

    // No --graph: the run needs one, and says so in resume's own words.
    let bare = run_salvor(&store, &["wake", "--dry-run"]).await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&bare.stdout));
    assert!(
        out.contains(&format!("graph run, recorded document {recorded}")),
        "the preview names the kind and the hash the log recorded: {out}"
    );
    assert!(
        out.contains("cannot be woken with these files: this is a graph run; pass --graph"),
        "and the refusal is the one a real wake gives: {out}"
    );
    assert_eq!(bare.status.code(), Some(1), "which is a failed preview");

    // The wrong document: a valid graph, but not the one this run started
    // with. The refusal is the hash mismatch, both hashes named.
    let wrong = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--graph",
            other.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&wrong.stdout));
    assert!(
        out.contains(&format!("hashes to {}", hash_of(OTHER_GRAPH)))
            && out.contains(&format!("recorded {recorded}")),
        "the mismatch names what was supplied and what the run recorded: {out}"
    );
    assert_eq!(wrong.status.code(), Some(1), "a wrong document cannot wake");

    // The document the run recorded: it would wake, and the preview names the
    // file it would be re-driven from.
    let right = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--graph",
            graph.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&right.stdout));
    assert!(
        out.contains(&format!("would wake with {}", graph.display())),
        "the preview names the file that satisfies the run: {out}"
    );
    assert!(
        right.status.success(),
        "every due run can be woken with these files: {out}"
    );

    assert_eq!(
        log_len(&store, &uuid).await,
        2,
        "and none of the three previews drove anything"
    );
}

/// A dry run checks every `--agent` file it was given, not only the ones a
/// due run's own kind happens to use. A due GRAPH run's readiness never looks
/// at `--agent` at all, so a typo in an agent path an operator also put on
/// the same crontab line would otherwise preview clean and only surface once
/// some agent run needed it. The bad file is reported once, on its own line,
/// not repeated against the due run that never needed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_also_checks_every_agent_file_even_when_only_a_graph_run_is_due() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let recorded = hash_of(GATE_GRAPH);
    let graph = write_graph(dir.path(), "gate.json", GATE_GRAPH);
    let uuid = seed_sleeping(
        &store,
        graph_head_recording(&recorded),
        now - Duration::hours(1),
    )
    .await;
    let missing_agent = dir.path().join("nope.toml");

    let dry = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--graph",
            graph.to_str().expect("a utf-8 path"),
            "--agent",
            missing_agent.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&dry.stdout));
    assert!(
        out.contains(&format!("--agent {}", missing_agent.display())),
        "the bad agent file is named: {out}"
    );
    assert!(
        out.contains("reading agent file"),
        "and the load error that stopped it: {out}"
    );
    assert!(
        out.contains(&format!("would wake with {}", graph.display())),
        "the due graph run is still reported ready: it never needed --agent: {out}"
    );
    assert_eq!(
        dry.status.code(),
        Some(1),
        "a bad file given fails the preview even though the due run itself is ready: {out}"
    );
    assert_eq!(log_len(&store, &uuid).await, 2, "a dry run drives nothing");
}

/// The mirror of the test above: a due AGENT run's readiness never looks at
/// `--graph`, so an unparsable graph document given alongside it would
/// otherwise preview clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_also_checks_the_graph_file_even_when_only_an_agent_run_is_due() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let agent = dir.path().join("agent.toml");
    std::fs::write(&agent, "model = \"claude-test-model\"\n").expect("write agent toml");
    let uuid = seed_sleeping(&store, agent_head(), now - Duration::hours(1)).await;
    let bad_graph = write_graph(dir.path(), "bad.json", "{ not json");

    let dry = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--agent",
            agent.to_str().expect("a utf-8 path"),
            "--graph",
            bad_graph.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&dry.stdout));
    assert!(
        out.contains(&format!("--graph {}", bad_graph.display())),
        "the bad graph file is named: {out}"
    );
    assert!(
        out.contains("not a valid graph document"),
        "and the parse error that stopped it: {out}"
    );
    assert!(
        out.contains(&format!("would wake with {}", agent.display())),
        "the due agent run is still reported ready: it never needed --graph: {out}"
    );
    assert_eq!(
        dry.status.code(),
        Some(1),
        "a bad file given fails the preview even though the due run itself is ready: {out}"
    );
    assert_eq!(log_len(&store, &uuid).await, 2, "a dry run drives nothing");
}

/// A due graph run whose document has a `tool` node needs an agent to carry
/// that tool; with no `--agent` at all there is nothing for the drive to ever
/// check the tool name against, and a dry run says so up front rather than
/// reporting the run "ready" and letting the real wake discover the same gap
/// a moment later. With a suitable `--agent` supplied, the preview goes back
/// to reporting ready: it only rules out having no agent at all, the rest of
/// tool resolution is still the drive's own job. A real wake with that same
/// `--agent` proves it: the file was given, so the preview clears it, but the
/// file carries no tools at all, so the drive itself refuses and names the
/// fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_blocks_a_graph_with_tool_nodes_when_no_agent_is_given() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let recorded = hash_of(TOOL_GRAPH);
    let graph = write_graph(dir.path(), "tool.json", TOOL_GRAPH);
    let uuid = seed_sleeping(
        &store,
        graph_head_recording(&recorded),
        now - Duration::hours(1),
    )
    .await;

    let bare = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--graph",
            graph.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&bare.stdout));
    assert!(out.contains(&uuid), "the run is named: {out}");
    assert!(
        out.contains("cannot be woken with these files:") && out.contains("tool node `step`"),
        "the refusal names the tool node, with no agent built to check it against: {out}"
    );
    assert_eq!(
        bare.status.code(),
        Some(1),
        "a graph with an unreachable tool node cannot be woken"
    );
    assert_eq!(log_len(&store, &uuid).await, 2, "a dry run drives nothing");

    // A suitable --agent given: the no-agent block clears, and the preview
    // reports the run ready again, exactly as any other graph run would be.
    let agent = dir.path().join("agent.toml");
    std::fs::write(&agent, "model = \"claude-test-model\"\n").expect("write agent toml");
    let ready = run_salvor(
        &store,
        &[
            "wake",
            "--dry-run",
            "--graph",
            graph.to_str().expect("a utf-8 path"),
            "--agent",
            agent.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&ready.stdout));
    assert!(
        out.contains(&format!("would wake with {}", graph.display())),
        "an --agent given clears the block, whether or not it actually carries the tool: {out}"
    );
    assert!(ready.status.success(), "the preview now passes: {out}");
    assert_eq!(log_len(&store, &uuid).await, 2, "still nothing driven");

    // A real wake with that same agent file: the preview's block is cleared,
    // but the agent still carries no tools, so the drive itself refuses, and
    // this time the refusal points at the fix rather than leaving an operator
    // to guess it.
    let woke = run_salvor(
        &store,
        &[
            "wake",
            "--graph",
            graph.to_str().expect("a utf-8 path"),
            "--agent",
            agent.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let out = common::flatten_wrapped_prose(&String::from_utf8_lossy(&woke.stdout));
    assert!(
        out.contains(
            "tool node `step` names tool `missing`, which none of the provided agents \
                       carry; pass --agent with an agent file whose tools include it"
        ),
        "a real wake refuses the same gap and names the fix: {out}"
    );
    assert_eq!(woke.status.code(), Some(1), "a failed drive exits non-zero");
    assert_eq!(
        log_len(&store, &uuid).await,
        2,
        "the refusal drives nothing"
    );
}

/// The reporting decision a sweep makes when a drive comes back with an error,
/// tested at the seam where it is made rather than by staging a race.
///
/// Two `salvor wake` processes at one due run is ordinary operation: the store
/// refuses the loser while the winner takes the run. What the loser must never
/// do is print its own error as the run's news and tell an operator to re-drive
/// a run another driver is finishing. Nothing here reads the error's text; two
/// typed signals decide it. A store conflict on an append names the race
/// outright, whatever the run reads as at that instant, because only a second
/// writer can produce one. Failing that, the run's own state answers, on the
/// rule that a drive which parked or finished a run returns success: a run
/// found parked, completed, abandoned, or asleep on a NEW deadline was driven
/// there by something else, while every other state is one this failing drive
/// could have left itself.
#[test]
fn a_failed_drive_tells_a_lost_race_apart_from_a_drive_that_failed() {
    let due_at = OffsetDateTime::now_utc() - Duration::hours(1);

    // Untouched: this invocation genuinely could not drive the run, which is
    // the failure a cron entry should hear about.
    assert_eq!(
        classify_failed_wake(
            &drive_error(),
            due_at,
            2,
            &RunStatus::Sleeping { wake_at: due_at },
            2
        ),
        FailedWake::NotWoken
    );

    // The loser of a race, caught mid-flight: it opened the store while the
    // winner was still driving, so the run reads `running` and only the
    // store's own refusal says whose work that was.
    assert_eq!(
        classify_failed_wake(&position_conflict(), due_at, 2, &RunStatus::Running, 4),
        FailedWake::TakenByAnotherDriver
    );

    // The same race lost by a GRAPH run, whose drive wraps the refusal one
    // layer deeper. The run reads `awaiting-tool` here, which the status rule
    // alone would call this sweep's own half-driven mess, so this case fails
    // unless the conflict is recognized through both coats.
    assert_eq!(
        classify_failed_wake(
            &graph_position_conflict(),
            due_at,
            2,
            &RunStatus::AwaitingTool,
            9
        ),
        FailedWake::TakenByAnotherDriver
    );

    // The loser of a race, arriving late: no append of its own to conflict
    // with, but the winner drove the run to completion, which no failing drive
    // of ours could have done.
    assert_eq!(
        classify_failed_wake(
            &drive_error(),
            due_at,
            2,
            &RunStatus::Completed {
                output: json!("done")
            },
            6
        ),
        FailedWake::TakenByAnotherDriver
    );

    // The winner woke it and it parked at a gate: again a state only a drive
    // that succeeded leaves behind.
    assert_eq!(
        classify_failed_wake(
            &drive_error(),
            due_at,
            2,
            &RunStatus::Suspended {
                reason: "approve the payout".to_owned(),
                input_schema: json!({ "type": "object" }),
                kind: None
            },
            5
        ),
        FailedWake::TakenByAnotherDriver
    );

    // The winner woke it and it went back to sleep on a later timer: the new
    // deadline is the proof that a drive got past this one.
    assert_eq!(
        classify_failed_wake(
            &drive_error(),
            due_at,
            2,
            &RunStatus::Sleeping {
                wake_at: due_at + Duration::days(1)
            },
            4
        ),
        FailedWake::TakenByAnotherDriver
    );

    // A drive that broke partway is NOT a lost race, however much the run
    // moved: this sweep may well be what left it half-driven, so it stays a
    // failure and keeps the triage that says the run is resumable.
    assert_eq!(
        classify_failed_wake(&drive_error(), due_at, 2, &RunStatus::AwaitingTool, 5),
        FailedWake::NotWoken
    );
    assert_eq!(
        classify_failed_wake(
            &drive_error(),
            due_at,
            2,
            &RunStatus::Failed {
                error: "the graph refuses on every drive".to_owned()
            },
            4
        ),
        FailedWake::NotWoken
    );
}

/// What a sweep prints for a run [`FailedWake::TakenByAnotherDriver`] was
/// classified for, tested at the same seam as `classify_failed_wake` itself.
///
/// A folded status only earns a place in the sentence when it is one a
/// FINISHED or PARKED drive leaves behind (a race the winner has already
/// settled). Every other status could just as well be the other driver's
/// still-open write, caught mid-flight by this sweep's re-fold, so naming it
/// (`needs-reconciliation`, `running`, ...) would read as a diagnosis of a run
/// that is merely being worked on by someone else, not this sweep's business
/// to report.
#[test]
fn a_taken_run_names_its_status_only_when_a_finished_drive_could_have_left_it() {
    // Finished or parked: the status is real news, so it is named.
    assert_eq!(
        describe_taken(&RunStatus::Completed {
            output: json!("done")
        }),
        "was picked up by another driver and is now completed"
    );
    assert_eq!(
        describe_taken(&RunStatus::Failed {
            error: "the graph refuses on every drive".to_owned()
        }),
        "was picked up by another driver and is now failed"
    );
    assert_eq!(
        describe_taken(&RunStatus::Abandoned {
            reason: None,
            unresolved_write: None
        }),
        "was picked up by another driver and is now abandoned"
    );
    assert_eq!(
        describe_taken(&RunStatus::Suspended {
            reason: "approve the payout".to_owned(),
            input_schema: json!({ "type": "object" }),
            kind: None
        }),
        "was picked up by another driver and is now suspended"
    );
    assert_eq!(
        describe_taken(&RunStatus::BudgetExceeded {
            budget: salvor_core::Budget {
                kind: salvor_core::BudgetKind::CostUsd,
                limit: 1.0
            },
            observed: 2.0
        }),
        "was picked up by another driver and is now budget-exceeded"
    );
    assert_eq!(
        describe_taken(&RunStatus::Sleeping {
            wake_at: OffsetDateTime::now_utc() + Duration::days(1)
        }),
        "was picked up by another driver and is now sleeping"
    );

    // Still in motion: the other driver's own dangling write, not this
    // sweep's news to report, so the status stays out of the sentence.
    for status in [
        RunStatus::Running,
        RunStatus::AwaitingModel,
        RunStatus::AwaitingTool,
        RunStatus::NeedsReconciliation,
        RunStatus::NotStarted,
    ] {
        assert_eq!(
            describe_taken(&status),
            "was picked up by another driver, which is still driving it; \
             this sweep recorded nothing",
            "status {status:?} must not be named"
        );
    }
}

/// A run whose log this sweep could not read at all, before or after the
/// drive, is reported as not woken, naming the run and the error the store
/// gave, and never inventing a status the read never produced. Unlike
/// `classify_failed_wake`, there is no decision to make here: a read that
/// fails leaves nothing to classify against, so it is always `NotWoken`, and
/// the only thing left to get right is which of the two moments it names.
///
/// This pins the wording at the seam where it is produced, the same way
/// `a_failed_drive_tells_a_lost_race_apart_from_a_drive_that_failed` pins the
/// classification: proving the sweep actually moves on to the NEXT due run
/// after a read like this fails needs a store that fails one read mid-sweep,
/// which is the same distance from a hermetic test as staging two real
/// wakers racing at one run is for that other test (see this module's docs).
/// This crate's own fix keeps the run's whole line inside a `match` that
/// `continue`s past a read error rather than propagating it with `?`, the
/// identical shape the neighbouring `NotWoken` branch (proven not to stop a
/// sweep by `one_run_that_will_not_drive_does_not_stop_the_rest`) already
/// uses.
#[test]
fn an_unreadable_log_names_the_run_and_which_read_failed() {
    let error = StoreError::Backend("disk I/O error".to_owned());
    let uuid = "11111111-1111-1111-1111-111111111111";

    let before = describe_unreadable(uuid, ReadTiming::BeforeTheDrive, &error);
    assert!(before.contains(uuid), "the run is named: {before}");
    assert!(
        before.contains("was not woken") && before.contains("before driving it"),
        "the moment is named: {before}"
    );
    assert!(
        before.contains("disk I/O error"),
        "the store's own error rides along: {before}"
    );

    let after = describe_unreadable(uuid, ReadTiming::AfterTheDrive, &error);
    assert!(
        after.contains("could not re-read") && after.contains("after the drive"),
        "the other moment reads distinctly: {after}"
    );
    assert!(
        after.contains("disk I/O error"),
        "and its error too: {after}"
    );
}

/// A run woken on schedule has not crashed, and the log must not say it has.
/// Every timer wake takes the recover path, because continuing a sleeping run
/// and continuing a crashed one are the same act; only the wording separates
/// them, and an operator reading "recovering crashed run" out of a nightly
/// cron entry goes looking for a fault that never happened.
///
/// Both kinds of run are woken here, so both wordings are pinned in one pass.
/// Neither drive gets far (a seeded log replays against a definition and a
/// document it never really ran), which is beside the point: the line under
/// test is written before the drive, and WHY the run is being driven is
/// settled before then too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_wake_says_it_is_waking_a_sleeping_run() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let now = OffsetDateTime::now_utc();

    let agent = dir.path().join("agent.toml");
    std::fs::write(&agent, "model = \"claude-test-model\"\n").expect("write agent toml");
    let graph = write_graph(dir.path(), "gate.json", GATE_GRAPH);
    let agent_run = seed_sleeping(&store, agent_head(), now - Duration::hours(2)).await;
    seed_sleeping(
        &store,
        graph_head_recording(&hash_of(GATE_GRAPH)),
        now - Duration::hours(1),
    )
    .await;

    let woke = run_salvor_logging(
        &store,
        "info",
        &[
            "wake",
            "--agent",
            agent.to_str().expect("a utf-8 path"),
            "--graph",
            graph.to_str().expect("a utf-8 path"),
        ],
    )
    .await;
    let errors = String::from_utf8_lossy(&woke.stderr);
    assert!(
        errors.contains("waking sleeping run"),
        "a due agent run is woken, not recovered: {errors}"
    );
    assert!(
        errors.contains("waking sleeping graph run"),
        "and so is a due graph run: {errors}"
    );
    assert!(
        !errors.contains("recovering crashed"),
        "nothing in the log calls a scheduled wake a crash: {errors}"
    );
    assert_eq!(
        log_len(&store, &agent_run).await,
        2,
        "the agent run's drive appended nothing, so it is still due"
    );
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
        common::flatten_wrapped_prose(&out).contains(
            "2 of 2 due run(s) could not be driven; every due run was tried, and each line \
             above says what it needs"
        ),
        "the tally is reported, pointing back at the per-run lines: {out}"
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
    // reports is a stable `2h`, however long the binary takes to start; the
    // formatter's own finer unit (minutes) may drift by however many seconds
    // that start-up eats into the one minute of slack, so the assertion below
    // only pins the `2h` prefix, not the whole `2h Nm` span.
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
