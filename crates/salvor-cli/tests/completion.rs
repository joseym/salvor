//! Dynamic shell completion, driven the way a shell drives it.
//!
//! There is no interactive shell here and there does not need to be one: the
//! shell's whole side of the protocol is an environment variable plus argv
//! after a `--` (see `salvor_cli::completion`). These tests speak exactly that
//! to the real binary, against real stores seeded in temp directories, so what
//! is asserted is what a terminal would receive.
//!
//! The tests split in two, and the second half is the load-bearing one:
//!
//! - the useful behaviour: run ids for the verbs that take one, agent
//!   identities for `salvor list --agent`;
//! - the degradation: a missing store, a corrupt store, a store path that is
//!   a directory. Each must produce no candidates, no output on either stream,
//!   and exit 0. This is the property most likely to rot silently, because a
//!   store that goes unreadable in a way nobody tested reaches an operator as
//!   a Rust panic printed over their half-typed command line.

mod common;

use std::path::Path;
use std::process::Output;

use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;
use tempfile::{TempDir, tempdir};
use uuid::Uuid;

/// The agent hash seeded on the ordinary runs below.
const AGENT: &str = "sha256:fixture-agent";

/// Asks the binary to complete `words`, with the cursor in the last one.
///
/// `current_dir` is a directory with no store in it, so a test that means "no
/// store" cannot accidentally find the default `./salvor.db` of whatever
/// directory the test runner happened to start in.
fn complete(store: &Path, current_dir: &Path, shell: &str, words: &[&str]) -> Output {
    let mut command = assert_cmd::Command::cargo_bin("salvor").expect("salvor binary builds");
    command
        .current_dir(current_dir)
        .env("COMPLETE", shell)
        .env("SALVOR_STORE", store)
        .env("_SALVOR_COMPLETE_INDEX", (words.len() - 1).to_string())
        .arg("--")
        .args(words);
    command.output().expect("salvor runs")
}

/// The candidate lines of a completion, which is all a shell reads.
fn candidates(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

/// Seeds a run whose first event names `AGENT`, returning its id.
async fn seed_agent_run(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let envelope = EventEnvelope::new(
        run_id,
        SequenceNumber::new(0),
        time::OffsetDateTime::UNIX_EPOCH,
        Event::RunStarted {
            agent_def_hash: AGENT.into(),
            input: json!("salvage the waratah"),
            labels: None,
            driven_by: None,
            caller: None,
        },
    );
    store.append(&envelope).await.expect("seed append");
    run_id
}

/// Seeds a GRAPH run, whose identity is the literal `graph run`.
async fn seed_graph_run(path: &Path) -> RunId {
    let store = SqliteStore::open(path).expect("store opens");
    let run_id = RunId::from_uuid(Uuid::new_v4());
    let envelope = EventEnvelope::new(
        run_id,
        SequenceNumber::new(0),
        time::OffsetDateTime::UNIX_EPOCH,
        Event::GraphRunStarted {
            graph_hash: "sha256:fixture-graph".into(),
            input: json!({"case": "dispute"}),
            labels: None,
            forked_from: None,
        },
    );
    store.append(&envelope).await.expect("seed append");
    run_id
}

/// A store with two agent runs and one graph run, plus an empty directory to
/// run the binary from.
async fn seeded() -> (TempDir, TempDir, Vec<String>) {
    let store_dir = tempdir().expect("tempdir");
    let elsewhere = tempdir().expect("tempdir");
    let store = store_dir.path().join("salvor.db");
    let mut ids = Vec::new();
    for _ in 0..2 {
        ids.push(seed_agent_run(&store).await.as_uuid().to_string());
    }
    ids.push(seed_graph_run(&store).await.as_uuid().to_string());
    (store_dir, elsewhere, ids)
}

/// Every verb that takes a run id offers the store's run ids. One test covers
/// all six, because a verb that grows a run id later should be covered by it
/// the day it exists.
#[tokio::test]
async fn run_ids_complete_for_every_verb_that_takes_one() {
    let (store_dir, elsewhere, ids) = seeded().await;
    let store = store_dir.path().join("salvor.db");

    for verb in ["history", "replay", "abandon", "resume", "resolve", "fork"] {
        let output = complete(&store, elsewhere.path(), "zsh", &["salvor", verb, ""]);
        assert!(output.status.success(), "{verb} exits 0");
        let offered = candidates(&output);
        for id in &ids {
            assert!(
                offered.contains(id),
                "`salvor {verb} <TAB>` should offer {id}, offered {offered:?}"
            );
        }
    }
}

/// A typed prefix narrows the candidates, which is what makes completion worth
/// pressing twice.
#[tokio::test]
async fn a_typed_prefix_narrows_the_run_ids() {
    let (store_dir, elsewhere, ids) = seeded().await;
    let store = store_dir.path().join("salvor.db");
    let wanted = &ids[0];
    let prefix = &wanted[..8];

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "history", prefix],
    ));
    assert_eq!(offered, vec![wanted.clone()], "only the matching run id");
}

/// `salvor list --agent` completes what the filter actually matches on: the
/// agent definition hash of an ordinary run, and the literal `graph run` for a
/// graph, which has no single agent to name.
#[tokio::test]
async fn agent_identities_complete_for_the_list_filter() {
    let (store_dir, elsewhere, _) = seeded().await;
    let store = store_dir.path().join("salvor.db");

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "list", "--agent", ""],
    ));
    assert!(offered.contains(&AGENT.to_owned()), "{offered:?}");
    assert!(offered.contains(&"graph run".to_owned()), "{offered:?}");
    // Two agent runs share one identity, and the operator wants the set.
    assert_eq!(
        offered.len(),
        2,
        "identities are de-duplicated: {offered:?}"
    );

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "list", "--agent", "g"],
    ));
    assert_eq!(offered, vec!["graph run"]);
}

