//! End to end through the real `salvor` binary for `graph run` and a graph
//! run's `resume`.
//!
//! These are hermetic: a single-`gate` graph parks with no model and no tool,
//! so the whole park -> resume -> complete loop runs offline. The headline is
//! the resume decision proven: a graph run's log records only the graph's HASH,
//! so `salvor resume` re-supplies the document through `--graph`, and its hash
//! must match the one the run recorded. The rest pins the refusals: an invalid
//! document, a hash mismatch on resume, and an unresolvable tool or agent.
//!
//! The last group pins the permanent/transient split as an operator meets it: a
//! PERMANENT engine refusal names the triage in the CLI's own voice and leaves
//! the run reading `failed` in `salvor list`; a TRANSIENT one names the same
//! triage pointed the other way, saying the run is recorded, resumable, and
//! giving the command that continues it; and a refused approval (which a
//! conforming one can still fix) records no terminal and the very same run
//! resumes and completes.

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

/// A graph run that parks on a `delay` node is not resumed by hand; it names
/// the `salvor wake` command that continues it once the deadline passes, and
/// that command carries `--store` so it is the real command to paste into a
/// crontab, not one missing the piece that names which store to open.
#[test]
fn a_timer_park_names_the_wake_command_with_its_store() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(
        dir.path(),
        "delay.json",
        r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "delay", "payload": { "id": "cooloff", "seconds": 3600 } }
  ],
  "edges": []
}"#,
    );

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "null"])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a timer park is a success, not a failure: {output:?}"
    );
    assert!(
        stdout.contains("sleeping until"),
        "the park reason is named: {stdout}"
    );

    let expected = format!(
        "salvor wake --store {} --graph {}",
        store.display(),
        graph.display()
    );
    assert!(
        stdout.contains(&expected),
        "the wake hint is the exact command, --store included ({expected}): {stdout}"
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

/// A single expression `branch` whose cases cannot both be false only if the
/// input has a `score`. Given an input with none, every case evaluates false and
/// the engine refuses with `NoBranchCaseMatched`, needing neither a model nor a
/// tool, so the whole refusal runs offline.
const UNROUTABLE_GRAPH: &str = r#"{
  "schema_version": 1,
  "nodes": [
    { "kind": "branch", "payload": {
      "id": "route",
      "on": "score",
      "cases": [
        { "name": "high", "when": { "kind": "expression", "value": "score >= 0.8" } },
        { "name": "low", "when": { "kind": "expression", "value": "score < 0.8" } }
      ]
    } },
    { "kind": "gate", "payload": { "id": "approve", "approval_schema": { "type": "object" } } },
    { "kind": "gate", "payload": { "id": "reject", "approval_schema": { "type": "object" } } }
  ],
  "edges": [
    { "from": "route", "to": "approve", "label": "high" },
    { "from": "route", "to": "reject", "label": "low" }
  ]
}"#;

/// A PERMANENT engine refusal ends the run: the CLI names the triage plainly,
/// the terminal `RunFailed` is recorded, and `salvor list` shows the run as
/// `failed` rather than leaving it reading as though it were still going.
///
/// This is the whole point of the permanent/transient split, end to end through
/// the real binary. The refusal (no branch case matched a routed value with no
/// `score` in it) is a pure function of the document and the recorded input, so
/// re-driving it forever produces the same answer; a run in that state is dead,
/// and a dead run that keeps reading `running` is a lie an operator acts on.
#[test]
fn a_permanent_graph_refusal_records_the_run_as_failed() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let graph = write(dir.path(), "unroutable.json", UNROUTABLE_GRAPH);

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", r#"{"topic":"otters"}"#])
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unroutable branch refuses: {output:?}"
    );
    assert!(
        stderr.contains("branch node `route`"),
        "the refusal names the node: {stderr}"
    );
    assert!(
        stderr.contains("recorded as failed") && stderr.contains("salvor list"),
        "the refusal names the triage plainly: {stderr}"
    );
    let run = run_id_from(&stdout);

    // The terminal is on disk, so the operator's own listing says `failed`.
    let output = salvor(&store).arg("list").output().expect("runs");
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "list runs: {output:?}");
    assert!(
        listing.contains(&run) && listing.contains("failed"),
        "salvor list shows the run as failed: {listing}"
    );
}

