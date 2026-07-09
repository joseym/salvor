//! Error types for the Messages API client.
//!
//! Every failure the client can produce is one variant of [`Error`]. The
//! variants are grouped so that the retry logic in [`crate::Client`] can ask a
//! single question about any failure: "is this worth trying again?" See
//! [`Error::is_retryable`].

use std::time::Duration;

use thiserror::Error;

/// A structured error returned by the Messages API for a non-2xx response.
///
/// The wire body of an API error looks like
/// `{"type":"error","error":{"type":...,"message":...},"request_id":...}`.
/// This struct carries the parts a caller acts on: the HTTP status, the API's
/// own error type string, the human-readable message, and (for `429`) the
/// `retry-after` delay the server asked us to wait.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// The HTTP status code of the response (for example `400` or `429`).
    pub status: u16,
    /// The API's error type string, taken from `error.type` in the body
    /// (for example `invalid_request_error` or `rate_limit_error`).
    pub kind: String,
    /// The human-readable message from `error.message`.
    pub message: String,
    /// The `request-id` response header, when the server sent one. Useful when
    /// reporting a failure to Anthropic.
    pub request_id: Option<String>,
    /// The `retry-after` delay parsed from the response header, when present.
    /// Only a `429` response is expected to set this.
    pub retry_after: Option<Duration>,
}

/// Everything that can go wrong when calling the Messages API.
#[derive(Debug, Error)]
pub enum Error {
    /// The HTTP request never produced a usable response: a connection failure,
    /// a timeout, or a request that could not be built or sent. Treated as
    /// retryable, since these are usually transient.
    #[error("HTTP transport failure while calling the Messages API")]
    Transport(#[source] reqwest::Error),

    /// A 2xx response arrived but its body did not deserialize into the
    /// expected shape. Not retried: the same body would fail again.
    #[error("could not deserialize the Messages API response body")]
    Decode(#[source] serde_json::Error),

    /// The API returned a non-2xx response with a well-formed error envelope.
    /// Retryable only for `429`, `500`, and `529`.
    #[error("Messages API returned HTTP {} ({}): {}", .0.status, .0.kind, .0.message)]
    Api(ApiError),

    /// A non-2xx response whose body was not a recognizable error envelope. The
    /// raw body is preserved so the caller can see what the server actually
    /// sent. Not retried.
    #[error("unexpected HTTP {status} response from the Messages API: {body}")]
    Unexpected {
        /// The HTTP status code of the response.
        status: u16,
        /// The raw response body, decoded lossily as UTF-8.
        body: String,
    },
}

impl Error {
    /// Whether trying the request again could plausibly succeed.
    ///
    /// Transport failures are always worth a retry. API errors are retried for
    /// the three statuses the protocol calls out as transient: `429` (rate
    /// limited), `500` (server error), and `529` (overloaded). Decode failures
    /// and other non-2xx responses are permanent for this request.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Api(api) => matches!(api.status, 429 | 500 | 529),
            // A 429/500/529 whose body was not a recognizable envelope is still
            // a transient status and worth retrying.
            Error::Unexpected { status, .. } => matches!(status, 429 | 500 | 529),
            Error::Decode(_) => false,
        }
    }

    /// The server-requested wait before retrying, when the response carried a
    /// `retry-after` header. The retry loop honours this in preference to its
    /// own backoff schedule.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Api(api) => api.retry_after,
            _ => None,
        }
    }
}