/// The same `--agent` flag on a different verb means a definition FILE, not an
/// identity, and must not be answered with identities.
#[tokio::test]
async fn agent_identities_do_not_leak_onto_the_agent_file_flag() {
    let (store_dir, elsewhere, _) = seeded().await;
    let store = store_dir.path().join("salvor.db");

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "resume", "some-run", "--agent", ""],
    ));
    assert!(
        !offered
            .iter()
            .any(|value| value == AGENT || value == "graph run"),
        "a file flag must not offer store identities: {offered:?}"
    );
}

/// A missing store: no candidates, nothing on either stream, exit 0, and no
/// store left behind, because the SQLite call underneath would create the file
/// it was asked to open.
#[tokio::test]
async fn a_missing_store_offers_nothing_and_says_nothing() {
    let elsewhere = tempdir().expect("tempdir");
    let missing = elsewhere.path().join("nowhere/salvor.db");

    for words in [
        vec!["salvor", "history", ""],
        vec!["salvor", "list", "--agent", ""],
    ] {
        let output = complete(&missing, elsewhere.path(), "zsh", &words);
        assert!(output.status.success(), "exits 0: {:?}", output.status);
        assert!(output.stdout.is_empty(), "no candidates");
        assert!(
            output.stderr.is_empty(),
            "nothing on stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!missing.exists(), "completion must not create a store");
    assert!(
        !elsewhere.path().join("salvor.db").exists(),
        "nor one in the working directory"
    );
}

/// A file that is not a database, and a store path that is a directory: the
/// same silence. These are the shapes an unreadable store actually takes on
/// disk: a truncated copy, a path pointing at the wrong thing.
#[tokio::test]
async fn an_unreadable_store_offers_nothing_and_says_nothing() {
    let elsewhere = tempdir().expect("tempdir");
    let corrupt = elsewhere.path().join("corrupt.db");
    std::fs::write(&corrupt, b"SQLite format 3\0not really, though").expect("write");
    let directory = elsewhere.path().join("a-directory.db");
    std::fs::create_dir(&directory).expect("mkdir");

    for store in [&corrupt, &directory] {
        for words in [
            vec!["salvor", "history", ""],
            vec!["salvor", "list", "--agent", ""],
        ] {
            let output = complete(store, elsewhere.path(), "zsh", &words);
            assert!(output.status.success(), "exits 0: {:?}", output.status);
            assert!(
                output.stdout.is_empty(),
                "no candidates: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                output.stderr.is_empty(),
                "nothing on stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

/// The fixed value sets and the verb list still complete, from the same
/// invocation: dynamic completion is additive, not a replacement.
#[tokio::test]
async fn static_knowledge_still_completes_through_the_dynamic_path() {
    let (store_dir, elsewhere, _) = seeded().await;
    let store = store_dir.path().join("salvor.db");

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "list", "--group", ""],
    ));
    assert_eq!(offered, vec!["waiting", "progress", "terminal"]);

    let offered = candidates(&complete(
        &store,
        elsewhere.path(),
        "zsh",
        &["salvor", "gr"],
    ));
    assert_eq!(offered, vec!["graph"]);
}

/// The registration script a user evaluates from their rc file, for each shell
/// this supports. It must name this binary, so the completion function calls
/// back into the salvor that printed it.
#[tokio::test]
async fn the_registration_script_names_this_binary() {
    let elsewhere = tempdir().expect("tempdir");
    for (shell, marker) in [
        ("zsh", "compdef _salvor_dynamic_completion salvor"),
        ("bash", "complete -o bashdefault"),
    ] {
        let mut command = assert_cmd::Command::cargo_bin("salvor").expect("binary builds");
        let output = command
            .current_dir(elsewhere.path())
            .env("COMPLETE", shell)
            .output()
            .expect("salvor runs");
        assert!(output.status.success());
        let script = String::from_utf8_lossy(&output.stdout);
        assert!(script.contains(marker), "{shell} script: {script}");
        assert!(
            script.contains("_SALVOR_COMPLETE_INDEX"),
            "{shell} script passes the cursor index: {script}"
        );
        assert!(
            script.contains(env!("CARGO_BIN_EXE_salvor")),
            "{shell} script calls back into this binary: {script}"
        );
    }
}

/// The static path is untouched: `salvor completions zsh` still prints a real
/// zsh script, and an ordinary invocation is still ordinary.
#[tokio::test]
async fn the_static_completion_script_still_works() {
    let store_dir = tempdir().expect("tempdir");
    let store = store_dir.path().join("salvor.db");

    let output = common::salvor(&store)
        .args(["completions", "zsh"])
        .output()
        .expect("salvor runs");
    assert!(output.status.success());
    let script = String::from_utf8_lossy(&output.stdout);
    assert!(script.contains("#compdef salvor"), "a zsh script: {script}");
    assert!(script.contains("_salvor()"), "with its function: {script}");
    // The static script's own knowledge of the verbs, which is what makes it
    // useful with no store in reach at all.
    assert!(script.contains("history"), "naming the verbs: {script}");
}
