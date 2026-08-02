//! Cross-run deduplication reached from the agent file, end to end through the
//! real `salvor` binary.
//!
//! The scenario is a field tester's: a payout desk with one `pay_claim` MCP
//! tool that has no deduplication of its own, run twice as two separate
//! `salvor run` invocations over one store. Before an idempotency key could be
//! declared in configuration, that paid the claim twice; the machinery to stop
//! it existed in the runtime and the store, and nothing a CLI user could write
//! reached it. `idempotency_keys = { pay_claim = "claim_id" }` is what reaches
//! it, and these tests are the proof that it does.
//!
//! # What is being measured
//!
//! The ledger file, always. It is the payment processor's own record, written
//! by a process outside salvor, one line per payout that really left the
//! building. Counting lines is the only assertion that cannot be satisfied by
//! salvor being confident about itself.
//!
//! # The fixture
//!
//! Each test writes a complete `--fixture` directory (`agent.toml`,
//! `input.json`, `model.json`) into a temp dir and runs it. Everything it names
//! is in this repository: the MCP server is
//! `salvor-mcp-payout-fixture` (located by Cargo), and the model is the one
//! `salvor run --fixture` stands up from `model.json`. No network, no key, and
//! nothing outside the workspace.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::run_salvor;
use serde_json::{Value, json};
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");
/// The payout MCP fixture server, located by Cargo.
const PAYOUT_FIXTURE: &str = env!("CARGO_BIN_EXE_salvor-mcp-payout-fixture");
/// The claim every test here pays, taken from the field tester's own input.
const CLAIM: &str = "wreck-9931";

/// Writes a complete fixture directory and returns its path.
///
/// `keys` is the `idempotency_keys` line as it appears in the file, so a test
/// can write a correct declaration, a misspelled one, or none at all.
/// `child_env` is passed to the MCP server through the agent file's own `env`
/// table, which is how a test opens the crash window. `tool_input` is what the
/// scripted model calls `pay_claim` with.
fn write_fixture(dir: &Path, ledger: &Path, keys: &str, child_env: &str, tool_input: Value) {
    let agent = format!(
        "model = \"test-model\"\n\
         name = \"payout-desk\"\n\
         system_prompt = \"You process salvage claim payouts.\"\n\
         \n\
         [llm]\n\
         base_url_env = \"SALVOR_PAYOUT_FIXTURE_URL\"\n\
         max_retries = 0\n\
         \n\
         [[mcp_servers]]\n\
         command = \"{PAYOUT_FIXTURE}\"\n\
         args = [\"{ledger}\"]\n\
         env = {{ {child_env} }}\n\
         effect_overrides = {{ pay_claim = \"write\" }}\n\
         {keys}\n",
        ledger = ledger.display(),
    );
    std::fs::write(dir.join("agent.toml"), agent).expect("write agent.toml");
    std::fs::write(dir.join("input.json"), format!("\"pay claim {CLAIM}\""))
        .expect("write input.json");

    // Two turns, keyed by message count exactly as every other fixture is: the
    // first asks for the payout, the second answers. A second run of the same
    // fixture starts from one message again, so one script serves both runs.
    let model = json!({
        "turns": [
            {
                "messages": 1,
                "response": {
                    "id": "msg_tool_tu_pay_claim",
                    "model": "test-model",
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "tu_pay_claim",
                        "name": "pay_claim",
                        "input": tool_input,
                    }],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 30, "output_tokens": 44}
                }
            },
            {
                "messages": 3,
                "response": {
                    "id": "msg_text",
                    "model": "test-model",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "payout handled"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 100, "output_tokens": 40}
                }
            }
        ]
    });
    std::fs::write(
        dir.join("model.json"),
        serde_json::to_string_pretty(&model).expect("model script serializes"),
    )
    .expect("write model.json");
}

/// The payout the scripted model asks for.
fn payout_input() -> Value {
    json!({"claim_id": CLAIM, "amount_cents": 483_200, "currency": "USD"})
}

/// The number of lines in the ledger: one per payout that really happened.
fn ledger_lines(ledger: &Path) -> usize {
    std::fs::read_to_string(ledger)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// The run id `salvor run` prints on its first line.
fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .expect("run prints its id first")
        .trim()
        .to_owned()
}

/// A run's log, read back through the CLI itself.
async fn history(store: &Path, run_id: &str) -> Vec<Value> {
    let output = run_salvor(store, &["history", run_id, "--json"]).await;
    assert!(output.status.success(), "history reads back: {output:?}");
    serde_json::from_slice(&output.stdout).expect("history --json is JSON")
}

/// The `deduplicated_from` origin on a run's tool completion, if it has one.
fn dedup_origin(log: &[Value]) -> Option<&Value> {
    log.iter()
        .find(|envelope| envelope["event"]["kind"] == "ToolCallCompleted")
        .and_then(|envelope| envelope["event"]["payload"].get("deduplicated_from"))
}

