//! Client configuration.
//!
//! [`Config`] holds everything the client needs that is not part of a single
//! request: where to send requests, how to authenticate, how hard to retry,
//! and how long to wait. A [`Config`] built through [`Config::new`] or the
//! builder methods never reads the environment. Reading `ANTHROPIC_API_KEY` is
//! a separate, explicit opt-in through [`Config::from_env`].

use std::time::Duration;

/// The public Anthropic Messages API base URL.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// How the configured key authenticates a request.
///
/// The two Anthropic auth schemes differ in more than a header name. A standard
/// API key (`sk-ant-api...`) goes in `x-api-key`. A subscription OAuth token
/// (`sk-ant-oat...`, as minted by `ant auth`) is rejected on `x-api-key` and
/// must instead be sent as an `Authorization: Bearer` credential together with
/// the `anthropic-beta: oauth-2025-04-20` header. This enum selects between the
/// two; the client reads it when deciding which auth headers to attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthKind {
    /// Send the key as the `x-api-key` header. The default, for standard API
    /// keys.
    #[default]
    ApiKey,
    /// Send the key as `Authorization: Bearer <key>` and add the
    /// `anthropic-beta: oauth-2025-04-20` header, for subscription OAuth tokens.
    Bearer,
}

/// Settings shared across every request a [`crate::Client`] makes.
#[derive(Debug, Clone)]
pub struct Config {
    /// The base URL requests are sent to. The client appends `/v1/messages`.
    /// Point this at a local server (LM Studio, Ollama) to talk to a local
    /// model over the same wire protocol.
    pub base_url: String,
    /// The API key sent to authenticate the request. When `None`, no auth
    /// header is sent at all, which is what local endpoints expect. How a
    /// present key is sent is governed by [`Config::auth_kind`].
    pub api_key: Option<String>,
    /// Which scheme authenticates the key. Defaults to [`AuthKind::ApiKey`]
    /// (the `x-api-key` header); [`AuthKind::Bearer`] switches to the OAuth
    /// bearer scheme. Ignored when `api_key` is `None`.
    pub auth_kind: AuthKind,
    /// How many times to retry a retryable failure before giving up. `0`
    /// disables retrying.
    pub max_retries: u32,
    /// The per-request timeout applied to the underlying HTTP client.
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            auth_kind: AuthKind::ApiKey,
            max_retries: 2,
            timeout: Duration::from_secs(60),
        }
    }
}

impl Config {
    /// A configuration with default settings: the public base URL, no API key,
    /// two retries, and a 60 second timeout. Does not read the environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read `ANTHROPIC_API_KEY` from the environment into the API key field,
    /// leaving every other setting at its default. This is the only place the
    /// crate touches the environment, and only when you call it. When the
    /// variable is unset or empty, the key stays `None`.
    #[must_use]
    pub fn from_env() -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.is_empty());
        Self {
            api_key,
            ..Self::default()
        }
    }

    /// Set the API key, consuming and returning `self` for chaining.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the authentication scheme, consuming and returning `self`. Leave it
    /// at the default [`AuthKind::ApiKey`] for standard API keys; pass
    /// [`AuthKind::Bearer`] for subscription OAuth tokens.
    #[must_use]
    pub fn with_auth_kind(mut self, auth_kind: AuthKind) -> Self {
        self.auth_kind = auth_kind;
        self
    }

    /// Set the base URL, consuming and returning `self` for chaining. A
    /// trailing slash is fine; the client trims it before appending the path.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the maximum number of retries, consuming and returning `self`.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set the per-request timeout, consuming and returning `self`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}
