//! The model-executor seam: the general injection point a host supplies so the
//! server can perform a model call on a client-driven run's behalf.
//!
//! # Why it is a seam, not baked in
//!
//! This mirrors the [`AgentFactory`](crate::AgentFactory) decision exactly. The
//! server owns the *mechanism* of a durable model step (append the write-ahead
//! intent, perform the call, append the completion, answer retries from the
//! log), but it must not own the *policy* of which provider runs, where its
//! credential comes from, or how the request is shaped. Salvor is for anyone
//! building on it, browser or backend; aarg is only the first consumer. So the
//! executor is a trait the embedding binary implements and injects, never a
//! hard-wired provider.
//!
//! `salvor serve` wires a default [`LlmModelExecutor`] from its own
//! client-construction path, so the feature works out of the box; another host
//! (a future `aarg serve`) injects its own executor resolving a keychain
//! credential, and nothing consumer-specific leaks into salvor-server.
//!
//! # The two methods
//!
//! - [`ModelExecutor::execute`] performs a single call and returns the assembled
//!   [`MessageResponse`]. This backs the non-streaming model step.
//! - [`ModelExecutor::open_stream`] opens the provider stream and returns a
//!   [`ModelStream`] the server pumps: each event feeds a live ticker frame and
//!   a [`MessageAccumulator`], and the assembled completion is recorded once at
//!   the end. This backs the server-sent-events model step.
//!
//! The request crosses the seam as a raw [`Value`] (the caller's canonical
//! request JSON), not a typed `MessageRequest`, because the server hashes and
//! records exactly those bytes: the hash it recorded is the hash it sent.
//!
//! An executor error is a plain `String`, the same human-message convention the
//! agent factory uses. A provider failure maps to the server error envelope
//! without a completion being recorded, so the intent is left dangling (legal,
//! the crash story) and the run stays drivable.
//!
//! # Naming the key on a 401, the way the CLI already does
//!
//! A raw 401 from `salvor_llm::Error::Api` says the request was rejected; it
//! says nothing about where to fix it. The CLI's `contextualize_auth_error`
//! (`salvor-cli/src/commands.rs`) solves this for the agent-driven run/resume
//! path because it still has the agent's own `[llm] api_key_env` in scope. This
//! executor has no such per-agent config to read: a client-driven run's model
//! step is served by the one [`LlmModelExecutor`] `salvor serve` wires for the
//! whole process, built by `Config::from_env`, which only ever reads
//! `ANTHROPIC_API_KEY` (see `salvor-llm/src/config.rs`). So `contextualize_401`
//! names that fixed variable directly, and points at the machine running the
//! server rather than the client's own environment: for a client-driven run the
//! key lives with the server, not the caller driving it over HTTP.

use async_trait::async_trait;
use salvor_llm::{Client, Error, MessageResponse, MessageStream, StreamEvent};
use serde_json::Value;

/// The environment variable `Config::from_env` reads for the client-driven
/// model step's executor. Fixed, not per-agent: see the module doc above.
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Turns a 401 from the Messages API into a message naming `ANTHROPIC_API_KEY`
/// and where to set it; every other error passes through as `to_string()`.
fn contextualize_401(error: Error) -> String {
    let Error::Api(ref api) = error else {
        return error.to_string();
    };
    if api.status != 401 {
        return error.to_string();
    }
    format!(
        "authentication failed calling the Messages API (HTTP 401: {}). The client-driven \
         model step reads its API key from the `{API_KEY_ENV}` environment variable on this \
         server (the key lives with the server, not the client driving the run). Export \
         `{API_KEY_ENV}` where `salvor serve` runs and try again.",
        api.message,
    )
}

/// Performs a model call on behalf of a client-driven run. Injected by the
/// embedding binary, exactly like [`AgentFactory`](crate::AgentFactory).
#[async_trait]
pub trait ModelExecutor: Send + Sync {
    /// Perform a single (non-streaming) model call and return the response.
    ///
    /// # Errors
    ///
    /// A human message when the provider call fails. The server maps it to the
    /// error envelope and records no completion.
    async fn execute(&self, request: Value) -> Result<MessageResponse, String>;

    /// Open a streaming model call, returning a [`ModelStream`] of provider
    /// events the server pumps for the live ticker.
    ///
    /// # Errors
    ///
    /// A human message when the stream cannot be opened.
    async fn open_stream(&self, request: Value) -> Result<Box<dyn ModelStream>, String>;
}

/// A stream of provider events opened by [`ModelExecutor::open_stream`].
///
/// It mirrors `salvor_llm::MessageStream`: pull one typed event at a time until
/// `None`. The server feeds each event to a [`MessageAccumulator`] so the
/// recorded completion is byte-identical to the non-streaming path.
#[async_trait]
pub trait ModelStream: Send {
    /// The next provider event, or `None` once the stream is exhausted. An
    /// `Err(String)` is a mid-stream failure: the server surfaces it and records
    /// no completion, leaving the intent dangling for a safe re-issue on resume.
    async fn next_event(&mut self) -> Option<Result<StreamEvent, String>>;
}

/// The default [`ModelExecutor`], wrapping a general `salvor_llm::Client`.
///
/// This is not consumer-specific: it wraps the general model transport, so any
/// host that reaches a provider through `salvor-llm` gets a working executor by
/// constructing a [`Client`] and handing it here. `salvor serve` builds one from
/// its environment client-construction path.
pub struct LlmModelExecutor {
    client: Client,
}

impl LlmModelExecutor {
    /// Wraps `client` as a model executor.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ModelExecutor for LlmModelExecutor {
    async fn execute(&self, request: Value) -> Result<MessageResponse, String> {
        // Forward the caller's request bytes verbatim: the hash the server
        // recorded over `request` is the hash it sends.
        self.client
            .send_message_value(&request)
            .await
            .map_err(contextualize_401)
    }

    async fn open_stream(&self, request: Value) -> Result<Box<dyn ModelStream>, String> {
        let stream = self
            .client
            .stream_message_value(&request)
            .await
            .map_err(contextualize_401)?;
        Ok(Box::new(LlmModelStream { inner: stream }))
    }
}

/// The [`ModelStream`] over a `salvor_llm::MessageStream`.
struct LlmModelStream {
    inner: MessageStream,
}

#[async_trait]
impl ModelStream for LlmModelStream {
    async fn next_event(&mut self) -> Option<Result<StreamEvent, String>> {
        self.inner
            .next_event()
            .await
            .map(|event| event.map_err(contextualize_401))
    }
}
