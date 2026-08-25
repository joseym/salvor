//! Decoding a park request an MCP server puts in `_meta` on its tool result.
//!
//! MCP has no field for "park this run." It has `_meta`, the extension point
//! the specification reserves on every result for exactly this: metadata a
//! particular client understands and every other client ignores. So a server
//! that wants to park the run calling it puts the request under
//! `_meta.salvor`, and a host that is not salvor reads a perfectly ordinary
//! tool result with an unfamiliar metadata key.
//!
//! # The two shapes
//!
//! A gate or a signal wait, which becomes [`Suspension`]:
//!
//! ```json
//! {"_meta": {"salvor": {"suspend": {
//!    "reason": "a person must confirm the payout",
//!    "input_schema": {"type": "object", "properties": {"approved": {"type": "boolean"}}},
//!    "kind": "signal"
//! }}}}
//! ```
//!
//! `kind` is optional and its only value is `"signal"`, an external system
//! owing the run a payload. Absent, the run waits on a person, which is what
//! every suspension recorded before signals existed meant.
//!
//! A durable timer, which becomes [`Sleep`]:
//!
//! ```json
//! {"_meta": {"salvor": {"sleep_until": "2026-08-14T09:00:00Z"}}}
//! ```
//!
//! The instant is RFC 3339. A duration is deliberately not accepted: the
//! runtime records an instant and replay matches it exactly, and resolving a
//! duration needs a clock read that would give a later answer on the second
//! drive. The server has a clock; it does the arithmetic and states the
//! deadline.
//!
//! # Everything questionable is a failure, never output
//!
//! A malformed request is
//! [`ToolError::MalformedResult`](crate::ToolError::MalformedResult) naming
//! `_meta.salvor` and the problem, and it fails the call once whatever the
//! tool's effect class: the bytes are already on the wire, so a retry decodes
//! them to the same refusal. It is never quietly treated as an ordinary
//! result. That rule is the whole point of this module: a server author who
//! writes `sleepUntil`, or an ISO week date, or both park keys at once, has
//! asked for something specific and has not got it, and the failure that says
//! so costs one run. A silent fall-through costs a person the
//! afternoon it takes to work out why the tool "just returned."
//!
//! For the same reason, a strictness that would look excessive elsewhere is
//! right here: an unknown key under `_meta.salvor`, or under `suspend`, is
//! refused rather than ignored, because in this position an unknown key is
//! almost always a misspelled known one.

use rmcp::model::CallToolResult;
use salvor_core::SuspensionKind;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::outcome::{Sleep, Suspension};

/// The `_meta` key everything here lives under. One namespace, so a server can
/// carry salvor's request and another host's metadata side by side without
/// either reading the other's.
const NAMESPACE: &str = "salvor";

/// The key naming a suspension request.
const SUSPEND: &str = "suspend";

/// The key naming a sleep request.
const SLEEP_UNTIL: &str = "sleep_until";

/// A park a server asked for, decoded into the same values a native tool
/// returns.
#[derive(Debug)]
pub(super) enum ParkRequest {
    /// Park awaiting a schema-validated input.
    Suspend(Suspension),
    /// Park until an instant.
    Sleep(Sleep),
}

/// The park request on `result`, if it carries one.
///
/// `Ok(None)` is the ordinary case of a tool result with no `_meta.salvor`,
/// and it is the case that must stay free: a result without the namespace is
/// not inspected further and reaches the log exactly as it always has.
pub(super) fn park_request(result: &CallToolResult) -> Result<Option<ParkRequest>, String> {
    let Some(namespace) = result
        .meta
        .as_ref()
        .and_then(|meta| meta.0.get(NAMESPACE))
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };

    // Refused before the shape is even read, because the contradiction is in
    // the pairing rather than in either half. `isError` says the tool ran and
    // failed; a park says the tool is fine and the run should wait. A client
    // that guessed which one the server meant would be guessing about money
    // half the time.
    if result.is_error == Some(true) {
        return Err("`_meta.salvor` asks to park the run on a result flagged `isError`. A call that failed parks nothing: return the park on a successful result, or drop the `_meta.salvor` key and let the failure stand".to_owned());
    }

    decode(namespace).map(Some)
}

/// Decodes the value under `_meta.salvor` into the park it names.
fn decode(namespace: &Value) -> Result<ParkRequest, String> {
    let Some(map) = namespace.as_object() else {
        return Err(format!(
            "`_meta.salvor` must be a JSON object naming one park request, got {}",
            describe(namespace)
        ));
    };

    let suspend = map.get(SUSPEND).filter(|value| !value.is_null());
    let sleep = map.get(SLEEP_UNTIL).filter(|value| !value.is_null());

    for key in map.keys() {
        if key != SUSPEND && key != SLEEP_UNTIL {
            return Err(format!(
                "`_meta.salvor` has an unknown key `{key}`. The only keys are `{SUSPEND}` and `{SLEEP_UNTIL}`"
            ));
        }
    }

    match (suspend, sleep) {
        (Some(_), Some(_)) => Err(format!(
            "`_meta.salvor` names both `{SUSPEND}` and `{SLEEP_UNTIL}`. A run parks one way at a time: on an input it waits for, or on an instant it waits until"
        )),
        (Some(suspend), None) => decode_suspend(suspend).map(ParkRequest::Suspend),
        (None, Some(sleep)) => decode_sleep(sleep).map(ParkRequest::Sleep),
        (None, None) => Err(format!(
            "`_meta.salvor` names no park request. It must carry exactly one of `{SUSPEND}` or `{SLEEP_UNTIL}`"
        )),
    }
}