/// The idempotency key recorded on a run's tool intent, which is the derived
/// key as the log kept it.
fn recorded_key(log: &[Value]) -> Option<&str> {
    log.iter()
        .find(|envelope| envelope["event"]["kind"] == "ToolCallRequested")
        .and_then(|envelope| envelope["event"]["payload"]["idempotency_key"].as_str())
}

/// Criterion one, the whole feature in one test: two independent `salvor run`
/// invocations over one store, an agent file declaring `pay_claim`'s key, and
/// exactly one payout. The second run completes (it is not an error to ask for
/// a payout that has already been made) and its log says whose payment it is
/// reporting.
#[tokio::test]
async fn two_runs_over_one_store_pay_the_claim_once() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("provider-ledger.jsonl");
    let fixture = dir.path().join("fixture");
    std::fs::create_dir(&fixture).expect("fixture dir");
    write_fixture(
        &fixture,
        &ledger,
        "idempotency_keys = { pay_claim = \"claim_id\" }",
        "",
        payout_input(),
    );
    let fixture_arg = fixture.to_str().expect("utf-8 path");

    let first = run_salvor(&store, &["run", "--fixture", fixture_arg]).await;
    assert!(first.status.success(), "the first run completes: {first:?}");
    let first_id = run_id_from(&String::from_utf8_lossy(&first.stdout));

    let second = run_salvor(&store, &["run", "--fixture", fixture_arg]).await;
    assert!(
        second.status.success(),
        "the second run completes: {second:?}"
    );
    let second_id = run_id_from(&String::from_utf8_lossy(&second.stdout));
    assert_ne!(first_id, second_id, "two independent runs");

    // The measurement that matters.
    assert_eq!(
        ledger_lines(&ledger),
        1,
        "the claim was paid exactly once across two runs: {:?}",
        std::fs::read_to_string(&ledger).unwrap_or_default()
    );

    // The first run executed the payout, so its completion names no origin.
    // Its intent carries the derived key in the documented format: the tool's
    // name, a colon, and the value of the declared field.
    let first_log = history(&store, &first_id).await;
    assert!(
        dedup_origin(&first_log).is_none(),
        "the run that executed the payout copied nothing: {first_log:?}"
    );
    assert_eq!(
        recorded_key(&first_log),
        Some(format!("pay_claim:{CLAIM}").as_str()),
        "the recorded key is <tool>:<field value>"
    );

    // The second run's completion names the run it copied from, which is how a
    // reader of the log tells a copy from a second execution.
    let second_log = history(&store, &second_id).await;
    let origin = dedup_origin(&second_log).expect("the second run's completion is deduplicated");
    assert_eq!(
        origin["run_id"].as_str(),
        Some(first_id.as_str()),
        "the origin is the run that paid: {origin}"
    );
}

/// Criterion two: a run killed with the payout committed and its outcome
/// unrecorded holds the identity, the next run refuses rather than paying
/// again, and `salvor resolve` is what releases it.
///
/// The kill is made deterministic by the fixture's `SLOW_AFTER_MS` window: the
/// ledger line lands, then the server sits for thirty seconds before answering,
/// so a kill after the line appears is guaranteed to land between the recorded
/// intent and the completion that never came.
#[tokio::test]
async fn a_killed_payout_holds_the_key_until_a_human_resolves_it() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("provider-ledger.jsonl");
    let fixture = dir.path().join("fixture");
    std::fs::create_dir(&fixture).expect("fixture dir");
    write_fixture(
        &fixture,
        &ledger,
        "idempotency_keys = { pay_claim = \"claim_id\" }",
        "SALVOR_PAYOUT_SLOW_AFTER_MS = \"30000\"",
        payout_input(),
    );
    let fixture_arg = fixture.to_str().expect("utf-8 path").to_owned();

    // Run it as a child so it can be killed mid-payout.
    let mut child = Command::new(SALVOR_BIN)
        .args(["--store", store.to_str().expect("utf-8 path")])
        .args(["run", "--fixture", &fixture_arg])
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor run");

    // Wait for the money to leave, which is the ledger line appearing.
    let deadline = Instant::now() + Duration::from_secs(30);
    while ledger_lines(&ledger) == 0 {
        assert!(Instant::now() < deadline, "the payout never executed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child.kill().expect("kill the run process");
    let killed = child.wait_with_output().expect("reap the killed process");
    let killed_id = run_id_from(&String::from_utf8_lossy(&killed.stdout));

    // The next run finds the identity held by a run that recorded no
    // completion, and refuses. Nothing is paid, and the refusal names the
    // holder so an operator knows where to look.
    let blocked = run_salvor(&store, &["run", "--fixture", &fixture_arg]).await;
    assert!(
        !blocked.status.success(),
        "a held identity refuses the next run: {blocked:?}"
    );
    let refusal = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        refusal.contains(&killed_id),
        "the refusal names the holding run: {refusal}"
    );
    assert!(
        refusal.contains(&format!("pay_claim:{CLAIM}")),
        "the refusal names the identity: {refusal}"
    );
    assert_eq!(ledger_lines(&ledger), 1, "the refused run paid nothing");

    // The human checks with the processor and records what happened. That is
    // the moment the payment stops being in flight, so it is the moment the
    // key is released.
    let resolve = run_salvor(
        &store,
        &[
            "resolve",
            &killed_id,
            "--output",
            r#"{"content": [{"type": "text", "text": "paid"}]}"#,
        ],
    )
    .await;
    assert!(resolve.status.success(), "resolve records it: {resolve:?}");

    // A run under the released key now deduplicates against the resolved
    // completion instead of refusing, and still pays nothing.
    let after = run_salvor(&store, &["run", "--fixture", &fixture_arg]).await;
    assert!(
        after.status.success(),
        "a resolved key lets the next run through: {after:?}"
    );
    let after_id = run_id_from(&String::from_utf8_lossy(&after.stdout));
    let after_log = history(&store, &after_id).await;
    let origin = dedup_origin(&after_log).expect("the later run's completion is deduplicated");
    assert_eq!(
        origin["run_id"].as_str(),
        Some(killed_id.as_str()),
        "it copied the resolved run's completion: {origin}"
    );
    assert_eq!(
        ledger_lines(&ledger),
        1,
        "the claim was paid once, across a kill, a refusal, and a resolve"
    );
}