/// An agent file whose model endpoint is a port nothing can be listening on
/// (binding port 1 needs privileges no test process has), with retries off so
/// the transport failure is immediate. No API key and no network: the connection
/// is refused before a request is ever formed.
const DEAD_MODEL_AGENT: &str = "model = \"test-model\"\n\
                                system_prompt = \"You are a test agent.\"\n\
                                \n\
                                [llm]\n\
                                base_url = \"http://127.0.0.1:1\"\n\
                                max_retries = 0\n";

/// A single-`agent` graph, with the agent hash filled in from the file on disk.
fn dead_model_graph(agent_hash: &str) -> String {
    format!(
        r#"{{
  "schema_version": 1,
  "nodes": [ {{ "kind": "agent", "payload": {{ "id": "work", "agent_hash": "{agent_hash}" }} }} ],
  "edges": []
}}"#
    )
}

/// A TRANSIENT refusal gets the triage line too, pointed the other way. The
/// mirror of `a_permanent_graph_refusal_records_the_run_as_failed`: that one
/// says the run is dead and where to read it, this one says the run is ALIVE and
/// how to continue it.
///
/// The failure is a model transport failure, which is what an operator actually
/// meets: an endpoint that is down, a proxy that dropped the connection. Nothing
/// about the graph or the log caused it, so re-driving can succeed, and the run
/// is sitting on disk with its completed steps durable. Printing only the error
/// strands that run: its id scrolled past on the first line, and nothing on
/// screen says the work survived or what command picks it back up.
#[test]
fn a_transient_graph_failure_names_the_run_as_resumable() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let agent = write(dir.path(), "agent.toml", DEAD_MODEL_AGENT);

    // The document must name the agent by the hash of the file beside it, so the
    // CLI computes it exactly as an operator would.
    let output = salvor(&store)
        .args(["agent", "hash"])
        .arg(&agent)
        .output()
        .expect("runs");
    assert!(output.status.success(), "agent hash: {output:?}");
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let graph = write(dir.path(), "dead-model.json", &dead_model_graph(&hash));

    let output = salvor(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", r#"{"topic":"otters"}"#])
        .arg("--agent")
        .arg(&agent)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unreachable model fails the drive: {output:?}"
    );
    let run = run_id_from(&stdout);

    assert!(
        stderr.contains(&run) && stderr.contains("recorded and resumable"),
        "the failure names the run and says it survives: {stderr}"
    );
    // The literal command, so it can be pasted rather than reconstructed.
    let expected = format!(
        "salvor resume {run} --store {} --graph {} --agent {}",
        store.display(),
        graph.display(),
        agent.display()
    );
    assert!(
        stderr.contains(&expected),
        "the failure prints the resume command verbatim ({expected}): {stderr}"
    );

    // And the run really is alive: no terminal was recorded for it.
    let output = salvor(&store).arg("list").output().expect("runs");
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "list runs: {output:?}");
    assert!(
        listing.contains(&run) && !listing.contains("failed"),
        "a transient failure never kills the run: {listing}"
    );
}

/// A TRANSIENT refusal records no terminal. The counterpart of the test above,
/// and the half that matters more: `--input` that is not an object cannot even
/// be parsed into a run, but an approval the gate's schema rejects is a live
/// refusal against a run that IS on disk, and that run must stay resumable.
#[test]
fn a_refused_approval_leaves_the_run_parked_not_failed() {
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

    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&graph)
        .args(["--input", "{}"])
        .output()
        .expect("runs");
    assert!(!output.status.success(), "the approval is refused");

    let output = salvor(&store).arg("list").output().expect("runs");
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.contains("failed"),
        "a refused approval never kills the run: {listing}"
    );

    // And the proof that it is still live: a conforming approval completes it.
    let output = salvor(&store)
        .args(["resume", &run])
        .arg("--graph")
        .arg(&graph)
        .args(["--input", r#"{"approved":true}"#])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "the same run resumes and completes: {output:?}"
    );
}