/// Decodes `_meta.salvor.suspend`.
fn decode_suspend(value: &Value) -> Result<Suspension, String> {
    let Some(map) = value.as_object() else {
        return Err(format!(
            "`_meta.salvor.{SUSPEND}` must be a JSON object, got {}",
            describe(value)
        ));
    };

    for key in map.keys() {
        if !matches!(key.as_str(), "reason" | "input_schema" | "kind") {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}` has an unknown key `{key}`. The keys are `reason`, `input_schema`, and an optional `kind`"
            ));
        }
    }

    let reason = match map.get("reason") {
        None | Some(Value::Null) => {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}` is missing `reason`, the line the person or the operator reads to know what the run is waiting for"
            ));
        }
        Some(Value::String(reason)) if reason.trim().is_empty() => {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}.reason` is empty. A park nobody can explain is a park nobody can answer"
            ));
        }
        Some(Value::String(reason)) => reason.clone(),
        Some(other) => {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}.reason` must be a string, got {}",
                describe(other)
            ));
        }
    };

    // Required, not defaulted to an empty schema. `salvor resume` validates the
    // supplied input against this before recording it, so a suspension with no
    // schema is one that accepts anything, and a server author who forgot the
    // key did not mean to say that.
    let input_schema = match map.get("input_schema") {
        None | Some(Value::Null) => {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}` is missing `input_schema`, the JSON Schema the resume input is validated against"
            ));
        }
        Some(schema) if schema.is_object() || schema.is_boolean() => schema.clone(),
        Some(other) => {
            return Err(format!(
                "`_meta.salvor.{SUSPEND}.input_schema` must be a JSON Schema object, got {}",
                describe(other)
            ));
        }
    };

    let kind = match map.get("kind") {
        None | Some(Value::Null) => None,
        Some(kind) => Some(
            serde_json::from_value::<SuspensionKind>(kind.clone()).map_err(|_| {
                format!(
                    "`_meta.salvor.{SUSPEND}.kind` must be `\"signal\"` when present, got {}. Omit it for the ordinary case, a person deciding",
                    describe(kind)
                )
            })?,
        ),
    };

    Ok(Suspension {
        reason,
        input_schema,
        kind,
    })
}

/// Decodes `_meta.salvor.sleep_until`.
fn decode_sleep(value: &Value) -> Result<Sleep, String> {
    let Some(text) = value.as_str() else {
        return Err(format!(
            "`_meta.salvor.{SLEEP_UNTIL}` must be an RFC 3339 timestamp string, got {}",
            describe(value)
        ));
    };
    let wake_at = OffsetDateTime::parse(text, &Rfc3339).map_err(|error| {
        format!(
            "`_meta.salvor.{SLEEP_UNTIL}` is not an RFC 3339 timestamp: `{text}` ({error}). A duration is not accepted here: the server holds the clock and states the instant"
        )
    })?;
    Ok(Sleep::until(wake_at))
}

