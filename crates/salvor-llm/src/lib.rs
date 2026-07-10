//! Salvor LLM: an asynchronous Rust client for the Anthropic Messages API.
//!
//! This crate is the shared Messages API client for the Salvor agent runtime.
//! It was extracted from cargo-mentor's provider module (`mentor-provider`);
//! cargo-mentor will consume this crate in turn once it is published. The
//! crate holds no product logic from either project: only the typed wire
//! protocol, an async [`Client`], retry behaviour, and errors.
//!
//! # What it does
//!
//! [`Client::send_message`] sends a [`MessageRequest`] to `POST
//! {base_url}/v1/messages` and returns a typed [`MessageResponse`]. The client
//! is built from a [`Config`] that sets the base URL, an optional API key, a
//! retry cap, and a timeout. Retryable failures (`429`, `500`, `529`, and
//! transport errors) are retried with exponential backoff and jitter, honouring
//! a `retry-after` header when the server sends one.
//!
//! # Role in Salvor
//!
//! The agent loop calls this client for every model turn, and
//! the runtime's budget enforcement consumes the [`Usage`] reported with each
//! response, which is why [`Usage`] is public and central. Tool use is
//! first-class because the loop's tool dispatch reads
//! [`ContentBlock::ToolUse`] blocks and answers them with
//! [`Message::tool_result`].
//!
//! # Authentication
//!
//! Two schemes are supported, selected by [`Config::auth_kind`]. The default
//! [`AuthKind::ApiKey`] sends a standard API key (`sk-ant-api...`) as the
//! `x-api-key` header. [`AuthKind::Bearer`] sends the key as `Authorization:
//! Bearer <key>` and adds `anthropic-beta: oauth-2025-04-20`, which is what an
//! Anthropic subscription OAuth token (`sk-ant-oat...`, minted by `ant auth`)
//! requires: such a token is rejected on `x-api-key` and accepted only under
//! the bearer scheme. In bearer mode `x-api-key` is never sent. When
//! [`Config::api_key`] is `None`, no auth header is sent under either scheme.
//!
//! # Local endpoints
//!
//! LM Studio (0.4.1+) and Ollama (0.14+) speak the same Messages wire protocol.
//! Point [`Config::base_url`] at the local server and leave [`Config::api_key`]
//! unset; the client omits the `x-api-key` header when no key is configured.
//! Talking to a local model is a configuration change, not a code path.
//!
//! # Forward compatibility
//!
//! Responses are deserialized tolerantly. Unknown content-block types are kept
//! verbatim in [`ContentBlock::Unknown`], unknown stop reasons in
//! [`StopReason::Other`], and unknown top-level fields are ignored. A response
//! that grows new fields never fails to parse.
//!
//! # Not yet included
//!
//! Streaming responses are deliberately out of scope for v0.1. The request
//! struct is kept minimal: tuning parameters such as `thinking` and
//! `temperature` are not sent, because current Claude models reject several of
//! them and the defaults are correct. Both are expected to arrive in a later
//! version, additively.
//!
//! # Example
//!
//! ```no_run
//! use salvor_llm::{Client, Config, Message, MessageRequest};
//!
//! # async fn run() -> Result<(), salvor_llm::Error> {
//! let client = Client::new(Config::from_env())?;
//! let request = MessageRequest::new("claude-opus-4-8", 1024)
//!     .push_message(Message::user("Explain the `?` operator in Rust."));
//! let response = client.send_message(&request).await?;
//! println!("{}", response.text());
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod client;
mod config;
mod error;
mod types;

pub use client::Client;
pub use config::{AuthKind, Config};
pub use error::{ApiError, Error};
pub use types::{
    Content, ContentBlock, Message, MessageRequest, MessageResponse, Role, StopReason, Tool,
    ToolResultContent, Usage,
};
