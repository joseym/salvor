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

use async_trait::async_trait;
use salvor_llm::{Client, MessageResponse, MessageStream, StreamEvent};
use serde_json::Value;

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
            .map_err(|error| error.to_string())
    }

    async fn open_stream(&self, request: Value) -> Result<Box<dyn ModelStream>, String> {
        let stream = self
            .client
            .stream_message_value(&request)
            .await
            .map_err(|error| error.to_string())?;
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
            .map(|event| event.map_err(|error| error.to_string()))
    }
}