/// A short account of what a value is, for a message that has to say what
/// arrived instead of what was wanted. Scalars quote themselves because seeing
/// the value is usually what identifies the mistake.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => format!("the boolean `{flag}`"),
        Value::Number(number) => format!("the number `{number}`"),
        Value::String(text) => format!("the string `{text}`"),
        Value::Array(_) => "an array".to_owned(),
        Value::Object(_) => "an object".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ContentBlock, Meta};
    use serde_json::json;

    /// A result carrying `value` under `_meta.salvor`, with ordinary content
    /// beside it as a real server's would have.
    fn with_namespace(value: Value) -> CallToolResult {
        let mut meta = Meta::new();
        meta.insert(NAMESPACE.to_owned(), value);
        CallToolResult::success(vec![ContentBlock::text("waiting on the bank")])
            .with_meta(Some(meta))
    }

    /// The error text for a namespace value that must not decode.
    fn refusal(value: Value) -> String {
        park_request(&with_namespace(value))
            .err()
            .unwrap_or_else(|| {
                panic!("a malformed `_meta.salvor` is refused, never treated as output")
            })
    }

    /// A result with no `_meta` at all, and one whose `_meta` belongs to
    /// somebody else, both decode to no park.
    #[test]
    fn a_result_without_the_namespace_is_not_a_park() {
        let plain = CallToolResult::success(vec![ContentBlock::text("done")]);
        assert!(matches!(park_request(&plain), Ok(None)));

        let mut meta = Meta::new();
        meta.insert("some.other.host".to_owned(), json!({"suspend": {}}));
        let foreign = plain.with_meta(Some(meta));
        assert!(
            matches!(park_request(&foreign), Ok(None)),
            "another host's `_meta` namespace is none of our business"
        );
    }

    /// The two well-formed shapes, and the gate that names no kind.
    #[test]
    fn the_two_shapes_decode() {
        let schema = json!({"type": "object", "properties": {"paid": {"type": "boolean"}}});
        let request = park_request(&with_namespace(json!({
            "suspend": {"reason": "waiting on the webhook", "input_schema": schema, "kind": "signal"}
        })))
        .expect("a well-formed suspension decodes")
        .expect("it is a park");
        match request {
            ParkRequest::Suspend(suspension) => {
                assert_eq!(suspension.reason, "waiting on the webhook");
                assert_eq!(suspension.input_schema, schema);
                assert_eq!(suspension.kind, Some(SuspensionKind::Signal));
            }
            ParkRequest::Sleep(_) => panic!("a suspension request is not a sleep"),
        }

        let gate = park_request(&with_namespace(json!({
            "suspend": {"reason": "needs sign-off", "input_schema": {"type": "object"}}
        })))
        .expect("a gate decodes")
        .expect("it is a park");
        match gate {
            ParkRequest::Suspend(suspension) => assert_eq!(
                suspension.kind, None,
                "an unnamed kind is the human gate, as it is everywhere else"
            ),
            ParkRequest::Sleep(_) => panic!("a suspension request is not a sleep"),
        }

        let sleep = park_request(&with_namespace(
            json!({"sleep_until": "2026-08-14T11:00:00+02:00"}),
        ))
        .expect("a well-formed sleep decodes")
        .expect("it is a park");
        match sleep {
            ParkRequest::Sleep(sleep) => assert_eq!(
                sleep.wake_at,
                time::macros::datetime!(2026-08-14 09:00:00 UTC),
                "the instant is the instant, whichever offset the server wrote it in"
            ),
            ParkRequest::Suspend(_) => panic!("a sleep request is not a suspension"),
        }
    }

    /// Every way a request can be wrong names `_meta.salvor` and says what
    /// went wrong. The messages are what a server author debugs against, so
    /// they are asserted rather than assumed.
    #[test]
    fn every_malformed_request_names_the_key_and_the_problem() {
        let schema = json!({"type": "object"});

        for (namespace, expected) in [
            (json!("suspend"), "must be a JSON object"),
            (json!({}), "names no park request"),
            (
                json!({"suspend": {"reason": "r", "input_schema": schema}, "sleep_until": "2026-08-14T09:00:00Z"}),
                "names both",
            ),
            (
                json!({"sleepUntil": "2026-08-14T09:00:00Z"}),
                "unknown key `sleepUntil`",
            ),
            (
                json!({"sleep_until": "tomorrow"}),
                "is not an RFC 3339 timestamp",
            ),
            (
                json!({"sleep_until": 1_800}),
                "must be an RFC 3339 timestamp string",
            ),
            (json!({"suspend": "please"}), "must be a JSON object"),
            (
                json!({"suspend": {"input_schema": schema}}),
                "is missing `reason`",
            ),
            (
                json!({"suspend": {"reason": "  ", "input_schema": schema}}),
                "is empty",
            ),
            (
                json!({"suspend": {"reason": 7, "input_schema": schema}}),
                "must be a string",
            ),
            (
                json!({"suspend": {"reason": "r"}}),
                "is missing `input_schema`",
            ),
            (
                json!({"suspend": {"reason": "r", "input_schema": "an object please"}}),
                "must be a JSON Schema object",
            ),
            (
                json!({"suspend": {"reason": "r", "input_schema": schema, "kind": "webhook"}}),
                "must be `\"signal\"` when present",
            ),
            (
                json!({"suspend": {"reason": "r", "inputSchema": schema}}),
                "unknown key `inputSchema`",
            ),
        ] {
            let message = refusal(namespace.clone());
            assert!(
                message.starts_with("`_meta.salvor"),
                "the message leads with the key, got: {message}"
            );
            assert!(
                message.contains(expected),
                "expected `{expected}` in the refusal of {namespace}, got: {message}"
            );
        }
    }

    /// A park request on a failed result is refused rather than obeyed or
    /// ignored.
    #[test]
    fn a_failed_result_may_not_park() {
        let mut meta = Meta::new();
        meta.insert(
            NAMESPACE.to_owned(),
            json!({"sleep_until": "2026-08-14T09:00:00Z"}),
        );
        let result = CallToolResult::error(vec![ContentBlock::text("the bank refused")])
            .with_meta(Some(meta));
        let message = park_request(&result).expect_err("a failing call parks nothing");
        assert!(
            message.contains("`isError`") && message.starts_with("`_meta.salvor`"),
            "the refusal names both halves of the contradiction, got: {message}"
        );
    }
}
