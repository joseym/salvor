//! Proves the demo assets work: drives the real `salvor` binary with the
//! unmodified `demo/agent.toml` and `demo/input.json` against a scripted
//! wiremock model, through all twenty turns to completion, and asserts the
//! findings file holds exactly the nine expected lines. Rehearse the demo
//! with this test before recording anything with it.
//!
//! # How the demo stays unmodified
//!
//! Two environment hooks make the repository-checked assets testable
//! without edits, both plumbed through ordinary environment inheritance
//! (test -> salvor -> spawned MCP child):
//!
//! - `SALVOR_DEMO_BASE_URL` routes model calls to the wiremock server (the
//!   agent file's `base_url_env` setting names it).
//! - `SALVOR_DEMO_FINDINGS` points the `salvor-demo-research` server's
//!   findings file at a temp path, keeping the repository clean.
//!
//! The agent file's MCP `command` is the target-relative path a demo user
//! gets from `cargo build`, so the test runs `salvor` from the workspace
//! root and first makes sure the binary exists at that relative location
//! (copying the Cargo-built one there when a custom target dir moved it).
//!
//! # The scripted conversation
//!
//! The model script mirrors the system prompt's procedure: for each of the
//! nine subtopics a `search_notes` call then a `save_finding` call (turns
//! 1..=18), one `get_finding_count` (turn 19), and a final text summary
//! (turn 20). Responses are selected by the number of messages in the
//! request (turn k carries 2k - 1), the same replay-safe scheme the other
//! CLI tests use. The script itself lives in
//! [`salvor_cli::demo_script`](salvor_cli::demo_script), shared verbatim with
//! the `salvor-demo-model` server that records the GIF, so the rehearsal and
//! the recording can never drift apart.

mod common;

use std::path::{Path, PathBuf};

use common::GateModel;
use salvor_cli::demo_script::{SUBTOPICS, finding_line, script};
use tempfile::tempdir;

/// The workspace root, two levels above this crate's manifest. The demo is
/// documented as running from here, so the test does too.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two levels under the workspace root")
        .to_owned()
}

/// Makes sure `target/debug/salvor-demo-research` exists relative to the
/// workspace root, which is the path `demo/agent.toml` names. Under the
/// default layout the Cargo-built binary already lives there; under a
/// custom CARGO_TARGET_DIR it is copied into place.
fn ensure_demo_server_at_documented_path(root: &Path) {
    let built = PathBuf::from(env!("CARGO_BIN_EXE_salvor-demo-research"));
    let documented = root.join("target/debug/salvor-demo-research");
    if built.canonicalize().ok() == documented.canonicalize().ok() {
        return;
    }
    std::fs::create_dir_all(documented.parent().expect("parent exists"))
        .expect("target/debug creates");
    std::fs::copy(&built, &documented).expect("demo server copies into place");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_assets_drive_a_twenty_step_run_to_completion() {
    let root = workspace_root();
    ensure_demo_server_at_documented_path(&root);

    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let findings = dir.path().join("findings.txt");

    // The twenty-turn script (turn k keyed by 2k - 1 messages) is the shared
    // one the recording server also serves; mounting it here proves the demo
    // assets drive the same conversation the GIF shows.
    let model = GateModel::mount(script()).await;

    // Drive the real binary from the workspace root with the unmodified
    // demo assets, off the async runtime so the in-process wiremock server
    // is not starved while the child runs.
    let model_uri = model.uri();
    let output = {
        let root = root.clone();
        let store = store.clone();
        let findings = findings.clone();
        tokio::task::spawn_blocking(move || {
            let mut command = assert_cmd::Command::cargo_bin("salvor").expect("salvor binary builds");
            command
                .current_dir(&root)
                .arg("--store")
                .arg(&store)
                .args([
                    "run",
                    "--agent",
                    "demo/agent.toml",
                    "--input",
                    "@demo/input.json",
                ])
                .env("RUST_LOG", "warn")
                .env("SALVOR_DEMO_BASE_URL", &model_uri)
                .env("SALVOR_DEMO_FINDINGS", &findings)
                // Keep the rehearsal hermetic even on a machine with a real
                // key exported: the mock endpoint needs none.
                .env_remove("ANTHROPIC_API_KEY");
            command.output().expect("salvor runs")
        })
        .await
        .expect("blocking task joins")
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the demo run completes: {output:?}"
    );
    assert!(
        stdout.contains("Research complete"),
        "the final summary is printed: {stdout}"
    );

    // Exactly twenty model calls reached the mock: the ~20-step demo shape,
    // none replayed (this run was uninterrupted).
    let requests = model.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 20, "twenty model calls were made");

    // The findings file is the write ledger: exactly one line per
    // save_finding execution, in prompt order.
    let expected: String = SUBTOPICS
        .iter()
        .map(|subtopic| finding_line(subtopic) + "\n")
        .collect();
    let recorded = std::fs::read_to_string(&findings).expect("findings file exists");
    assert_eq!(
        recorded, expected,
        "the findings file holds exactly the nine expected lines"
    );
}
