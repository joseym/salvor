//! An MCP server parking the run it was called from, end to end through the
//! real `salvor` binary.
//!
//! The mechanism is `_meta.salvor` on the tool result, the MCP extension point
//! a server uses to say something to one particular client. The client decodes
//! it into the same `ToolOutcome::Suspend` or `ToolOutcome::Sleep` a native
//! tool returns, and everything below that is the runtime's ordinary park
//! machinery. These tests exist to prove that last clause: that an MCP park
//! records the same events, in the same order, with the same claim behavior,
//! as one a native tool asked for.
//!
//! # What is being measured
//!
//! The event log and the store's call commitments, read directly, plus the
//! count file the fixture server appends to. The count file is the honest
//! witness: one line per execution that really happened, written by a process
//! outside salvor, so "the tool did not run again on the resume" is a fact
//! about the world rather than salvor's opinion of itself.
//!
//! # The clock
//!
//! `salvor run` reads the real clock. A test does not move it; it moves the
//! deadline instead, which is the same trick `wake_cli.rs` uses. An hour out
//! is not due under any clock a test machine could have, and a minute in the
//! past is due under all of them.

mod common;

use std::path::{Path, PathBuf};

use common::{COUNT_FIXTURE, GateModel, count_lines, run_salvor, text_response, tool_use_response};
use salvor_core::{Event, RunId, RunStatus, SuspensionKind, derive_state};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};
use tempfile::tempdir;

/// Writes an agent TOML wiring the counting fixture, with `extra` appended to
/// the MCP server entry (an `idempotency_keys` declaration, for instance).
fn write_agent(dir: &Path, model_uri: &str, count_file: &Path, extra: &str) -> PathBuf {
    let toml = format!(
        "model = \"test-model\"\n\
         system_prompt = \"You are a test agent.\"\n\
         \n\
         [llm]\n\
         base_url = \"{model_uri}\"\n\
         max_retries = 0\n\
         \n\
         [[mcp_servers]]\n\
         command = \"{fixture}\"\n\
         args = [\"{count}\"]\n\
         {extra}\n",
        fixture = COUNT_FIXTURE,
        count = count_file.display(),
    );
    let path = dir.join("agent.toml");
    std::fs::write(&path, toml).expect("write agent toml");
    path
}

/// The run id `salvor run` prints on its first line.
fn run_id_from(stdout: &str) -> String {
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("run "))
        .expect("run prints its id first")
        .trim()
        .to_owned()
}

/// The recorded log of `run_id`, read straight out of the store.
async fn log(store: &Path, run_id: &str) -> Vec<salvor_core::EventEnvelope> {
    let store = SqliteStore::open(store).expect("the store opens");
    let run_id = RunId::from_uuid(run_id.parse().expect("the printed id is a uuid"));
    store.read_log(run_id).await.expect("the log reads")
}

/// The event names of a log, in order. Read off the recorded wire form, which
/// is adjacently tagged, so this stays right without a match arm per variant.
fn kinds(log: &[salvor_core::EventEnvelope]) -> Vec<String> {
    log.iter()
        .map(|envelope| {
            serde_json::to_value(&envelope.event).expect("an event serializes")["kind"]
                .as_str()
                .expect("every event is tagged with its kind")
                .to_owned()
        })
        .collect()
}

/// The output recorded on the one `ToolCallCompleted` in a log.
fn completion_output(log: &[salvor_core::EventEnvelope]) -> Value {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("the tool call completed")
}

/// A tool that asks for a sleep parks the run, and the recorded order is
/// intent, completion, `SleepStarted`. That order is the whole point of
/// carrying the request in the completion: the call settles first, so the
/// store's claim on the call's identity is released before the wait begins
/// and a run asleep for a week blocks nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_sleep_request_parks_the_run_after_the_completion_lands() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    let model = GateModel::mount(vec![(
        1,
        tool_use_response(
            "tu_hold",
            "hold",
            json!({"seconds": 3600, "hold_id": "order-9"}),
            100,
            20,
        ),
    )])
    .await;
    // The hold is keyed on `hold_id`, which is what gives the call an identity
    // for the store to claim. Without a declared key there is no claim, and
    // the claim is half of what this test is about.
    let agent = write_agent(
        dir.path(),
        &model.uri(),
        &count_file,
        "effect_overrides = { hold = \"write\" }\n\
         idempotency_keys = { hold = \"hold_id\" }",
    );

    let run = run_salvor(
        &store,
        &[
            "run",
            "--agent",
            agent.to_str().unwrap(),
            "--input",
            "\"go\"",
        ],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(run.status.success(), "parking is not a failure: {run:?}");
    let run_id = run_id_from(&stdout);

    let recorded = log(&store, &run_id).await;
    assert_eq!(
        kinds(&recorded),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "SleepStarted",
        ],
        "the completion settles the call, and only then does the sleep start"
    );
    let output = completion_output(&recorded);
    assert!(
        output.get("__salvor_sleep").is_some(),
        "the request rides in the completion's own output, as a native tool's does: {output}"
    );
    assert!(
        matches!(derive_state(&recorded).status, RunStatus::Sleeping { .. }),
        "the run is asleep"
    );
    assert_eq!(count_lines(&count_file), 1, "the tool ran once");

    // THE CLAIM, while the run sleeps. A commitment carrying a completion
    // position is a settled one, and a settled claim refuses nobody.
    let opened = SqliteStore::open(&store).expect("the store opens");
    let commitment = opened
        .lookup_call("hold", "hold:order-9")
        .await
        .expect("the lookup reads")
        .expect("a keyed call took a claim");
    assert!(
        commitment.completion_seq.is_some(),
        "the claim settled at the completion the sleep follows, so a sleeping run holds nothing"
    );
}

