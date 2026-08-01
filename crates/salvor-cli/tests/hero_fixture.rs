//! Proves the hero fixture works: drives the real `salvor` binary with
//! `--fixture examples/hero`, from the workspace root, with no API key, no
//! network, and no scripted model of the test's own: the fixture supplies all
//! three of those itself.
//!
//! This is the offline sibling of `demo_run.rs`. That test mounts a wiremock
//! model and passes `--agent`/`--input`; this one passes one flag and lets
//! `salvor run --fixture` stand the model up from `examples/hero/model.json`.
//! What is being guarded is exactly the claim the landing page makes: a reader
//! who clones the repo, runs `cargo build`, and types one command gets a
//! completed run and one recorded write.
//!
//! # How the fixture stays unmodified
//!
//! One environment hook, plumbed through ordinary environment inheritance
//! (test -> salvor -> spawned MCP child): `SALVOR_HERO_CLAIMS` points the
//! `salvor-hero-tools` server's claims file at a temp path, keeping the
//! repository clean. The model endpoint needs no hook at all: that is what
//! `--fixture` is for; it exports `SALVOR_HERO_BASE_URL` (the variable
//! `examples/hero/agent.toml` declares) at its own in-process server.
//!
//! The fixture's MCP `command` is the target-relative path a reader gets from
//! `cargo build`, so the test runs `salvor` from the workspace root and first
//! makes sure the binary exists at that relative location (copying the
//! Cargo-built one there when a custom target dir moved it), exactly as
//! `demo_run.rs` does for `salvor-demo-research`.
//!
//! # The two things asserted
//!
//! - The claims file holds exactly **one** line. It is the write ledger: one
//!   line per real `save_claim` execution, appended by a process outside
//!   salvor. One line is the whole zero-duplicate-write claim in one number.
//! - The event log is the exact ten-event shape below. Pinning the full
//!   sequence, not a subset, is what makes an accidental extra model call or a
//!   missing write visible as a failure rather than a slightly different log
//!   nobody reads.

mod common;

use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::tempdir;

/// The workspace root, two levels above this crate's manifest. The fixture is
/// documented as running from here (its MCP `command` is a target-relative
/// path), so the test does too.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two levels under the workspace root")
        .to_owned()
}

/// Makes sure `target/debug/salvor-hero-tools` exists relative to the
/// workspace root, which is the path `examples/hero/agent.toml` names. Under
/// the default layout the Cargo-built binary already lives there; under a
/// custom CARGO_TARGET_DIR it is copied into place.
fn ensure_hero_tools_at_documented_path(root: &Path) {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_salvor-hero-tools"));
    let documented = root.join("target/debug/salvor-hero-tools");
    if built.canonicalize().ok() == documented.canonicalize().ok() {
        return;
    }
    std::fs::create_dir_all(documented.parent().expect("parent exists"))
        .expect("target/debug creates");
    std::fs::copy(&built, &documented).expect("hero tools server copies into place");
}

/// The event kinds of a run's log, read back through `salvor history --json`
/// (the same mechanism `cli_e2e.rs` uses to inspect a log from outside the
/// process that wrote it).
fn event_kinds(json: &str) -> Vec<String> {
    let log: Value = serde_json::from_str(json).expect("history --json is JSON");
    log.as_array()
        .expect("history --json is an array")
        .iter()
        .map(|envelope| {
            envelope["event"]["kind"]
                .as_str()
                .expect("every envelope names its event kind")
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hero_fixture_runs_offline_and_writes_exactly_once() {
    let root = workspace_root();
    ensure_hero_tools_at_documented_path(&root);

    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let claims = dir.path().join("claims.txt");

    // Drive the real binary from the workspace root with the unmodified
    // fixture, off the async runtime so a blocking process wait does not
    // starve this test's own runtime.
    let output = {
        let root = root.clone();
        let store = store.clone();
        let claims = claims.clone();
        tokio::task::spawn_blocking(move || {
            let mut command =
                assert_cmd::Command::cargo_bin("salvor").expect("salvor binary builds");
            command
                .current_dir(&root)
                .arg("--store")
                .arg(&store)
                .args(["run", "--fixture", "examples/hero"])
                .env("RUST_LOG", "warn")
                .env("SALVOR_HERO_CLAIMS", &claims)
                // Two removals prove the offline claim on a developer machine
                // that has both exported: no key is needed, and --fixture sets
                // the base URL itself rather than inheriting a stale one.
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("SALVOR_HERO_BASE_URL");
            command.output().expect("salvor runs")
        })
        .await
        .expect("blocking task joins")
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the hero fixture run completes: {output:?}"
    );
    assert!(
        stdout.contains("1 claim recorded for ss-waratah."),
        "the scripted final answer is printed: {stdout}"
    );

    // The claims file is the write ledger: exactly one line per real
    // `save_claim` execution.
    let recorded = std::fs::read_to_string(&claims).expect("claims file exists");
    assert_eq!(
        recorded.lines().count(),
        1,
        "exactly one claim was written: {recorded:?}"
    );

    // The log, read back through the CLI itself.
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .expect("run prints its id first")
        .trim()
        .to_owned();
    let history = common::run_salvor(&store, &["history", &run_id, "--json"]).await;
    assert!(history.status.success(), "history reads back: {history:?}");
    let kinds = event_kinds(&String::from_utf8_lossy(&history.stdout));

    // The full recorded shape of a two-turn run with one write in the middle.
    //
    // Note the SECOND `NowObserved`: the driver records one clock observation
    // per iteration (see `salvor_runtime::driver`), so a two-turn run has two
    // of them and ten events, not the nine a turn-by-turn reading suggests.
    // The prose in `examples/hero/agent.toml` says "Nine events"; ten is what
    // the runtime actually records, and this test pins the truth.
    assert_eq!(
        kinds,
        vec![
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted",
        ]
    );
}
