//! The recorded wire shapes this crate layers on top of the core event
//! vocabulary: the suspension and sleep sentinels, the structured tool-error
//! object, and the deterministic rendering of JSON values into model-visible
//! text.
//!
//! # Why sentinels exist
//!
//! The replay cursor requires every tool intent to be followed by
//! exactly one `ToolCallCompleted`. Three tool outcomes do not produce a plain
//! output value, and all three are encoded *inside* the completion's `output`
//! field as a reserved-key JSON object:
//!
//! - **Suspension.** A live tool that returns `ToolOutcome::Suspend` still
//!   gets a completion; its output is the sentinel
//!
//!   ```json
//!   {"__salvor_suspend": {"reason": "<reason>", "input_schema": { ... }}}
//!   ```
//!
//!   The runtime then records `Suspended` and parks. On replay the same
//!   completion replays through the cursor, decodes back into a suspension,
//!   and the orchestration takes the identical path.
//!
//! - **Sleep.** A live tool that returns `ToolOutcome::Sleep` gets a
//!   completion too; its output is
//!
//!   ```json
//!   {"__salvor_sleep": {"wake_at": "2026-08-14T09:00:00Z"}}
//!   ```
//!
//!   `wake_at` is RFC 3339 normalized to UTC, the same reading rule every
//!   recorded instant in a log follows. The runtime then records
//!   `SleepStarted` and parks.
//!
//!   **Why this is a sentinel and not an event.** The completion settles the
//!   tool call: it closes the intent, and for a keyed call it settles the
//!   store's claim in the same atomic append. Only then is `SleepStarted`
//!   recorded. So the recorded order is intent, completion, `SleepStarted`,
//!   and a run that sleeps for a week holds no claim while it sleeps and
//!   leaves no dangling write intent if the process dies. A sleep event
//!   emitted *instead of* a completion would invert that: the claim would be
//!   held for the length of the timer and every other run under that
//!   idempotency key would get `CallInFlight` until it expired.
//!
//! - **Failure.** A tool call that exhausted its retries (or was never
//!   retryable) also gets a completion; its output is
//!
//!   ```json
//!   {"__salvor_error": {"is_error": true, "kind": "handler",
//!                      "message": "<full error chain>", "attempts": 2}}
//!   ```
//!
//!   `kind` is one of `"invalid_input"`, `"handler"`, or
//!   `"output_serialization"` (the three `ToolError` variants), `message` is
//!   the **full** error chain (sources joined with `": "`), and `attempts`
//!   counts executions including retries. The full error always lives here,
//!   in the log; what reaches the model is compacted separately (see
//!   [`crate::compact`]).
//!
//! The three keys are reserved: a tool's own output must not be a one-key
//! object using any of these names at the top level, or replay would misread
//! it. Decoding only triggers on a JSON object with exactly one key equal to
//! the sentinel, which keeps ordinary outputs (including objects that merely
//! *contain* these names deeper down) unaffected.

use salvor_tools::{Sleep, Suspension};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::hash::canonical_json;

/// The reserved key marking a completion output as a recorded suspension.
pub const SUSPEND_SENTINEL_KEY: &str = "__salvor_suspend";

/// The reserved key marking a completion output as a recorded sleep request.
pub const SLEEP_SENTINEL_KEY: &str = "__salvor_sleep";

/// The reserved key marking a completion output as a recorded tool failure.
pub const ERROR_SENTINEL_KEY: &str = "__salvor_error";

/// Which layer of the tool dispatch produced a recorded failure. Mirrors the
/// three `salvor_tools::ToolError` variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolFailureKind {
    /// The model's arguments did not match the tool's input schema; the tool
    /// never ran.
    InvalidInput,
    /// The tool ran and its handler failed.
    Handler,
    /// The tool succeeded but its output could not be serialized.
    OutputSerialization,
}

impl ToolFailureKind {
    /// The wire string recorded in the failure object's `kind` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Handler => "handler",
            Self::OutputSerialization => "output_serialization",
        }
    }

    /// Parses a recorded `kind` string back into the enum.
    #[must_use]
    pub fn from_wire(kind: &str) -> Option<Self> {
        match kind {
            "invalid_input" => Some(Self::InvalidInput),
            "handler" => Some(Self::Handler),
            "output_serialization" => Some(Self::OutputSerialization),
            _ => None,
        }
    }
}

