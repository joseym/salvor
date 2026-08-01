//! End to end through the real `salvor` binary for `graph run` and a graph
//! run's `resume`.
//!
//! These are hermetic: a single-`gate` graph parks with no model and no tool,
//! so the whole park -> resume -> complete loop runs offline. The headline is
//! the resume decision proven: a graph run's log records only the graph's HASH,
//! so `salvor resume` re-supplies the document through `--graph`, and its hash
//! must match the one the run recorded. The rest pins the refusals: an invalid
//! document, a hash mismatch on resume, and an unresolvable tool or agent.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::tempdir;

/// A single-gate graph: it parks immediately, needing neither a model nor a
/// tool, so the whole test runs offline.
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

/// A single-tool graph whose tool no provided agent carries.
const TOOL_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [ { "kind": "tool", "payload": { "id": "step", "tool": "missing" } } ],
  "edges": []
}"#;

/// A single-agent graph referencing a hash nothing supplies.
const AGENT_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [ { "kind": "agent", "payload": {
    "id": "work",
    "agent_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
  } } ],
  "edges": []
}"#;

/// A fresh handle to the `salvor` binary with tracing quieted and the store
/// pointed at a tempdir file.
fn salvor(store: &Path) -> Command {
    let mut command = Command::cargo_bin("salvor").expect("salvor binary builds");
    command.env("RUST_LOG", "warn");
    command.arg("--store").arg(store);
    command
}

/// Writes `content` to `dir/name` and returns the path.
fn write(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write");
    path
}

/// The run id `graph run` prints first: the `run <uuid>` line.
fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .expect("a `run <uuid>` line")
        .trim()
        .to_owned()
}

#[test]
fn graph_run_parks_at_a_gate_then_resume_with_the_document_completes() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "gate.json", GATE_GRAPH);

    // Drive it: it parks at the gate.
    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", r#"{"topic":"otters"}"#])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "graph run parks (exit 0): {output:?}"
    );
    assert!(
        stdout.contains("parked at node `approve`"),
        "parked report: {stdout}"
    );
    let run = run_id_from(&stdout);

    // Resume through `salvor resume`, re-supplying the SAME document via --graph.
    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&graph)
        .args(["--input", r#"{"approved":true}"#])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "resume completes: {output:?}");
    assert!(
        stdout.contains("\"approved\": true"),
        "the gate's output is the approval: {stdout}"
    );
}

#[test]
fn resume_with_a_mismatched_graph_document_refuses() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "gate.json", GATE_GRAPH);

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    let run = run_id_from(&String::from_utf8_lossy(&output.stdout));

    // A different document (an extra property in the schema) hashes differently.
    let other = write(
        dir.path(),
        "other.json",
        &GATE_GRAPH.replace("\"boolean\"", "\"string\""),
    );
    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&other)
        .args(["--input", r#"{"approved":true}"#])
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "mismatch refuses: {output:?}");
    assert!(
        stderr.contains("recorded") && stderr.contains("SAME document"),
        "the refusal explains the hash mismatch: {stderr}"
    );
}

#[test]
fn graph_run_of_an_invalid_document_refuses_with_precise_errors() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let bad = write(
        dir.path(),
        "bad.json",
        r#"{ "schema_version": 1,
             "nodes": [ { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } } ],
             "edges": [ { "from": "approve", "to": "ghost" } ] }"#,
    );
    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&bad)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "invalid refuses: {output:?}");
    assert!(
        stderr.contains("ghost"),
        "the dangling edge is named: {stderr}"
    );
}

#[test]
fn graph_run_with_an_unresolvable_tool_refuses_precisely() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "tool.json", TOOL_GRAPH);
    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unresolvable tool refuses");
    assert!(
        stderr.contains("step") && stderr.contains("missing"),
        "names the node and tool: {stderr}"
    );
}

#[test]
fn graph_run_with_an_unprovided_agent_lists_what_was_provided() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "agent.json", AGENT_GRAPH);
    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unprovided agent refuses");
    assert!(
        stderr.contains("provided: none"),
        "lists what was provided: {stderr}"
    );
}

/// A loose gate schema: `required` and `properties` with no `type`, the shape
/// that plain JSON Schema semantics leave satisfied by any non-object.
const LOOSE_GATE_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "gate", "payload": { "id": "approve", "approval_schema": {
      "required": ["approved"],
      "properties": { "approved": { "type": "boolean" } }
    } } }
  ],
  "edges": []
}"#;

/// `salvor resume` refuses a `--input` that the gate's `approval_schema` does
/// not describe: it names the gate, lists every violation, shows what a
/// conforming approval satisfies, and appends nothing, so the corrected input
/// resumes the very same parked run.
#[test]
fn resume_refuses_an_approval_the_gate_schema_does_not_describe() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "gate.json", LOOSE_GATE_GRAPH);

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", r#"{"topic":"otters"}"#])
        .output()
        .expect("runs");
    assert!(output.status.success(), "graph run parks: {output:?}");
    let run = run_id_from(&String::from_utf8_lossy(&output.stdout));

    for bad in ["null", "42", r#""nope""#, "{}"] {
        let output = salvor(&store)
            .args(["resume", &run])
            .arg("--graph")
            .arg(&graph)
            .args(["--input", bad])
            .output()
            .expect("runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "resuming with {bad} must be refused: {output:?}"
        );
        assert!(
            stderr.contains("gate `approve`'s approval_schema"),
            "the refusal names the gate for {bad}: {stderr}"
        );
        assert!(
            stderr.contains("$"),
            "the refusal lists a violation path for {bad}: {stderr}"
        );
        assert!(
            stderr.contains("still parked at that gate"),
            "the refusal says the run survives, for {bad}: {stderr}"
        );
        assert!(
            stderr.contains("A conforming approval satisfies:"),
            "the refusal shows the shape wanted, for {bad}: {stderr}"
        );
    }

    // The run never moved, so the corrected approval still resumes it.
    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&graph)
        .args(["--input", r#"{"approved":true}"#])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the corrected approval resumes: {output:?}"
    );
    assert!(
        stdout.contains("\"approved\": true"),
        "the gate's output is the approval: {stdout}"
    );
}
