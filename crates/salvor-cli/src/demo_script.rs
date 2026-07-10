//! The scripted twenty-turn model conversation behind the `demo/` agent.
//!
//! This is demo- and test-support code, compiled only under the crate's
//! default-on `fixture` feature (the same gate the in-repo MCP servers use),
//! never part of a `--no-default-features` product build. It holds one thing:
//! the fixed sequence of model responses that walks the `demo/` research
//! agent through its run, expressed as the same `(message_count, response)`
//! pairs the CLI integration tests already key on.
//!
//! Two consumers share it, which is the reason it lives here instead of
//! inside either one:
//!
//! - [`crate::bin`-style] the `salvor-demo-model` binary serves this script
//!   over HTTP so the demo GIF records against a hermetic local model.
//! - `tests/demo_run.rs` mounts the same script on a wiremock server to prove
//!   the checked-in `demo/` assets drive a real `salvor` run to completion.
//!
//! Keeping the script in one place means the recorded demo and the test that
//! guards it can never disagree about what the model says.
//!
//! # Why message count keys the script
//!
//! Every turn appends to the conversation, so turn `k` reaches the model
//! carrying `2k - 1` messages (a request/response pair per prior turn, plus
//! the current request). Selecting a response by that count is stateless and
//! replay-safe: after a `kill -9` and `resume`, the earlier turns are read
//! from the durable log and never reach the model, and the first live turn
//! arrives with exactly the message count it would have had uninterrupted, so
//! a stateless server serves the pre-kill run and the resume identically.

use serde_json::{Value, json};

/// The nine research subtopics, in prompt order. Must match the system
/// prompt in `demo/agent.toml` and the canned library in `demo_research.rs`;
/// that cross-file agreement is the contract the demo run exercises.
pub const SUBTOPICS: [&str; 9] = [
    "event sourcing",
    "write-ahead logging",
    "idempotency keys",
    "crash recovery",
    "replay determinism",
    "suspension and approval",
    "budget enforcement",
    "side-effect classification",
    "process supervision",
];

/// The one finding line the scripted model saves for a subtopic. Both the
/// scripted `save_finding` input and any expected-file assertion are built
/// from this function, so a test's expectation cannot drift from the script.
#[must_use]
pub fn finding_line(subtopic: &str) -> String {
    format!("{subtopic}: noted for the report")
}

/// A canned assistant response that asks to call one tool.
fn tool_use_response(
    tool_use_id: &str,
    tool: &str,
    input: Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id": format!("msg_tool_{tool_use_id}"),
        "model": "salvor-demo-model",
        "role": "assistant",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": tool, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A canned assistant text (end-of-turn) response.
fn text_response(text: &str, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "id": "msg_text",
        "model": "salvor-demo-model",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// The full script as `(message_count, response)` pairs.
///
/// The shape mirrors the system prompt: for each of the nine subtopics a
/// `search_notes` call then a `save_finding` call (turns 1..=18), one
/// `get_finding_count` (turn 19), and a final text summary (turn 20). Turn
/// `k` is keyed by `2k - 1`, so the keys are the odd numbers 1, 3, ..., 39.
#[must_use]
pub fn script() -> Vec<(usize, Value)> {
    let mut script = Vec::with_capacity(20);
    for (index, subtopic) in SUBTOPICS.iter().enumerate() {
        let search_turn = 2 * index + 1;
        let save_turn = search_turn + 1;
        script.push((
            2 * search_turn - 1,
            tool_use_response(
                &format!("tu_search_{}", index + 1),
                "search_notes",
                json!({ "query": subtopic }),
                200,
                20,
            ),
        ));
        script.push((
            2 * save_turn - 1,
            tool_use_response(
                &format!("tu_save_{}", index + 1),
                "save_finding",
                json!({ "finding": finding_line(subtopic) }),
                210,
                21,
            ),
        ));
    }
    script.push((
        2 * 19 - 1,
        tool_use_response("tu_count", "get_finding_count", json!({}), 220, 22),
    ));
    script.push((
        2 * 20 - 1,
        text_response("Research complete: 9 findings saved.", 230, 30),
    ));
    script
}
