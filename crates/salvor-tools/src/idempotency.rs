//! Deriving a declared idempotency key from a call's input.
//!
//! A hand-written [`ToolHandler`](crate::ToolHandler) can say what a call *is*
//! by overriding [`idempotency_key`](crate::ToolHandler::idempotency_key). A
//! tool that arrives at runtime, from an MCP server or a wasm component, has no
//! Rust code to override anything in: its author is not in this process, and
//! for MCP is not even on this machine. What the operator has instead is the
//! call's input and the knowledge of which field of it names the effect. This
//! module turns that knowledge into a key.
//!
//! # The declaration
//!
//! An [`IdempotencyPath`] is a field path into the call's input:
//! `"claim_id"`, or `"payment.claim_id"` for a nested field. The operator
//! writes it once, per tool, in the agent file (`idempotency_keys` on an MCP
//! server, `idempotency_key` on a wasm tool); everything below is what that
//! declaration means at dispatch.
//!
//! # The key
//!
//! The derived key is `<tool>:<value>`: the tool's own name, a colon, and the
//! field's value verbatim. So a `pay_claim` call carrying
//! `{"claim_id": "wreck-9931"}` is the identity `pay_claim:wreck-9931`, which
//! is exactly the key a hand-written payout tool declares in
//! [`DynTool::idempotency_key`](crate::DynTool::idempotency_key)'s own
//! documentation. Two properties make that format worth pinning:
//!
//! - It is stable. The same input yields the same key in this process and the
//!   next one, on any machine, with no clock, counter, or randomness folded in.
//!   A key that moved would silently stop deduplicating.
//! - It is legible on its own. The store already keys commitments on
//!   `(tool, key)`, so the prefix is redundant *there*; it is not redundant in
//!   a `salvor history` line, a `CallInFlight` refusal, or an operator's grep,
//!   where a bare `wreck-9931` says nothing about what was done to the claim.
//!
//! The value is taken verbatim for a string and through its JSON form for a
//! number (`483200`, `1.5`). Nothing else is accepted: see
//! [`IdempotencyPath::derive`].
//!
//! # A declared key never degrades to no key
//!
//! If the path is not there, or holds something that cannot be an identity, the
//! call is refused with [`ToolError::MissingIdempotencyKey`] and nothing runs.
//! The alternative would be to fall back to an unkeyed call, which for the
//! payout tool this exists for means the second run pays again. An operator who
//! declared a key asked for exactly one execution; a loud refusal is the only
//! answer that keeps that promise.

use serde_json::Value;

use crate::error::ToolError;

/// A field path into a call's input, naming the value that identifies the
/// operation.
///
/// Parsed once, at agent-build time, so a malformed declaration fails before a
/// run rather than during one. See the [module docs](self) for the key format
/// and the refusal rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdempotencyPath {
    /// The path as the operator wrote it, for error messages.
    raw: String,
    /// The dot-separated segments, in order.
    segments: Vec<String>,
}

impl IdempotencyPath {
    /// Parses a dotted field path.
    ///
    /// `"claim_id"` is one segment; `"payment.claim_id"` is two, walked as
    /// object lookups in order. There is no array indexing and no escaping: a
    /// field whose name contains a dot cannot be named, which is a limit worth
    /// having while the syntax is one character wide.
    ///
    /// # Errors
    ///
    /// [`IdempotencyPathError::Empty`] for an empty path, and
    /// [`IdempotencyPathError::EmptySegment`] for a path with an empty
    /// segment (`"a."`, `".a"`, `"a..b"`). Both are typos, and a typo in the
    /// declaration that names a payment must not survive to a run.
    pub fn parse(path: &str) -> Result<Self, IdempotencyPathError> {
        if path.is_empty() {
            return Err(IdempotencyPathError::Empty);
        }
        let segments: Vec<String> = path.split('.').map(ToOwned::to_owned).collect();
        if segments.iter().any(String::is_empty) {
            return Err(IdempotencyPathError::EmptySegment {
                path: path.to_owned(),
            });
        }
        Ok(Self {
            raw: path.to_owned(),
            segments,
        })
    }