/// A tool failure as recorded in (and decoded from) a completion output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFailure {
    /// Which dispatch layer failed.
    pub kind: ToolFailureKind,
    /// The full error chain: the top error's message and every source,
    /// joined with `": "`. Never truncated here; compaction happens only on
    /// the way into model context.
    pub message: String,
    /// How many times the call executed, counting retries.
    pub attempts: u32,
}

impl ToolFailure {
    /// Builds a failure record from a dispatch error, capturing the full
    /// source chain into [`message`](Self::message).
    #[must_use]
    pub fn from_error(error: &salvor_tools::ToolError, attempts: u32) -> Self {
        let kind = match error {
            salvor_tools::ToolError::InvalidInput { .. } => ToolFailureKind::InvalidInput,
            // A call refused for want of its declared idempotency key never
            // reached the tool, and the fault is in the arguments, which is
            // exactly what `invalid_input` records. It shares that wire kind
            // rather than minting a new one: the recorded `kind` strings are a
            // stable format that replay parses, and this failure needs no new
            // handling, only its own message.
            salvor_tools::ToolError::MissingIdempotencyKey { .. } => ToolFailureKind::InvalidInput,
            salvor_tools::ToolError::Handler { .. } => ToolFailureKind::Handler,
            salvor_tools::ToolError::OutputSerialization { .. } => {
                ToolFailureKind::OutputSerialization
            }
        };
        Self {
            kind,
            message: error_chain(error),
            attempts,
        }
    }
}

/// Joins an error's `Display` with every `source()` below it, separated by
/// `": "`, so the recorded message keeps the information `thiserror` spreads
/// across the chain.
#[must_use]
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}

/// Encodes a suspension as the sentinel completion output.
#[must_use]
pub fn encode_suspension(suspension: &Suspension) -> Value {
    json!({
        SUSPEND_SENTINEL_KEY: {
            "reason": suspension.reason,
            "input_schema": suspension.input_schema,
        }
    })
}

/// Encodes a sleep request as the sentinel completion output.
///
/// The instant is normalized to UTC before it is formatted, so a tool that
/// returns `wake_at` in some other offset records the same instant every other
/// recorded instant in the log is written in, and the value decoded back is
/// the value the run sleeps on. It also removes the one way RFC 3339
/// formatting can fail on a representable instant (an offset carrying
/// seconds), which is what lets this return a `Value` rather than a `Result`.
#[must_use]
pub fn encode_sleep(sleep: &Sleep) -> Value {
    json!({ SLEEP_SENTINEL_KEY: { "wake_at": rfc3339(sleep.wake_at) } })
}

/// Encodes a tool failure as the sentinel completion output.
#[must_use]
pub fn encode_failure(failure: &ToolFailure) -> Value {
    json!({
        ERROR_SENTINEL_KEY: {
            "is_error": true,
            "kind": failure.kind.as_str(),
            "message": failure.message,
            "attempts": failure.attempts,
        }
    })
}

/// Decodes a completion output that is the suspension sentinel, if it is one.
#[must_use]
pub fn decode_suspension(output: &Value) -> Option<Suspension> {
    let body = sentinel_body(output, SUSPEND_SENTINEL_KEY)?;
    Some(Suspension {
        reason: body.get("reason")?.as_str()?.to_owned(),
        input_schema: body.get("input_schema")?.clone(),
    })
}

/// Decodes a completion output that is the sleep sentinel, if it is one.
#[must_use]
pub fn decode_sleep(output: &Value) -> Option<Sleep> {
    let body = sentinel_body(output, SLEEP_SENTINEL_KEY)?;
    let wake_at = OffsetDateTime::parse(body.get("wake_at")?.as_str()?, &Rfc3339).ok()?;
    Some(Sleep { wake_at })
}

/// What a woken tool call reports as its result: the tool asked to park until
/// an instant, the run parked, and the instant came.
///
/// A slept call has no output of its own to hand back (the tool returned a
/// deadline, not a value), and there is no resume input to stand in for one
/// either, so the result is derived: a fixed key over the recorded wake
/// instant. Both drivers use this one function, so a `tool` node's recorded
/// output and the `tool_result` the built-in loop feeds the model say the same
/// thing, and both are pure functions of the log.
#[must_use]
pub fn slept_output(wake_at: OffsetDateTime) -> Value {
    json!({ "slept_until": rfc3339(wake_at) })
}

