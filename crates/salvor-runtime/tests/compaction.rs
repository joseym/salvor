//! Error compaction, end to end: the model sees a truncated error (cap plus
//! elision marker), consecutive identical failures collapse into a counted
//! summary, and the event log carries the full error text regardless.

mod common;

use std::sync::Arc;

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, fixed_clock, fixed_random, fixed_run_id,
    text_response, tool_result_contents, tool_use_response,
};
use salvor_core::{Effect, Event};
use salvor_runtime::{
    COMPACT_HEAD_CHARS, COMPACT_MESSAGE_CAP, COMPACT_TAIL_CHARS, ERROR_SENTINEL_KEY, RunOutcome,
    Runtime,
};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};

#[tokio::test]
async fn long_errors_truncate_and_repeats_collapse() {
    // A failure message far over the cap, with distinctive head and tail.
    let handler_message = format!(
        "HEAD_MARKER {} TAIL_MARKER",
        "connection pool exhausted; ".repeat(60)
    );
    // The recorded message is the full dispatch error chain: the erased
    // layer's context plus the handler's own message.
    let full_message = format!("tool `flaky` failed: {handler_message}");
    assert!(full_message.chars().count() > COMPACT_MESSAGE_CAP);

    // The model calls the failing tool twice in a row, then finishes.
    let server = ScriptedModel::mount(vec![
        (1, tool_use_response("tu_1", "flaky", json!({}), 10, 1)),
        (3, tool_use_response("tu_2", "flaky", json!({}), 12, 1)),
        (5, text_response("gave up on the tool", 14, 2)),
    ])
    .await;
    let (tool, _calls) = TestTool::new(
        "flaky",
        Effect::Read,
        ToolBehavior::Fail(handler_message.clone()),
    );
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(40);
    let outcome = runtime
        .start_with_id(&agent, run_id, json!("try the flaky tool"))
        .await
        .expect("run completes");
    assert!(matches!(outcome, RunOutcome::Completed { .. }));

    // What the model actually received, read back from the recorded
    // requests: the second request carries the first failure, the third
    // carries the collapsed repeat.
    let requests = server.received_requests().await.expect("requests recorded");
    let contents_by_len: Vec<Vec<String>> = requests
        .iter()
        .map(|request| tool_result_contents(&request.body))
        .filter(|contents| !contents.is_empty())
        .collect();
    assert_eq!(contents_by_len.len(), 2, "two requests carry tool results");
    assert_eq!(
        contents_by_len[1].len(),
        2,
        "the final request's conversation carries both tool results"
    );

    // First failure: truncated to head + marker + tail, in characters.
    let first = &contents_by_len[0][0];
    let expected_head: String = full_message.chars().take(COMPACT_HEAD_CHARS).collect();
    let tail_skip = full_message.chars().count() - COMPACT_TAIL_CHARS;
    let expected_tail: String = full_message.chars().skip(tail_skip).collect();
    let expected_elided = full_message.chars().count() - COMPACT_HEAD_CHARS - COMPACT_TAIL_CHARS;
    assert!(first.starts_with(&expected_head), "head survives");
    assert!(first.ends_with(&expected_tail), "tail survives");
    assert!(
        first.contains(&format!("[... {expected_elided} chars elided ...]")),
        "the marker names the elided count: {first}"
    );
    assert!(
        first.chars().count() < full_message.chars().count(),
        "the compacted form is shorter than the original"
    );

    // Second identical failure (the LAST tool result in the final
    // request's conversation): a counted summary, not the text again.
    let second = contents_by_len[1].last().expect("a second tool result");
    assert!(
        second.contains("`flaky`") && second.contains("2 consecutive times"),
        "the repeat collapses into a counted summary: {second}"
    );
    assert!(
        !second.contains("HEAD_MARKER"),
        "the collapsed form does not repeat the error text"
    );

    // The event log carries the FULL error in both completions.
    let log = store.read_log(run_id).await.expect("log reads");
    let recorded_messages: Vec<&str> = log
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::ToolCallCompleted { output, .. } => output
                .get(ERROR_SENTINEL_KEY)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str),
            _ => None,
        })
        .collect();
    assert_eq!(recorded_messages.len(), 2, "both failures were recorded");
    for recorded in recorded_messages {
        assert_eq!(
            recorded, full_message,
            "the log records the full, uncompacted error"
        );
    }
}
