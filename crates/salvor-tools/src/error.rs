//! The two error surfaces of the tool layer: [`HandlerError`], which a tool's
//! own code returns, and [`ToolError`], which the type-erased dispatch layer
//! returns.
//!
//! Keeping them separate is the point. The runtime loop treats a bad
//! input from the model differently from a tool that ran and failed: the first
//! is fed back to the model so it can correct its arguments, the second is
//! recorded and surfaced per the effect's retry policy. That difference is
//! encoded as distinct [`ToolError`] variants, not left to string matching.

use thiserror::Error;

/// The error a [`ToolHandler`](crate::ToolHandler) returns from its own code.
///
/// A handler is only ever called with an already-typed, already-validated
/// input, so it never has to report a schema mismatch. What it reports is a
/// genuine failure of the work it does: a provider rejected the request, a
/// precondition did not hold, and so on. This type carries a human-readable
/// message and an optional underlying source error.
///
/// The dispatch layer wraps a returned `HandlerError` in
/// [`ToolError::Handler`], tagging it with the tool name.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct HandlerError {
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl HandlerError {
    /// Builds a `HandlerError` from a message alone, with no underlying source.
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Builds a `HandlerError` that wraps an underlying error as its source.
    ///
    /// The message is taken from the source's `Display` output, so the wrapped
    /// error's own text is preserved. Use this to forward a `?`-propagated
    /// error out of a handler: `some_call().map_err(HandlerError::new)?`.
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>) -> Self {
        let source = source.into();
        Self {
            message: source.to_string(),
            source: Some(source),
        }
    }
}

/// The error the type-erased dispatch layer
/// ([`DynTool::call_json`](crate::DynTool::call_json)) returns.
///
/// The variants are deliberately distinct so the runtime loop can route on them:
///
/// - [`ToolError::InvalidInput`] means the JSON the model produced did not
///   deserialize into the tool's `Input` type. The handler was **not** called.
///   The loop feeds this back to the model to let it fix its arguments.
/// - [`ToolError::Handler`] means the handler ran and returned a
///   [`HandlerError`]. This is a real execution failure, subject to the
///   effect's [retry policy](crate::RetryPolicy).
/// - [`ToolError::OutputSerialization`] means the handler succeeded but its
///   `Output` value could not be serialized to JSON. This is an internal fault
///   in the tool definition, not the model's doing and not a retryable failure.
/// - [`ToolError::MissingIdempotencyKey`] means the operator declared which
///   input field identifies this tool's calls and the call does not carry it.
///   The tool was **not** called, and it will not be called unkeyed. Like
///   `InvalidInput` this is the arguments being wrong, so the loop feeds it back
///   to the model rather than retrying it.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The model's JSON input did not match the tool's input schema. The
    /// handler was never invoked.
    #[error("input for tool `{tool}` did not match its schema: {source}")]
    InvalidInput {
        /// The tool that rejected the input.
        tool: String,
        /// The deserialization error describing why the input was rejected.
        #[source]
        source: serde_json::Error,
    },
    /// The handler ran and returned a failure.
    #[error("tool `{tool}` failed")]
    Handler {
        /// The tool that failed.
        tool: String,
        /// The handler's own error.
        #[source]
        source: HandlerError,
    },
    /// The tool has a declared idempotency key path and this call's input does
    /// not yield a key from it. Nothing ran.
    ///
    /// See [`IdempotencyPath::derive`](crate::IdempotencyPath::derive) for what
    /// counts as a key and why falling back to an unkeyed call is not an option
    /// here.
    #[error(
        "tool `{tool}` declares idempotency key path `{path}`, but this call's input yields no key: {detail}. Nothing ran: a tool whose calls carry an identity is never called without one"
    )]
    MissingIdempotencyKey {
        /// The tool whose call was refused.
        tool: String,
        /// The declared path, as the operator wrote it.
        path: String,
        /// What went wrong at that path, including the keys the input carries.
        detail: String,
    },
    /// The handler succeeded but its output could not be serialized to JSON.
    #[error("tool `{tool}` produced output that could not be serialized: {source}")]
    OutputSerialization {
        /// The tool whose output failed to serialize.
        tool: String,
        /// The serialization error.
        #[source]
        source: serde_json::Error,
    },
}
