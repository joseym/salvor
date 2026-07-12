//! The asynchronous Messages API client.
//!
//! [`Client`] wraps a [`reqwest::Client`] and a [`Config`]. Its one public
//! action, [`Client::send_message`], sends a [`MessageRequest`] and returns a
//! [`MessageResponse`], retrying transient failures with exponential backoff
//! and jitter along the way.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::Error as _;
use serde_json::Value;
use tokio_stream::{Stream, StreamExt};

use crate::config::{AuthKind, Config};
use crate::error::{ApiError, Error};
use crate::types::{
    ContentBlock, ContentDelta, MessageDeltaUsage, MessageRequest, MessageResponse, StreamEvent,
    Usage,
};

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

    /// Open a streaming request and return a reader over its events.
    ///
    /// The request is sent with `stream: true` set internally, so the caller's
    /// [`MessageRequest`] is untouched and the non-streaming path is unaffected.
    /// Only the initial connection is retried, on the same schedule as
    /// [`Client::send_message`]: once the server answers `2xx` and bytes begin
    /// to flow, any later failure is surfaced through [`MessageStream`] rather
    /// than retried, because a partially consumed stream cannot be replayed.
    ///
    /// Drive the returned [`MessageStream`] with [`MessageStream::next_event`]
    /// for a live event feed, or [`MessageStream::get_final_message`] to fold it
    /// into the same [`MessageResponse`] a non-streaming call would have
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Client::send_message`] for the initial
    /// connection: an [`Error::Api`] for a non-2xx response, [`Error::Transport`]
    /// for a network failure, or [`Error::Unexpected`] for an unrecognizable
    /// non-2xx body.
    pub async fn stream_message(&self, request: &MessageRequest) -> Result<MessageStream, Error> {
        let mut streaming = request.clone();
        streaming.stream = true;
        let mut attempt: u32 = 0;
        loop {
            match self.try_stream(&streaming).await {
                Ok(stream) => return Ok(stream),
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
        let response = self
            .post_builder()
            .json(request)
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        let request_id = header_string(response.headers(), "request-id");
        let retry_after = retry_after_of(response.headers());
        let body = response.bytes().await.map_err(Error::Transport)?;

        if status.is_success() {
            return serde_json::from_slice(&body).map_err(Error::Decode);
        }
        Err(error_from_body(
            status.as_u16(),
            request_id,
            retry_after,
            &body,
        ))
    }

    /// Perform a single streaming request attempt, with no retrying.
    async fn try_stream(&self, request: &MessageRequest) -> Result<MessageStream, Error> {
        let response = self
            .post_builder()
            .json(request)
            .send()
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        let request_id = header_string(response.headers(), "request-id");

        if status.is_success() {
            // Convert each chunk to an owned `Vec<u8>` so the boxed stream does
            // not name reqwest's `Bytes`, keeping the dependency surface small.
            let bytes = response
                .bytes_stream()
                .map(|chunk| chunk.map(|b| b.to_vec()));
            return Ok(MessageStream::new(Box::pin(bytes), request_id));
        }

        let retry_after = retry_after_of(response.headers());
        let body = response.bytes().await.map_err(Error::Transport)?;
        Err(error_from_body(
            status.as_u16(),
            request_id,
            retry_after,
            &body,
        ))
    }

    /// A `POST` builder carrying the version header and the auth headers the
    /// configured scheme requires.
    ///
    /// Omit every auth header when no key is configured; local endpoints expect
    /// no key and reject an empty one. When a key is present, the scheme decides
    /// which headers carry it: a standard API key rides `x-api-key`, while an
    /// OAuth token needs `Authorization: Bearer` plus the oauth beta opt-in and
    /// must not send `x-api-key` at all.
    fn post_builder(&self) -> reqwest::RequestBuilder {
        let mut builder = self
            .http
            .post(&self.url)
            .header("anthropic-version", ANTHROPIC_VERSION);
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
        builder
    }
}

/// Parse the `retry-after` header as whole seconds, when present.
fn retry_after_of(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Map a non-2xx response body to a typed error: [`Error::Api`] when the body is
/// a recognizable error envelope, [`Error::Unexpected`] otherwise.
fn error_from_body(
    status: u16,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    body: &[u8],
) -> Error {
    match serde_json::from_slice::<ErrorEnvelope>(body) {
        Ok(envelope) => Error::Api(ApiError {
            status,
            kind: envelope.error.kind,
            message: envelope.error.message,
            request_id,
            retry_after,
        }),
        Err(_) => Error::Unexpected {
            status,
            body: String::from_utf8_lossy(body).into_owned(),
        },
    }
}

/// The boxed byte stream a [`MessageStream`] reads from. Chunks are owned
/// `Vec<u8>` so the type does not have to name reqwest's `Bytes`.
type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Vec<u8>>> + Send>>;

/// A reader over a streaming Messages response.
///
/// Pull typed events one at a time with [`MessageStream::next_event`] (for a
/// live consumer such as a token/cost ticker), or fold the whole stream into a
/// [`MessageResponse`] with [`MessageStream::get_final_message`]. The assembled
/// message is byte-for-byte equal in content and usage to what
/// [`Client::send_message`] would have returned for the same request, so a
/// consumer that records the response is unaffected by which path produced it.
///
/// The reader parses the server-sent-events frames off the byte stream by hand:
/// it buffers bytes, splits complete lines, joins `data:` fields, and decodes
/// one JSON payload per blank-line-terminated frame. Unknown event and delta
/// types are preserved rather than rejected, so the stream is always consumed to
/// the end.
pub struct MessageStream {
    bytes: ByteStream,
    buffer: Vec<u8>,
    data: String,
    finished: bool,
    request_id: Option<String>,
    pending: VecDeque<Result<StreamEvent, Error>>,
}

impl MessageStream {
    /// Wrap a byte stream and the response's request id.
    fn new(bytes: ByteStream, request_id: Option<String>) -> Self {
        Self {
            bytes,
            buffer: Vec::new(),
            data: String::new(),
            finished: false,
            request_id,
            pending: VecDeque::new(),
        }
    }

    /// The `request-id` header of the streaming response, when the server sent
    /// one. Useful when reporting a failure to Anthropic.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Read the next event, or `None` once the stream is exhausted.
    ///
    /// # Errors
    ///
    /// Yields [`Error::Transport`] if the underlying byte stream fails,
    /// [`Error::Decode`] for a frame whose JSON does not parse, or
    /// [`Error::Api`] for an `error` event. Once an error is yielded the stream
    /// is not retried: a partially consumed stream cannot be replayed.
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, Error>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            match self.bytes.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                    self.drain_lines();
                }
                Some(Err(err)) => {
                    self.finished = true;
                    return Some(Err(Error::Transport(err)));
                }
                None => {
                    self.finished = true;
                    self.flush_tail();
                }
            }
        }
    }

    /// Fold the whole stream into the final [`MessageResponse`].
    ///
    /// Consumes the stream, applying every event to a [`MessageAccumulator`]. A
    /// consumer that also wants live events can instead drive
    /// [`MessageStream::next_event`] and feed each event to its own
    /// [`MessageAccumulator`].
    ///
    /// # Errors
    ///
    /// Yields the first stream error seen (see [`MessageStream::next_event`]),
    /// or [`Error::Decode`] if the stream ended before a `message_start` event.
    pub async fn get_final_message(mut self) -> Result<MessageResponse, Error> {
        let mut accumulator = MessageAccumulator::new();
        while let Some(event) = self.next_event().await {
            accumulator.apply(&event?)?;
        }
        accumulator.into_message()
    }

    /// Split every complete line out of the buffer and handle it.
    fn drain_lines(&mut self) {
        while let Some(pos) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let mut line = &line[..line.len() - 1];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            self.handle_line(line);
        }
    }

    /// At end of stream, treat any trailing bytes as a final line and dispatch
    /// any frame that was not terminated by a blank line.
    fn flush_tail(&mut self) {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let mut line = &line[..];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            self.handle_line(line);
        }
        self.dispatch();
    }

    /// Handle one SSE line: a blank line ends a frame, a `:` line is a comment,
    /// a `data:` line appends to the current frame, other fields are ignored.
    fn handle_line(&mut self, line: &[u8]) {
        if line.is_empty() {
            self.dispatch();
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (field, value) = split_field(line);
        if field == b"data" {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(&String::from_utf8_lossy(value));
        }
        // `event:`, `id:`, and `retry:` fields carry nothing we act on: the JSON
        // payload names its own type, so the dispatch reads that instead.
    }

    /// Decode the accumulated frame data into a typed event (or error) and queue
    /// it. A frame with no data is a no-op.
    fn dispatch(&mut self) {
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data);
        let event = match serde_json::from_str::<Value>(&data) {
            Ok(value) => StreamEvent::from_value(value),
            Err(err) => Err(Error::Decode(err)),
        };
        self.pending.push_back(event);
    }
}

/// Split an SSE `field: value` line into its field and value, dropping one
/// optional space after the colon. A line with no colon is a field with an
/// empty value.
fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&byte| byte == b':') {
        Some(index) => {
            let field = &line[..index];
            let mut value = &line[index + 1..];
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            (field, value)
        }
        None => (line, &[]),
    }
}

/// Folds a stream of [`StreamEvent`]s into a [`MessageResponse`].
///
/// [`MessageStream::get_final_message`] uses this internally. It is public so a
/// live consumer can watch events with [`MessageStream::next_event`] and still
/// assemble the final message, feeding each event to [`MessageAccumulator::apply`]
/// and calling [`MessageAccumulator::into_message`] at the end. The result
/// matches what [`Client::send_message`] returns for the same request: text and
/// tool-call input concatenate from their deltas, and usage merges the input
/// count from `message_start` with the output count from `message_delta`.
#[derive(Default)]
pub struct MessageAccumulator {
    message: Option<MessageResponse>,
    blocks: Vec<BlockState>,
}

/// A content block under construction, with the buffer for a tool call's
/// streamed input JSON.
struct BlockState {
    block: ContentBlock,
    partial_json: String,
}

impl BlockState {
    /// Apply one delta to the block in progress. A delta whose kind does not
    /// match the block is ignored, keeping the fold forward-tolerant.
    fn apply_delta(&mut self, delta: &ContentDelta) {
        match delta {
            ContentDelta::Text { text } => {
                if let ContentBlock::Text { text: current } = &mut self.block {
                    current.push_str(text);
                }
            }
            ContentDelta::Thinking { thinking } => {
                if let ContentBlock::Thinking {
                    thinking: current, ..
                } = &mut self.block
                {
                    current.push_str(thinking);
                }
            }
            ContentDelta::Signature { signature } => {
                if let ContentBlock::Thinking {
                    signature: current, ..
                } = &mut self.block
                {
                    match current {
                        Some(existing) => existing.push_str(signature),
                        None => *current = Some(signature.clone()),
                    }
                }
            }
            ContentDelta::InputJson { partial_json } => {
                if matches!(self.block, ContentBlock::ToolUse { .. }) {
                    self.partial_json.push_str(partial_json);
                }
            }
            ContentDelta::Unknown(_) => {}
        }
    }

    /// Parse the buffered tool-call input JSON into the block's input.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the concatenated fragments are not valid
    /// JSON.
    fn finalize(&mut self) -> Result<(), Error> {
        if let ContentBlock::ToolUse { input, .. } = &mut self.block
            && !self.partial_json.is_empty()
        {
            *input = serde_json::from_str(&self.partial_json).map_err(Error::Decode)?;
        }
        Ok(())
    }
}

impl MessageAccumulator {
    /// A fresh accumulator with no events applied.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event to the running message.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if a tool call's streamed input JSON does not
    /// parse when its block stops.
    pub fn apply(&mut self, event: &StreamEvent) -> Result<(), Error> {
        match event {
            StreamEvent::MessageStart(message) => {
                let mut message = message.clone();
                message.content.clear();
                self.message = Some(message);
                self.blocks.clear();
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                // Indices arrive in order, but pad defensively so an out-of-order
                // start never panics.
                while self.blocks.len() <= *index {
                    self.blocks.push(BlockState {
                        block: ContentBlock::text(""),
                        partial_json: String::new(),
                    });
                }
                self.blocks[*index] = BlockState {
                    block: content_block.clone(),
                    partial_json: String::new(),
                };
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(block) = self.blocks.get_mut(*index) {
                    block.apply_delta(delta);
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                if let Some(block) = self.blocks.get_mut(*index) {
                    block.finalize()?;
                }
            }
            StreamEvent::MessageDelta {
                stop_reason,
                stop_sequence,
                usage,
            } => {
                if let Some(message) = self.message.as_mut() {
                    if stop_reason.is_some() {
                        message.stop_reason = stop_reason.clone();
                    }
                    if stop_sequence.is_some() {
                        message.stop_sequence = stop_sequence.clone();
                    }
                    merge_usage(&mut message.usage, usage);
                }
            }
            StreamEvent::MessageStop | StreamEvent::Ping | StreamEvent::Unknown(_) => {}
        }
        Ok(())
    }

    /// Finish the fold and return the assembled message.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if no `message_start` event was applied, since
    /// there is then no message shell to complete.
    pub fn into_message(self) -> Result<MessageResponse, Error> {
        let mut message = self.message.ok_or_else(|| {
            Error::Decode(serde_json::Error::custom(
                "streaming response ended before a message_start event",
            ))
        })?;
        message.content = self.blocks.into_iter().map(|state| state.block).collect();
        Ok(message)
    }
}

/// Merge the streamed usage counts onto the message's [`Usage`]. Present fields
/// overwrite; absent fields leave the existing value.
fn merge_usage(usage: &mut Usage, delta: &MessageDeltaUsage) {
    if let Some(input) = delta.input_tokens {
        usage.input_tokens = input;
    }
    if let Some(output) = delta.output_tokens {
        usage.output_tokens = output;
    }
    if delta.cache_creation_input_tokens.is_some() {
        usage.cache_creation_input_tokens = delta.cache_creation_input_tokens;
    }
    if delta.cache_read_input_tokens.is_some() {
        usage.cache_read_input_tokens = delta.cache_read_input_tokens;
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
