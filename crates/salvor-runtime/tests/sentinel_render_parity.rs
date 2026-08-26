//! Holds the two readers of the recorded sentinel contract equal.
//!
//! A tool call's completion output carries three reserved shapes: the
//! suspension sentinel, the sleep sentinel, and the structured tool-failure
//! object. Two places read them.
//! `salvor_runtime::wire` decodes them into this crate's `Suspension`, `Sleep`,
//! and `ToolFailure` types, and `salvor_core::event_detail` (defined in the pure
//! `salvor-replay` crate) reads the same recorded fields to render a run's
//! history and its live progress line.
//!
//! The renderer cannot call the runtime's decoders. `decode_suspension` returns
//! `salvor_tools::Suspension` and `ToolFailure::from_error` is an inherent impl
//! over `salvor_tools::ToolError`, so both are pinned to a crate that pulls
//! tokio through the MCP client. The renderer must stay free of an executor to
//! keep building for `wasm32-unknown-unknown`, so it reads the reserved keys
//! itself. That leaves one contract with two implementations, and this test is
//! what keeps them from drifting: it renders every interesting completion
//! output through `event_detail` and compares it against the string built from
//! the runtime's own decoders.
//!
//! The corpus is deliberately weighted toward the fall-through edges. Whether a
//! malformed sentinel renders as a suspension, as a failure, or as an ordinary
//! output is the part most likely to diverge when someone adds a sentinel kind
//! or tightens a field check on one side only.
//!
//! This test lives in `salvor-runtime` because it is the only crate that can
//! see both readers at once.

use salvor_core::{Event, SequenceNumber, event_detail};
use salvor_runtime::{
    ERROR_SENTINEL_KEY, SLEEP_SENTINEL_KEY, SUSPEND_SENTINEL_KEY, decode_failure, decode_sleep,
    decode_suspension,
};
use serde_json::{Value, json};

/// The cap `event_detail` truncates every payload to. Stated here rather than
/// imported so a change to the renderer's cap has to be made deliberately on
/// both sides.
const CAP: usize = 80;

/// The reference detail for a `ToolCallCompleted` output, built from the
/// runtime's decoders. Mirrors what `event_detail` must produce for the
/// completion arm: a suspension renders its reason, a failure renders its kind,
/// attempt count, and truncated message, and anything else renders as a
/// truncated output payload.
fn reference_detail(output: &Value) -> String {
    if let Some(suspension) = decode_suspension(output) {
        format!("suspends: {}", suspension.reason)
    } else if let Some(sleep) = decode_sleep(output) {
        format!("sleeps until {}", format_ts(sleep.wake_at))
    } else if let Some(failure) = decode_failure(output) {
        format!(
            "error ({}, {} attempt(s)): {}",
            failure.kind.as_str(),
            failure.attempts,
            truncate(&failure.message)
        )
    } else {
        format!("output {}", truncate(&output.to_string()))
    }
}

/// The renderer's timestamp format, stated here rather than imported for the
/// same reason [`CAP`] is: a change to how a recorded instant reads has to be
/// made deliberately on both sides.
fn format_ts(ts: time::OffsetDateTime) -> String {
    let utc = ts.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
}

/// Truncates to [`CAP`] characters with an ellipsis, counting characters rather
/// than bytes.
fn truncate(text: &str) -> String {
    if text.chars().count() > CAP {
        let head: String = text.chars().take(CAP).collect();
        format!("{head}\u{2026}")
    } else {
        text.to_owned()
    }
}

/// Wraps a sentinel body under `key`, so every case below names the reserved
/// key through the runtime's exported constant. Renaming a constant on the
/// runtime side then fails this test instead of passing silently while the
/// renderer keeps reading the old name.
fn sentinel(key: &str, body: Value) -> Value {
    json!({ key: body })
}

