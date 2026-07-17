//! End to end through the real `salvor` binary for `salvor fork`.
//!
//! Hermetic: a two-`gate` graph parks with no model and no tool, so the whole
//! run -> resume -> fork loop runs offline. These prove the CLI wiring — the
//! local flavor that re-supplies the document (hash-checked), plans the fork,
//! and drives the child from the fork node — plus the typed refusals: a hash
//! mismatch, a node the origin never entered, and an unknown run. The
//! refuse-then-record behavior over a real recorded WRITE is proven
//! authoritatively in the engine and server suites (a hermetic CLI write tool
//! would need a live model or MCP server); here the dry run over a
//! write-free segment shows the preview path.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::tempdir;

/// gate1 -> gate2: parks at gate1, and after a resume parks at gate2, so both
/// node boundaries are recorded with neither a model nor a tool in play.
const TWO_GATE_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "gate", "payload": { "id": "gate1", "approval_schema": {
      "type": "object", "properties": { "approved": { "type": "boolean" } }
    } } },
    { "kind": "gate", "payload": { "id": "gate2", "approval_schema": {
      "type": "object", "properties": { "approved": { "type": "boolean" } }
    } } }
  ],
  "edges": [ { "from": "gate1", "to": "gate2" } ]
}"#;

fn salvor(store: &Path) -> Command {
    let mut command = Command::cargo_bin("salvor").expect("salvor binary builds");
    command.env("RUST_LOG", "warn");
    command.arg("--store").arg(store);
    command
}

fn write(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write");
    path
}

fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .expect("a `run <uuid>` line")
        .split_whitespace()
        .next()
        .expect("a uuid token")
        .to_owned()
}

/// Drives the two-gate graph to a park at gate2 (through run then resume), and
/// returns `(store, graph_path, origin_run_id)`.
fn origin_parked_at_gate2(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let store = dir.join("salvor.db");
    let graph = write(dir, "gates.json", TWO_GATE_GRAPH);

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "graph run parks: {output:?}");
    let run = run_id_from(&stdout);

    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&graph)
        .args(["--input", r#"{"approved":true}"#])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "resume advances to gate2: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("parked at node `gate2`"),
        "at gate2: {stdout}"
    );

    (store, graph, run)
}

#[test]
fn fork_from_a_node_boundary_creates_a_new_run() {
    let dir = tempdir().expect("tempdir");
    let (store, graph, origin) = origin_parked_at_gate2(dir.path());

    let output = salvor(&store)
        .args(["fork", &origin])
        .args(["--from-node", "gate2"])
        .arg("--graph")
        .arg(&graph)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fork proceeds (no write hazard): {output:?}"
    );
    assert!(
        stdout.contains(&format!("forked from {origin}")),
        "the child names its origin: {stdout}"
    );
    let child = run_id_from(&stdout);
    assert_ne!(child, origin, "the fork is a NEW run");
    assert!(
        stdout.contains("parked at node `gate2`"),
        "the child re-walks the fork node: {stdout}"
    );

    // The child's projection records its fork origin.
    let output = salvor(&store)
        .args(["replay", &child, "--dry-run"])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "replay reads the child: {output:?}"
    );
}

#[test]
fn a_dry_run_previews_and_creates_nothing() {
    let dir = tempdir().expect("tempdir");
    let (store, graph, origin) = origin_parked_at_gate2(dir.path());

    let before = salvor(&store).arg("list").output().expect("runs");
    let before = String::from_utf8_lossy(&before.stdout).lines().count();

    let output = salvor(&store)
        .args(["fork", &origin])
        .args(["--from-node", "gate1"])
        .arg("--graph")
        .arg(&graph)
        .arg("--dry-run")
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dry run exits 0: {output:?}");
    assert!(
        stdout.contains("dry run"),
        "labels itself a dry run: {stdout}"
    );
    assert!(
        stdout.contains("no writes in the re-walked segment"),
        "reports the write-free segment: {stdout}"
    );

    let after = salvor(&store).arg("list").output().expect("runs");
    let after = String::from_utf8_lossy(&after.stdout).lines().count();
    assert_eq!(before, after, "a dry run creates no run");
}

#[test]
fn fork_with_a_mismatched_graph_refuses() {
    let dir = tempdir().expect("tempdir");
    let (store, _graph, origin) = origin_parked_at_gate2(dir.path());
    let other = write(
        dir.path(),
        "other.json",
        &TWO_GATE_GRAPH.replace("\"boolean\"", "\"string\""),
    );

    let output = salvor(&store)
        .args(["fork", &origin])
        .args(["--from-node", "gate2"])
        .arg("--graph")
        .arg(&other)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "mismatch refuses: {output:?}");
    assert!(
        stderr.contains("SAME document"),
        "the refusal explains the hash mismatch: {stderr}"
    );
}

#[test]
fn fork_from_a_node_the_run_never_entered_refuses() {
    let dir = tempdir().expect("tempdir");
    let (store, graph, origin) = origin_parked_at_gate2(dir.path());

    let output = salvor(&store)
        .args(["fork", &origin])
        .args(["--from-node", "ghost"])
        .arg("--graph")
        .arg(&graph)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unknown node refuses: {output:?}");
    assert!(
        stderr.contains("never entered node `ghost`"),
        "names the node: {stderr}"
    );
}

#[test]
fn fork_of_an_unknown_run_refuses() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "gates.json", TWO_GATE_GRAPH);
    let missing = uuid::Uuid::new_v4();

    let output = salvor(&store)
        .args(["fork", &missing.to_string()])
        .args(["--from-node", "gate1"])
        .arg("--graph")
        .arg(&graph)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unknown run refuses: {output:?}");
    assert!(stderr.contains("no run"), "names the missing run: {stderr}");
}