/// Criterion three: a declared tool called without the field it is keyed on is
/// refused, and the refusal names the tool, the path, and the keys the input
/// did carry. Nothing is paid.
#[tokio::test]
async fn a_call_missing_the_declared_field_is_refused_and_pays_nothing() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("provider-ledger.jsonl");
    let fixture = dir.path().join("fixture");
    std::fs::create_dir(&fixture).expect("fixture dir");
    write_fixture(
        &fixture,
        &ledger,
        "idempotency_keys = { pay_claim = \"claim_id\" }",
        "",
        json!({"amount_cents": 483_200, "currency": "USD"}),
    );
    let fixture_arg = fixture.to_str().expect("utf-8 path");

    let run = run_salvor(&store, &["run", "--fixture", fixture_arg]).await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let run_id = run_id_from(&stdout);
    assert_eq!(
        ledger_lines(&ledger),
        0,
        "a call with no identity pays nothing"
    );

    // The refusal is a recorded tool failure: the model is told what it left
    // out, exactly as it is for any other bad argument, and the log keeps the
    // whole message.
    let log = history(&store, &run_id).await;
    let recorded = serde_json::to_string(&log).expect("log serializes");
    assert!(
        recorded.contains("pay_claim"),
        "the failure names the tool: {recorded}"
    );
    assert!(
        recorded.contains("claim_id"),
        "the failure names the declared path: {recorded}"
    );
    assert!(
        recorded.contains("amount_cents, currency"),
        "the failure names the keys the input carried: {recorded}"
    );
}

/// Criterion four, the reachable half: a key declared for a tool the server
/// does not advertise fails the build, before a run starts, listing what the
/// server does advertise.
#[tokio::test]
async fn a_key_for_an_unadvertised_tool_fails_the_build() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("provider-ledger.jsonl");
    let fixture = dir.path().join("fixture");
    std::fs::create_dir(&fixture).expect("fixture dir");
    write_fixture(
        &fixture,
        &ledger,
        "idempotency_keys = { pay_claimm = \"claim_id\" }",
        "",
        payout_input(),
    );
    let fixture_arg = fixture.to_str().expect("utf-8 path");

    let run = run_salvor(&store, &["run", "--fixture", fixture_arg]).await;
    assert!(
        !run.status.success(),
        "a key that binds to no tool fails the build: {run:?}"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("pay_claimm"),
        "the failure names the declared tool: {stderr}"
    );
    assert!(
        stderr.contains("advertises: pay_claim"),
        "the failure lists what the server does advertise: {stderr}"
    );
    assert_eq!(ledger_lines(&ledger), 0, "nothing ran");
}

/// Criterion five, from the outside: with no declaration in the file, two runs
/// over one store behave exactly as they always have. The tool is keyless, the
/// store arbitrates nothing, and the claim is paid twice. This is the
/// unchanged-behavior control, and it is deliberately the uncomfortable
/// assertion: the promise is what the operator declares, not what salvor
/// guesses.
#[tokio::test]
async fn without_a_declaration_nothing_changes() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let ledger = dir.path().join("provider-ledger.jsonl");
    let fixture = dir.path().join("fixture");
    std::fs::create_dir(&fixture).expect("fixture dir");
    write_fixture(&fixture, &ledger, "", "", payout_input());
    let fixture_arg = fixture.to_str().expect("utf-8 path");

    for _ in 0..2 {
        let run = run_salvor(&store, &["run", "--fixture", fixture_arg]).await;
        assert!(run.status.success(), "an unkeyed run completes: {run:?}");
    }
    assert_eq!(
        ledger_lines(&ledger),
        2,
        "an undeclared tool is untouched by any of this"
    );
}