/// A tool that asks to wait for a signal records `Suspended` with the signal
/// discriminator, and `salvor resume` continues it with a payload the server's
/// own schema accepts. The MCP server is respawned on the resume and never
/// called a second time: the completion replays out of the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_suspension_records_its_kind_and_resumes_without_a_second_call() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    let model = GateModel::mount(vec![
        (
            1,
            tool_use_response("tu_wait", "await_settlement", json!({}), 100, 20),
        ),
        (3, text_response("settlement confirmed", 150, 30)),
    ])
    .await;
    let agent = write_agent(
        dir.path(),
        &model.uri(),
        &count_file,
        "effect_overrides = { await_settlement = \"read\" }",
    );
    let agent_path = agent.to_str().unwrap();

    let run = run_salvor(&store, &["run", "--agent", agent_path, "--input", "\"go\""]).await;
    assert!(run.status.success(), "parking is not a failure: {run:?}");
    let run_id = run_id_from(&String::from_utf8_lossy(&run.stdout));

    let recorded = log(&store, &run_id).await;
    let suspended = recorded
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::Suspended {
                kind,
                reason,
                input_schema,
                ..
            } => Some((*kind, reason.clone(), input_schema.clone())),
            _ => None,
        })
        .expect("the run suspended");
    assert_eq!(
        suspended.0,
        Some(SuspensionKind::Signal),
        "a wait on a webhook is recorded as one, so no inbox shows it as an approval"
    );
    assert_eq!(suspended.1, "waiting on the settlement webhook");
    assert_eq!(
        suspended.2["properties"]["paid"]["type"],
        json!("boolean"),
        "the recorded schema is the server's own: {}",
        suspended.2
    );
    assert_eq!(
        kinds(&recorded).last().map(String::as_str),
        Some("Suspended"),
        "the completion lands before the suspension, as it does for a sleep"
    );
    assert_eq!(count_lines(&count_file), 1, "the tool ran once");

    // An input the recorded schema rejects is refused, which is the proof that
    // the schema travelled from the server into the log intact.
    let wrong = run_salvor(
        &store,
        &[
            "resume",
            &run_id,
            "--agent",
            agent_path,
            "--input",
            "{\"paid\": \"probably\"}",
        ],
    )
    .await;
    assert!(
        !wrong.status.success(),
        "a payload the server's schema rejects does not resume the run: {wrong:?}"
    );

    let resume = run_salvor(
        &store,
        &[
            "resume",
            &run_id,
            "--agent",
            agent_path,
            "--input",
            "{\"paid\": true}",
        ],
    )
    .await;
    let resumed = String::from_utf8_lossy(&resume.stdout);
    assert!(resume.status.success(), "the resume completes: {resume:?}");
    assert!(
        resumed.contains("settlement confirmed"),
        "the run finished on its answer: {resumed}"
    );
    assert_eq!(
        count_lines(&count_file),
        1,
        "the resume respawned the server and called nothing: the completion replayed"
    );
}

/// A park request spelled wrong fails the call, and the recorded failure names
/// `_meta.salvor`. The alternative, passing the result through as ordinary
/// output, is the bug this contract exists to prevent: a server author would
/// see a tool that "just returned" and have nothing to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_park_request_is_a_recorded_failure_naming_the_key() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let count_file = dir.path().join("count.txt");

    let model = GateModel::mount(vec![
        (
            1,
            tool_use_response("tu_bad", "bad_park", json!({}), 100, 20),
        ),
        (3, text_response("gave up on the park", 150, 30)),
    ])
    .await;
    let agent = write_agent(
        dir.path(),
        &model.uri(),
        &count_file,
        "effect_overrides = { bad_park = \"write\" }",
    );

    let run = run_salvor(
        &store,
        &[
            "run",
            "--agent",
            agent.to_str().unwrap(),
            "--input",
            "\"go\"",
        ],
    )
    .await;
    let run_id = run_id_from(&String::from_utf8_lossy(&run.stdout));

    let recorded = log(&store, &run_id).await;
    let output = completion_output(&recorded);
    let failure = output
        .get("__salvor_error")
        .expect("the malformed park was recorded as a tool failure, not as output");
    let message = failure["message"]
        .as_str()
        .expect("the failure has a message");
    assert!(
        message.contains("`_meta.salvor` has an unknown key `sleepUntil`"),
        "the recorded message names the key and the mistake: {message}"
    );
    assert!(
        !kinds(&recorded)
            .iter()
            .any(|kind| kind == "SleepStarted" || kind == "Suspended"),
        "nothing parked: {:?}",
        kinds(&recorded)
    );
}