    /// The path as the operator wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Derives the key for one call, or refuses the call.
    ///
    /// Returns `<tool>:<value>` where `value` is the non-empty string or the
    /// number at this path in `input`.
    ///
    /// # Errors
    ///
    /// [`ToolError::MissingIdempotencyKey`] when the path is absent, runs
    /// through a non-object, or lands on anything but a non-empty string or a
    /// number. A boolean, a null, an object, an array, and an empty string are
    /// all refused: none of them names one operation, and a key that does not
    /// name one operation is worse than no key at all, because it looks like an
    /// identity while being a collision. The message names the tool, the path,
    /// and the keys the input actually carries.
    pub fn derive(&self, tool: &str, input: &Value) -> Result<String, ToolError> {
        let refuse = |detail: String| ToolError::MissingIdempotencyKey {
            tool: tool.to_owned(),
            path: self.raw.clone(),
            detail,
        };

        let mut current = input;
        for (index, segment) in self.segments.iter().enumerate() {
            let Some(object) = current.as_object() else {
                return Err(refuse(format!(
                    "{} is not a JSON object, so `{segment}` cannot be looked up in it; {}",
                    location(&self.segments, index),
                    present_keys(input)
                )));
            };
            let Some(next) = object.get(segment) else {
                return Err(refuse(format!(
                    "there is no `{segment}` in {}; {}",
                    location(&self.segments, index),
                    present_keys(input)
                )));
            };
            current = next;
        }

        match current {
            Value::String(value) if !value.is_empty() => Ok(format!("{tool}:{value}")),
            Value::Number(value) => Ok(format!("{tool}:{value}")),
            other => Err(refuse(format!(
                "`{}` holds {}, and an idempotency key must be a non-empty string or a number; {}",
                self.raw,
                describe(other),
                present_keys(input)
            ))),
        }
    }
}

/// Names the place a lookup happened, for an error message: the input itself
/// for the first segment, the consumed prefix for any later one.
fn location(segments: &[String], index: usize) -> String {
    if index == 0 {
        "the call's input".to_owned()
    } else {
        format!("`{}`", segments[..index].join("."))
    }
}

