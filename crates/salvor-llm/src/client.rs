//! The asynchronous Messages API client.
//!
//! [`Client`] wraps a [`reqwest::Client`] and a [`Config`]. Its one public
//! action, [`Client::send_message`], sends a [`MessageRequest`] and returns a
//! [`MessageResponse`], retrying transient failures with exponential backoff
//! and jitter along the way.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::config::{AuthKind, Config};
use crate::error::{ApiError, Error};
use crate::types::{MessageRequest, MessageResponse};

/// The `anthropic-version` header value this client speaks.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The `anthropic-beta` opt-in that lets a subscription OAuth token
/// authenticate against `/v1/messages`. Sent only in [`AuthKind::Bearer`] mode.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The base backoff delay, doubled on each successive retry.
const BACKOFF_BASE_MS: u64 = 500;

/// The ceiling on a single backoff delay.
const BACKOFF_MAX_MS: u64 = 30_000;

/// The error envelope the API returns for a non-2xx response body.
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

/// An asynchronous client for the Anthropic Messages API.
///
/// Construct one from a [`Config`] with [`Client::new`], or from the
/// environment with [`Client::from_env`]. Cloning is cheap: the underlying
/// HTTP client shares its connection pool across clones.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
    url: String,
}

impl Client {
    /// Build a client from an explicit [`Config`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Transport`] if the underlying HTTP client cannot be
    /// built (for example, if the platform TLS backend fails to initialize).
    pub fn new(config: Config) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(Error::Transport)?;
        let url = format!("{}/v1/messages", config.base_url.trim_end_matches('/'));
        Ok(Self { http, config, url })
    }

    /// Build a client from [`Config::from_env`], reading `ANTHROPIC_API_KEY`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Transport`] if the underlying HTTP client cannot be
    /// built.
    pub fn from_env() -> Result<Self, Error> {
        Self::new(Config::from_env())
    }

    /// The configuration this client was built with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Send a request and return the parsed response.
    ///
    /// Retryable failures (`429`, `500`, `529`, and transport errors) are
    /// retried up to [`Config::max_retries`] times, waiting the server's
    /// `retry-after` when it sent one and otherwise an exponential backoff with
    /// jitter. A non-retryable failure returns immediately.
    ///
    /// # Errors
    ///
    /// Returns the last [`Error`] seen: an [`Error::Api`] carrying the status,
    /// API error type, and message for a non-2xx response; [`Error::Decode`]
    /// for a 2xx body that did not parse; [`Error::Transport`] for a network
    /// failure; or [`Error::Unexpected`] for a non-2xx body that was not a
    /// recognizable error envelope.
    pub async fn send_message(&self, request: &MessageRequest) -> Result<MessageResponse, Error> {
        let mut attempt: u32 = 0;
        loop {
            match self.try_send(request).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    if attempt >= self.config.max_retries || !err.is_retryable() {
                        return Err(err);
                    }
                    let delay = err.retry_after().unwrap_or_else(|| backoff_delay(attempt));
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Perform a single request attempt, with no retrying.
    async fn try_send(&self, request: &MessageRequest) -> Result<MessageResponse, Error> {
        let mut builder = self
            .http
            .post(&self.url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(request);
        // Omit every auth header when no key is configured; local endpoints
        // expect no key and reject an empty one. When a key is present, the
        // scheme decides which headers carry it: a standard API key rides
        // `x-api-key`, while an OAuth token needs `Authorization: Bearer` plus
        // the oauth beta opt-in and must not send `x-api-key` at all.
        if let Some(api_key) = &self.config.api_key {
            match self.config.auth_kind {
                AuthKind::ApiKey => {
                    builder = builder.header("x-api-key", api_key);
                }
                AuthKind::Bearer => {
                    builder = builder
                        .header("authorization", format!("Bearer {api_key}"))
                        .header("anthropic-beta", OAUTH_BETA);
                }
            }
        }

        let response = builder.send().await.map_err(Error::Transport)?;
        let status = response.status();
        let request_id = header_string(response.headers(), "request-id");
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response.bytes().await.map_err(Error::Transport)?;

        if status.is_success() {
            return serde_json::from_slice(&body).map_err(Error::Decode);
        }

        match serde_json::from_slice::<ErrorEnvelope>(&body) {
            Ok(envelope) => Err(Error::Api(ApiError {
                status: status.as_u16(),
                kind: envelope.error.kind,
                message: envelope.error.message,
                request_id,
                retry_after,
            })),
            Err(_) => Err(Error::Unexpected {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).into_owned(),
            }),
        }
    }
}

/// Read a response header as an owned `String`, if present and valid UTF-8.
fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The backoff delay for a given retry attempt (0-based).
///
/// Uses equal jitter: half the exponential window is fixed and the other half
/// is randomized, which spreads out retries from many callers without letting
/// any single delay collapse to zero. The randomness comes from the current
/// clock's sub-millisecond bits, which avoids pulling in a random-number crate
/// for a non-security use.
fn backoff_delay(attempt: u32) -> Duration {
    let window = BACKOFF_BASE_MS
        .saturating_mul(1u64 << attempt.min(6))
        .min(BACKOFF_MAX_MS);
    let half = window / 2;
    let jitter = (jitter_fraction() * half as f64) as u64;
    Duration::from_millis(half + jitter)
}

/// A pseudo-random fraction in `[0, 1)` derived from the clock.
fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1000) / 1000.0
}