/// Every completion output whose classification is worth pinning: both valid
/// sentinels, each way a sentinel can be malformed enough to fall through, and
/// ordinary outputs that merely resemble one.
fn corpus() -> Vec<Value> {
    vec![
        // Valid suspension.
        sentinel(
            SUSPEND_SENTINEL_KEY,
            json!({"reason": "needs approval", "input_schema": {"type": "object"}}),
        ),
        // Suspension missing the schema its resume input must satisfy.
        sentinel(SUSPEND_SENTINEL_KEY, json!({"reason": "no schema"})),
        // Suspension missing its reason.
        sentinel(SUSPEND_SENTINEL_KEY, json!({"input_schema": {}})),
        // Suspension whose reason is not a string.
        sentinel(
            SUSPEND_SENTINEL_KEY,
            json!({"reason": 7, "input_schema": {}}),
        ),
        // Suspension whose body is not an object at all.
        sentinel(SUSPEND_SENTINEL_KEY, json!("not an object")),
        // Reserved key alongside another key, so not a one-key sentinel. The
        // bodies here are well formed on purpose: a case with an empty body
        // falls through for a second reason and would still agree if the
        // one-key rule were dropped, so it would not pin that rule.
        json!({ SUSPEND_SENTINEL_KEY: {"reason": "approve", "input_schema": {}}, "other": 1 }),
        json!({ ERROR_SENTINEL_KEY: {"kind": "handler", "message": "m", "attempts": 1}, "x": 0 }),
        json!({ SUSPEND_SENTINEL_KEY: {}, "other": 1 }),
        // Valid sleep, and every way one can fall through: a deadline that
        // will not parse, a missing one, one that is not a string, a body that
        // is not an object, and the reserved key beside another.
        sentinel(
            SLEEP_SENTINEL_KEY,
            json!({"wake_at": "2026-08-14T09:00:00Z"}),
        ),
        sentinel(SLEEP_SENTINEL_KEY, json!({"wake_at": "tomorrow"})),
        sentinel(SLEEP_SENTINEL_KEY, json!({})),
        sentinel(SLEEP_SENTINEL_KEY, json!({"wake_at": 1_755_162_000})),
        sentinel(SLEEP_SENTINEL_KEY, json!("not an object")),
        json!({ SLEEP_SENTINEL_KEY: {"wake_at": "2026-08-14T09:00:00Z"}, "other": 1 }),
        // Valid failures, one per legal kind.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"is_error": true, "kind": "handler", "message": "boom", "attempts": 2}),
        ),
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "output_serialization", "message": "m", "attempts": 0}),
        ),
        // A failure message past the truncation boundary.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "invalid_input", "message": "x".repeat(200), "attempts": 1}),
        ),
        // A failure message exactly at the boundary, which must not truncate.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "handler", "message": "y".repeat(CAP), "attempts": 1}),
        ),
        // Unrecognized failure kind.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "nope", "message": "m", "attempts": 1}),
        ),
        // Failure missing its message.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "handler", "attempts": 1}),
        ),
        // Failure missing its attempt count.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "handler", "message": "m"}),
        ),
        // Attempt count past what a u32 holds.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "handler", "message": "m", "attempts": 5_000_000_000u64}),
        ),
        // Negative attempt count.
        sentinel(
            ERROR_SENTINEL_KEY,
            json!({"kind": "handler", "message": "m", "attempts": -1}),
        ),
        // The reserved name below the top level, which is an ordinary output.
        json!({ "nested": { ERROR_SENTINEL_KEY: {} } }),
        // The reserved name as a bare string value.
        json!(ERROR_SENTINEL_KEY),
        // Ordinary outputs, including the non-object shapes and one long enough
        // to truncate.
        json!({"result": 1}),
        json!(null),
        json!([1, 2, 3]),
        json!({"a": "z".repeat(300)}),
    ]
}

/// The renderer classifies and formats every completion output exactly as the
/// runtime's decoders do. A divergence here means `salvor history` prints
/// something other than what the runtime recorded.
#[test]
fn renderer_matches_runtime_decoders() {
    for output in corpus() {
        let event = Event::ToolCallCompleted {
            seq: SequenceNumber::new(0),
            output: output.clone(),
            deduplicated_from: None,
            settled_by: None,
        };
        assert_eq!(
            event_detail(&event),
            reference_detail(&output),
            "detail diverged for completion output {output}"
        );
    }
}

/// The corpus actually exercises all four classifications, so a change that
/// broke decoding outright could not pass by rendering everything as an
/// ordinary output.
#[test]
fn corpus_covers_every_classification() {
    let corpus = corpus();
    assert!(
        corpus.iter().any(|o| decode_suspension(o).is_some()),
        "corpus must contain a decodable suspension"
    );
    assert!(
        corpus.iter().any(|o| decode_sleep(o).is_some()),
        "corpus must contain a decodable sleep"
    );
    assert!(
        corpus.iter().any(|o| decode_failure(o).is_some()),
        "corpus must contain a decodable failure"
    );
    assert!(
        corpus.iter().any(|o| decode_suspension(o).is_none()
            && decode_sleep(o).is_none()
            && decode_failure(o).is_none()),
        "corpus must contain an output that is no sentinel at all"
    );
}