/// The keys the input carries, as a message fragment. This is the part an
/// operator reads to see whether the declaration or the caller is wrong.
fn present_keys(input: &Value) -> String {
    match input.as_object() {
        Some(object) if object.is_empty() => "the input has no keys".to_owned(),
        Some(object) => format!(
            "the input's keys are: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
        None => format!("the input is not an object; it is {}", describe(input)),
    }
}

/// A short human name for a JSON value's kind, with the degenerate string
/// called out by name so an empty `claim_id` does not read as a type error.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(s) if s.is_empty() => "an empty string",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// What a declared path can be wrong about before it ever sees a call.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdempotencyPathError {
    /// The path was empty. There is no field named "", so this is always a
    /// mistake, and an operator who meant "no key" leaves the declaration out.
    #[error(
        "an idempotency key path cannot be empty; name the input field that identifies the operation, for example \"claim_id\" or \"payment.claim_id\""
    )]
    Empty,
    /// The path had an empty segment: a leading, trailing, or doubled dot.
    #[error(
        "idempotency key path `{path}` has an empty segment; write dotted paths as `payment.claim_id`, with no leading, trailing, or doubled dots"
    )]
    EmptySegment {
        /// The path as written.
        path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The documented key format, for the two value kinds that are allowed.
    #[test]
    fn the_key_is_the_tool_name_and_the_field_value() {
        let path = IdempotencyPath::parse("claim_id").expect("parses");
        let key = path
            .derive("pay_claim", &json!({"claim_id": "wreck-9931"}))
            .expect("derives");
        assert_eq!(key, "pay_claim:wreck-9931");

        let path = IdempotencyPath::parse("invoice").expect("parses");
        let key = path
            .derive("charge", &json!({"invoice": 483_200}))
            .expect("derives");
        assert_eq!(key, "charge:483200");
    }

    /// The same input yields the same key, whatever else is in the input and
    /// whatever order it arrived in. This is the property the whole promise
    /// rests on, so it gets its own test.
    #[test]
    fn the_key_is_a_pure_function_of_the_named_field() {
        let path = IdempotencyPath::parse("claim_id").expect("parses");
        let first = path
            .derive(
                "pay_claim",
                &json!({"claim_id": "wreck-9931", "amount_cents": 1}),
            )
            .expect("derives");
        let second = path
            .derive(
                "pay_claim",
                &json!({"amount_cents": 1, "claim_id": "wreck-9931"}),
            )
            .expect("derives");
        assert_eq!(first, second);
    }

    /// A dotted path walks nested objects.
    #[test]
    fn a_dotted_path_reads_a_nested_field() {
        let path = IdempotencyPath::parse("payment.claim_id").expect("parses");
        let key = path
            .derive("pay_claim", &json!({"payment": {"claim_id": "wreck-9931"}}))
            .expect("derives");
        assert_eq!(key, "pay_claim:wreck-9931");
    }

    /// The refusal names the tool, the path, and the keys the input does
    /// carry: the three facts an operator needs to tell a wrong declaration
    /// from a wrong call.
    #[test]
    fn a_missing_field_refuses_and_says_what_was_there() {
        let path = IdempotencyPath::parse("claim_id").expect("parses");
        let error = path
            .derive(
                "pay_claim",
                &json!({"amount_cents": 483_200, "currency": "USD"}),
            )
            .expect_err("a missing key field must refuse");
        let message = error.to_string();
        assert!(message.contains("pay_claim"), "names the tool: {message}");
        assert!(message.contains("claim_id"), "names the path: {message}");
        assert!(
            message.contains("amount_cents, currency"),
            "names the keys present: {message}"
        );
    }

    /// A nested path says where the walk stopped, so `payment.claim_id`
    /// against a payload with no `payment` reads differently from one whose
    /// `payment` lacks the field.
    #[test]
    fn a_nested_miss_names_where_it_stopped() {
        let path = IdempotencyPath::parse("payment.claim_id").expect("parses");
        let error = path
            .derive("pay_claim", &json!({"payment": {"amount_cents": 1}}))
            .expect_err("a missing nested field must refuse");
        let message = error.to_string();
        assert!(message.contains("`payment`"), "names the prefix: {message}");
        assert!(
            message.contains("no `claim_id`"),
            "names the segment: {message}"
        );
    }

    /// Anything that is not a non-empty string or a number is refused, because
    /// none of them names one operation.
    #[test]
    fn a_value_that_cannot_be_an_identity_refuses() {
        let path = IdempotencyPath::parse("claim_id").expect("parses");
        for input in [
            json!({"claim_id": true}),
            json!({"claim_id": null}),
            json!({"claim_id": ""}),
            json!({"claim_id": ["wreck-9931"]}),
            json!({"claim_id": {"id": "wreck-9931"}}),
        ] {
            let error = path
                .derive("pay_claim", &input)
                .expect_err("only a non-empty string or a number is a key");
            let message = error.to_string();
            assert!(message.contains("claim_id"), "names the path: {message}");
            assert!(
                message.contains("non-empty string or a number"),
                "teaches the rule: {message}"
            );
        }
    }

    /// A path that runs through a non-object says so rather than reporting the
    /// field as merely absent.
    #[test]
    fn a_path_through_a_non_object_refuses() {
        let path = IdempotencyPath::parse("payment.claim_id").expect("parses");
        let error = path
            .derive("pay_claim", &json!({"payment": "wreck-9931"}))
            .expect_err("a scalar cannot be walked into");
        assert!(
            error.to_string().contains("is not a JSON object"),
            "says what went wrong: {error}"
        );
    }

    /// The parse-time rejections: an empty path and an empty segment.
    #[test]
    fn a_malformed_path_fails_at_parse() {
        assert_eq!(IdempotencyPath::parse(""), Err(IdempotencyPathError::Empty));
        for path in ["a.", ".a", "a..b", "."] {
            assert!(
                matches!(
                    IdempotencyPath::parse(path),
                    Err(IdempotencyPathError::EmptySegment { .. })
                ),
                "`{path}` must be rejected"
            );
        }
    }
}