/// Formats an instant as RFC 3339 in UTC, the encoding every recorded instant
/// in a log uses.
///
/// Infallible in practice, and deliberately: normalizing to UTC rules out an
/// offset with seconds, and without the `time` crate's `large-dates` feature
/// an `OffsetDateTime` cannot hold a year outside 0000..=9999. Those are the
/// only two ways RFC 3339 formatting fails.
fn rfc3339(instant: OffsetDateTime) -> String {
    instant
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .expect("an instant a run can hold formats as RFC 3339 in UTC")
}

/// Decodes a completion output that is the failure sentinel, if it is one.
#[must_use]
pub fn decode_failure(output: &Value) -> Option<ToolFailure> {
    let body = sentinel_body(output, ERROR_SENTINEL_KEY)?;
    Some(ToolFailure {
        kind: ToolFailureKind::from_wire(body.get("kind")?.as_str()?)?,
        message: body.get("message")?.as_str()?.to_owned(),
        attempts: u32::try_from(body.get("attempts")?.as_u64()?).ok()?,
    })
}

/// The sentinel's body when `output` is an object with exactly one key equal
/// to `key`; `None` for every other value.
fn sentinel_body<'v>(output: &'v Value, key: &str) -> Option<&'v Value> {
    let map = output.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.get(key)
}

/// Renders a JSON value as the text handed to the model (an initial input or
/// a `tool_result` content string).
///
/// A JSON string renders as its bare text; anything else renders as
/// canonical JSON. The canonical form matters: this text flows into the next
/// model request, the request is hashed, and the hash must reproduce on
/// replay, so the rendering cannot depend on map iteration order.
#[must_use]
pub fn content_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => canonical_json(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sentinel survives an encode/decode round trip.
    #[test]
    fn sentinels_round_trip() {
        let suspension = Suspension {
            reason: "needs approval".to_owned(),
            input_schema: json!({"type": "object", "required": ["approved"]}),
        };
        assert_eq!(
            decode_suspension(&encode_suspension(&suspension)),
            Some(suspension)
        );

        let sleep = Sleep::until(time::macros::datetime!(2026-08-14 09:00:00 UTC));
        assert_eq!(decode_sleep(&encode_sleep(&sleep)), Some(sleep));

        let failure = ToolFailure {
            kind: ToolFailureKind::Handler,
            message: "tool `x` failed: connection reset".to_owned(),
            attempts: 3,
        };
        assert_eq!(decode_failure(&encode_failure(&failure)), Some(failure));
    }

    /// Ordinary outputs never decode as sentinels, even when they contain
    /// the reserved names below the top level or alongside other keys.
    #[test]
    fn ordinary_outputs_are_not_sentinels() {
        assert_eq!(decode_suspension(&json!({"result": 1})), None);
        assert_eq!(
            decode_suspension(&json!({"__salvor_suspend": {}, "other": 1})),
            None
        );
        assert_eq!(
            decode_failure(&json!({"nested": {"__salvor_error": {}}})),
            None
        );
        assert_eq!(decode_failure(&json!("__salvor_error")), None);
        assert_eq!(
            decode_sleep(&json!({"wake_at": "2026-08-14T09:00:00Z"})),
            None
        );
        assert_eq!(
            decode_sleep(&json!({SLEEP_SENTINEL_KEY: {"wake_at": "tomorrow"}})),
            None
        );
    }

    /// A wake instant recorded in another offset comes back as the same
    /// instant in UTC, so the deadline the run sleeps on is the deadline the
    /// tool asked for however the tool spelled it.
    #[test]
    fn a_sleep_instant_records_in_utc() {
        let sleep = Sleep::until(time::macros::datetime!(2026-08-14 11:00:00 +02:00));
        assert_eq!(
            encode_sleep(&sleep),
            json!({SLEEP_SENTINEL_KEY: {"wake_at": "2026-08-14T09:00:00Z"}})
        );
        assert_eq!(
            decode_sleep(&encode_sleep(&sleep)).map(|decoded| decoded.wake_at),
            Some(time::macros::datetime!(2026-08-14 09:00:00 UTC))
        );
    }

    /// String values render bare; structured values render canonically.
    #[test]
    fn content_string_renders_deterministically() {
        assert_eq!(content_string(&json!("plain")), "plain");
        let a: Value = serde_json::from_str(r#"{"b": 1, "a": 2}"#).unwrap();
        assert_eq!(content_string(&a), r#"{"a":2,"b":1}"#);
    }
}
