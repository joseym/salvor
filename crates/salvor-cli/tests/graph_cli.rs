//! End to end through the real `salvor` binary for the `graph` subcommand:
//! `graph validate` on a valid file, on an invalid file, on a missing file, and
//! on garbage, plus `graph schema`. These assert on substance (the summary, the
//! precise error text, exit codes), not just success.
//!
//! No store, no model, no MCP server: a graph document is validated in
//! isolation, so these tests write a JSON file to a tempdir and run the binary
//! against it. The `graph` handlers never open the store, so the default
//! `--store` path is never touched.

use std::fs;

use assert_cmd::Command;
use tempfile::tempdir;

/// A canonical valid document: research -> review -> gate -> publish.
const VALID: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "agent", "payload": { "id": "research", "agent_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111" } },
    { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } }
  ],
  "edges": [ { "from": "research", "to": "approve" } ]
}"#;

/// A fresh handle to the `salvor` binary with tracing quieted.
fn salvor() -> Command {
    let mut command = Command::cargo_bin("salvor").expect("salvor binary builds");
    command.env("RUST_LOG", "warn");
    command
}

#[test]
fn validate_accepts_a_valid_document_and_prints_a_summary() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("valid.json");
    fs::write(&path, VALID).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "valid document exits 0: {output:?}"
    );
    assert!(
        stdout.contains("graph ok: 2 node(s), 1 edge(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("entry:    research"), "{stdout}");
    assert!(stdout.contains("terminal: approve"), "{stdout}");
}

#[test]
fn validate_rejects_a_dangling_edge_with_a_precise_error() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("dangling.json");
    // The edge target `aprove` is a typo of the node `approve`.
    let doc = VALID.replace(r#""to": "approve""#, r#""to": "aprove""#);
    fs::write(&path, doc).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "invalid exits 1: {output:?}");
    assert!(
        stderr.contains("references unknown node id `aprove`"),
        "names the missing id: {stderr}"
    );
    assert!(
        stderr.contains("did you mean `approve`?"),
        "suggests the near miss: {stderr}"
    );
}

#[test]
fn validate_reports_a_cycle() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("cycle.json");
    let doc = r#"{
      "schema_version": 1,
      "nodes": [
        { "kind": "agent", "payload": { "id": "a", "agent_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111" } },
        { "kind": "agent", "payload": { "id": "b", "agent_hash": "sha256:2222222222222222222222222222222222222222222222222222222222222222" } }
      ],
      "edges": [ { "from": "a", "to": "b" }, { "from": "b", "to": "a" } ]
    }"#;
    fs::write(&path, doc).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "invalid exits 1");
    assert!(
        stderr.contains("cycle detected"),
        "reports the cycle: {stderr}"
    );
}

/// A branch case with a misspelled edge label (`lst` for the case `lost`) is
/// exactly the mistake this check exists to catch: the edge that was meant to
/// realize `lost` never does, and the run would silently skip past it.
#[test]
fn validate_rejects_a_branch_case_with_no_matching_edge() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("unrouted_case.json");
    let doc = r#"{
      "schema_version": 1,
      "nodes": [
        { "kind": "tool", "payload": { "id": "assess", "tool": "assess" } },
        { "kind": "branch", "payload": { "id": "route", "on": "assess.outcome", "cases": [
            { "name": "won", "when": { "kind": "expression", "value": "outcome == \"won\"" } },
            { "name": "lost", "when": { "kind": "expression", "value": "outcome == \"lost\"" } }
          ] } },
        { "kind": "tool", "payload": { "id": "celebrate", "tool": "notify" } }
      ],
      "edges": [
        { "from": "assess", "to": "route" },
        { "from": "route", "to": "celebrate", "label": "won" },
        { "from": "route", "to": "celebrate", "label": "lst" }
      ]
    }"#;
    fs::write(&path, doc).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "invalid exits 1: {output:?}");
    assert!(
        stderr.contains("branch node `route`") && stderr.contains("case `lost`"),
        "names the branch and the unrouted case: {stderr}"
    );
    assert!(
        stderr.contains("terminal node"),
        "says what to do about a route meant to end the run: {stderr}"
    );
}

/// An outbound edge from a branch labeled with a name no case declares is the
/// mirror mistake: a misspelled edge label (`lst`) that realizes neither of
/// the branch's declared cases (`lost`, `paid`). Both declared cases carry
/// their own correctly labeled edge, so this isolates the new check alone.
#[test]
fn validate_rejects_a_branch_edge_labeled_with_no_matching_case() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("unmatched_label.json");
    let doc = r#"{
      "schema_version": 1,
      "nodes": [
        { "kind": "tool", "payload": { "id": "assess", "tool": "assess" } },
        { "kind": "branch", "payload": { "id": "route", "on": "assess.outcome", "cases": [
            { "name": "lost", "when": { "kind": "expression", "value": "outcome == \"lost\"" } },
            { "name": "paid", "when": { "kind": "expression", "value": "outcome == \"paid\"" } }
          ] } },
        { "kind": "tool", "payload": { "id": "close", "tool": "notify" } },
        { "kind": "tool", "payload": { "id": "celebrate", "tool": "notify" } }
      ],
      "edges": [
        { "from": "assess", "to": "route" },
        { "from": "route", "to": "close", "label": "lost" },
        { "from": "route", "to": "celebrate", "label": "paid" },
        { "from": "route", "to": "close", "label": "lst" }
      ]
    }"#;
    fs::write(&path, doc).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "invalid exits 1: {output:?}");
    assert!(
        stderr.contains("branch node `route`") && stderr.contains("labeled `lst`"),
        "names the branch and the offending label: {stderr}"
    );
    assert!(
        stderr.contains("names no case"),
        "says what went wrong: {stderr}"
    );
}

#[test]
fn validate_rejects_an_unknown_field() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("stray.json");
    let doc = VALID.replace(
        r#""edges": [ { "from": "research", "to": "approve" } ]"#,
        r#""edges": [ { "from": "research", "to": "approve" } ], "surprise": true"#,
    );
    fs::write(&path, doc).expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "strict parse exits 1");
    assert!(
        stderr.contains("not a valid graph document"),
        "reports a parse failure: {stderr}"
    );
    assert!(
        stderr.contains("surprise"),
        "names the stray field: {stderr}"
    );
}

#[test]
fn validate_errors_cleanly_on_a_missing_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("does-not-exist.json");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "missing file exits 1, no panic"
    );
    assert!(
        stderr.contains("cannot read graph file"),
        "clear read error: {stderr}"
    );
    // A panic would print a backtrace note; assert it did not happen.
    assert!(!stderr.contains("panicked"), "must not panic: {stderr}");
}

#[test]
fn validate_errors_cleanly_on_garbage() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("garbage.json");
    fs::write(&path, "this is not json {").expect("write");

    let output = salvor()
        .args(["graph", "validate"])
        .arg(&path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "garbage exits 1, no panic");
    assert!(
        stderr.contains("not a valid graph document"),
        "reports the parse failure: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "must not panic: {stderr}");
}

#[test]
fn schema_prints_the_json_schema_and_exits_zero() {
    let output = salvor().args(["graph", "schema"]).output().expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "schema exits 0: {output:?}");
    // The emitted schema describes the document: its own fields and the node
    // payloads reachable through $defs.
    for expected in [
        "schema_version",
        "nodes",
        "edges",
        "AgentNode",
        "approval_schema",
    ] {
        assert!(stdout.contains(expected), "schema mentions {expected}");
    }
    // It is valid JSON.
    serde_json::from_str::<serde_json::Value>(&stdout).expect("schema is valid JSON");
}
