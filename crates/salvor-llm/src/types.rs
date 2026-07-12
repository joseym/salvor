//! Typed models for the Anthropic Messages API wire protocol.
//!
//! These types mirror the JSON the API accepts and returns. Two design rules
//! run through the module:
//!
//! - **Requests stay minimal and additive.** Only the fields v0.1 sends are
//!   present. Optional fields are skipped when unset so the serialized body
//!   never carries a `null` the API might reject.
//! - **Responses tolerate the future.** The API adds content-block types and
//!   stop reasons over time. Unknown content blocks land in
//!   [`ContentBlock::Unknown`] with their raw JSON intact, unknown stop reasons
//!   land in [`StopReason::Other`], and unknown top-level fields are ignored.
//!   Deserializing a response never fails just because the API grew.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::{ApiError, Error};

/// The author of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A message from the user (or carrying tool results back to the model).
    User,
    /// A message from the assistant.
    Assistant,
}

/// A message's content: either a plain string or a list of content blocks.
///
/// The API accepts both forms. A plain-text turn serializes as a bare string;
/// a turn with tool results or multiple blocks serializes as an array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// A single run of text.
    Text(String),
    /// An ordered list of content blocks.
    Blocks(Vec<ContentBlock>),
}

/// One message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Who authored the message.
    pub role: Role,
    /// The message body.
    pub content: Content,
}

impl Message {
    /// A user message carrying a single run of text.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Content::Text(text.into()),
        }
    }

    /// An assistant message carrying a single run of text.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Content::Text(text.into()),
        }
    }

    /// A user message built from explicit content blocks.
    pub fn user_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content: Content::Blocks(blocks),
        }
    }

    /// An assistant message built from explicit content blocks.
    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content: Content::Blocks(blocks),
        }
    }

    /// The user message that answers a tool call, ready to append to the
    /// conversation as the follow-up turn.
    ///
    /// `tool_use_id` must match the `id` of the [`ContentBlock::ToolUse`] block
    /// from the assistant's previous turn. This is the ergonomic path for the
    /// common case where a tool returns a string result.
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::user_blocks(vec![ContentBlock::tool_result(tool_use_id, content)])
    }
}

/// The content of a `tool_result` block: a plain string or nested blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// A single run of text (the common case).
    Text(String),
    /// Nested content blocks.
    Blocks(Vec<ContentBlock>),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        ToolResultContent::Text(String::new())
    }
}

/// The source of an image or document ([`ContentBlock::Image`] /
/// [`ContentBlock::Document`]) block.
///
/// Two forms are modelled directly: inline base64-encoded data (with its media
/// type) and a URL the API fetches. Any other source shape the API accepts (for
/// example a Files API `file_id` reference) is preserved verbatim in
/// [`Source::Unknown`], so it round-trips unchanged. Serialization and
/// deserialization are hand-written for the same reason [`ContentBlock`]'s are:
/// the unknown case must keep its raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Inline base64-encoded bytes with their media type (for example
    /// `image/png` or `application/pdf`).
    Base64 {
        /// The MIME type of the encoded data.
        media_type: String,
        /// The base64-encoded bytes, with no line breaks.
        data: String,
    },
    /// A URL the API fetches the bytes from.
    Url {
        /// The source URL.
        url: String,
    },
    /// A source shape this client does not model. The raw JSON is preserved so
    /// it can be re-serialized unchanged.
    Unknown(Value),
}

impl Serialize for Source {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Source::Base64 { media_type, data } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "base64")?;
                map.serialize_entry("media_type", media_type)?;
                map.serialize_entry("data", data)?;
                map.end()
            }
            Source::Url { url } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "url")?;
                map.serialize_entry("url", url)?;
                map.end()
            }
            // The raw JSON already carries its own `type`; serialize it as-is.
            Source::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Buffer the source as generic JSON, then dispatch on its `type`. An
        // unrecognized type keeps the whole value rather than erroring.
        let value = Value::deserialize(deserializer)?;
        match value.get("type").and_then(Value::as_str) {
            Some("base64") => {
                let raw: RawBase64Source =
                    serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Source::Base64 {
                    media_type: raw.media_type,
                    data: raw.data,
                })
            }
            Some("url") => {
                let raw: RawUrlSource = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Source::Url { url: raw.url })
            }
            _ => Ok(Source::Unknown(value)),
        }
    }
}

#[derive(Deserialize)]
struct RawBase64Source {
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    data: String,
}

#[derive(Deserialize)]
struct RawUrlSource {
    #[serde(default)]
    url: String,
}

/// A single block of message content.
///
/// The four known request/response block types are modelled directly. Any
/// block type the API introduces later is captured verbatim in
/// [`ContentBlock::Unknown`], so deserialization never fails on an unfamiliar
/// block. Serialization and deserialization are hand-written (rather than
/// derived) precisely so the unknown case can round-trip its raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    /// A run of text.
    Text {
        /// The text.
        text: String,
    },
    /// A request from the model to call a tool. Appears in responses.
    ToolUse {
        /// The id used to correlate this call with its result.
        id: String,
        /// The name of the tool to call.
        name: String,
        /// The tool input, as arbitrary JSON matching the tool's schema.
        input: Value,
    },
    /// The result of a tool call, sent back inside a user message.
    ToolResult {
        /// The `id` of the [`ContentBlock::ToolUse`] this answers.
        tool_use_id: String,
        /// The result payload.
        content: ToolResultContent,
        /// Whether the result represents an error, when the field is set.
        is_error: Option<bool>,
    },
    /// A model reasoning block. Appears in responses from thinking-capable
    /// models. Modelled so it round-trips, but the client never sends it.
    Thinking {
        /// The reasoning text (may be empty depending on the display setting).
        thinking: String,
        /// The opaque signature the API attaches to a thinking block.
        signature: Option<String>,
    },
    /// An image, sent inside a user message. The API accepts base64-encoded
    /// bytes or a URL; see [`Source`].
    Image {
        /// Where the image bytes come from.
        source: Source,
    },
    /// A document (for example a PDF), sent inside a user message. The API
    /// accepts base64-encoded bytes or a URL; see [`Source`].
    Document {
        /// Where the document bytes come from.
        source: Source,
    },
    /// A block type this client does not model. The raw JSON is preserved so no
    /// information is lost and the block can be re-serialized unchanged.
    Unknown(Value),
}

impl ContentBlock {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    /// A tool-result block carrying a string result.
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: ToolResultContent::Text(content.into()),
            is_error: None,
        }
    }

    /// A tool-result block flagged as an error, carrying a string message.
    pub fn tool_error(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: ToolResultContent::Text(content.into()),
            is_error: Some(true),
        }
    }

    /// An image block carrying inline base64-encoded bytes.
    ///
    /// `media_type` is the image's MIME type (for example `image/png` or
    /// `image/jpeg`); `data` is the base64 encoding with no line breaks.
    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentBlock::Image {
            source: Source::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    /// An image block referencing a URL the API fetches.
    pub fn image_url(url: impl Into<String>) -> Self {
        ContentBlock::Image {
            source: Source::Url { url: url.into() },
        }
    }

    /// A document block carrying inline base64-encoded bytes.
    ///
    /// `media_type` is the document's MIME type (for example
    /// `application/pdf`); `data` is the base64 encoding with no line breaks.
    pub fn document_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentBlock::Document {
            source: Source::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }

    /// A document block carrying a base64-encoded PDF.
    ///
    /// A convenience over [`ContentBlock::document_base64`] that fixes the media
    /// type to `application/pdf`.
    pub fn document_pdf(data: impl Into<String>) -> Self {
        ContentBlock::document_base64("application/pdf", data)
    }

    /// A document block referencing a URL the API fetches.
    pub fn document_url(url: impl Into<String>) -> Self {
        ContentBlock::Document {
            source: Source::Url { url: url.into() },
        }
    }

    /// The text, if this is a [`ContentBlock::Text`] block.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    /// The `(id, name, input)` of a tool call, if this is a
    /// [`ContentBlock::ToolUse`] block.
    #[must_use]
    pub fn as_tool_use(&self) -> Option<(&str, &str, &Value)> {
        match self {
            ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
            _ => None,
        }
    }
}

// Helper structs used only while deserializing a known block type from its
// JSON object. Keeping them private keeps the public surface to `ContentBlock`.
#[derive(Deserialize)]
struct RawText {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RawToolUse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    input: Value,
}

#[derive(Deserialize)]
struct RawToolResult {
    #[serde(default)]
    tool_use_id: String,
    #[serde(default)]
    content: ToolResultContent,
    #[serde(default)]
    is_error: Option<bool>,
}

#[derive(Deserialize)]
struct RawThinking {
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    signature: Option<String>,
}

// Image and Document blocks share one shape: a single `source`. Its own
// hand-written `Deserialize` (with an `Unknown` fallback) does the dispatch.
#[derive(Deserialize)]
struct RawSourced {
    source: Source,
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            ContentBlock::Text { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            ContentBlock::ToolUse { id, name, input } => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", "tool_use")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("input", input)?;
                map.end()
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let len = if is_error.is_some() { 4 } else { 3 };
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", "tool_result")?;
                map.serialize_entry("tool_use_id", tool_use_id)?;
                map.serialize_entry("content", content)?;
                if let Some(is_error) = is_error {
                    map.serialize_entry("is_error", is_error)?;
                }
                map.end()
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                let len = if signature.is_some() { 3 } else { 2 };
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", "thinking")?;
                map.serialize_entry("thinking", thinking)?;
                if let Some(signature) = signature {
                    map.serialize_entry("signature", signature)?;
                }
                map.end()
            }
            ContentBlock::Image { source } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "image")?;
                map.serialize_entry("source", source)?;
                map.end()
            }
            ContentBlock::Document { source } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "document")?;
                map.serialize_entry("source", source)?;
                map.end()
            }
            // The raw JSON already carries its own `type`; serialize it as-is.
            ContentBlock::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Buffer the block as generic JSON first, then dispatch on its `type`.
        // An unrecognized type keeps the whole value rather than erroring.
        let value = Value::deserialize(deserializer)?;
        match value.get("type").and_then(Value::as_str) {
            Some("text") => {
                let raw: RawText = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ContentBlock::Text { text: raw.text })
            }
            Some("tool_use") => {
                let raw: RawToolUse = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ContentBlock::ToolUse {
                    id: raw.id,
                    name: raw.name,
                    input: raw.input,
                })
            }
            Some("tool_result") => {
                let raw: RawToolResult = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ContentBlock::ToolResult {
                    tool_use_id: raw.tool_use_id,
                    content: raw.content,
                    is_error: raw.is_error,
                })
            }
            Some("thinking") => {
                let raw: RawThinking = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(ContentBlock::Thinking {
                    thinking: raw.thinking,
                    signature: raw.signature,
                })
            }
            // An image or document without a well-formed `source` falls back to
            // Unknown rather than erroring, keeping the block round-trippable.
            Some("image") => match serde_json::from_value::<RawSourced>(value.clone()) {
                Ok(raw) => Ok(ContentBlock::Image { source: raw.source }),
                Err(_) => Ok(ContentBlock::Unknown(value)),
            },
            Some("document") => match serde_json::from_value::<RawSourced>(value.clone()) {
                Ok(raw) => Ok(ContentBlock::Document { source: raw.source }),
                Err(_) => Ok(ContentBlock::Unknown(value)),
            },
            _ => Ok(ContentBlock::Unknown(value)),
        }
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// The tool name the model uses to invoke it.
    pub name: String,
    /// A description of when and how to use the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON Schema object describing the tool's input.
    pub input_schema: Value,
}

impl Tool {
    /// A tool with a name, description, and input schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
        }
    }
}

/// A prompt-cache marker for a [`SystemBlock`].
///
/// The Anthropic API caches the request prefix up to and including a block that
/// carries `cache_control`. Only the `ephemeral` cache type is modelled; the
/// optional [`ttl`](CacheControl::ttl) selects a non-default lifetime (for
/// example `"1h"`). Serialization is hand-written so the `type` discriminant is
/// always emitted; deserialization ignores that discriminant and any other
/// field it does not model, keeping the type forward-tolerant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CacheControl {
    /// The cache lifetime, when a non-default one is requested (for example
    /// `"1h"`). Left unset for the default (five-minute) cache, in which case it
    /// is omitted from the serialized block.
    #[serde(default)]
    pub ttl: Option<String>,
}

impl CacheControl {
    /// An ephemeral cache marker with the default (five-minute) lifetime.
    #[must_use]
    pub fn ephemeral() -> Self {
        Self { ttl: None }
    }

    /// An ephemeral cache marker with an explicit time-to-live, for example
    /// `"1h"`.
    pub fn ephemeral_ttl(ttl: impl Into<String>) -> Self {
        Self {
            ttl: Some(ttl.into()),
        }
    }
}

impl Serialize for CacheControl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let len = if self.ttl.is_some() { 2 } else { 1 };
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("type", "ephemeral")?;
        if let Some(ttl) = &self.ttl {
            map.serialize_entry("ttl", ttl)?;
        }
        map.end()
    }
}

/// One text block in a structured system prompt.
///
/// Serializes to the Anthropic API shape `{"type":"text","text":"..."}`, plus a
/// `cache_control` entry when one is set. Only the text block is modelled, which
/// mirrors [`ContentBlock::Text`] and matches the block the API documents for a
/// system prompt; any extra field on deserialization is ignored rather than
/// rejected, so a block that grows new fields still parses.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemBlock {
    /// The block text.
    pub text: String,
    /// An optional prompt-cache marker. When set, the API caches the request
    /// prefix up to and including this block.
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    /// A system text block with no cache marker.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: None,
        }
    }

    /// Attach a cache-control marker to this block.
    #[must_use]
    pub fn with_cache_control(mut self, cache_control: CacheControl) -> Self {
        self.cache_control = Some(cache_control);
        self
    }
}

// Helper struct used only while deserializing a `SystemBlock` from its JSON
// object. The `type` discriminant is not read back (it is always `"text"`); any
// unmodelled field is ignored, keeping the block forward-tolerant.
#[derive(Deserialize)]
struct RawSystemBlock {
    #[serde(default)]
    text: String,
    #[serde(default)]
    cache_control: Option<CacheControl>,
}

impl Serialize for SystemBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let len = if self.cache_control.is_some() { 3 } else { 2 };
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("type", "text")?;
        map.serialize_entry("text", &self.text)?;
        if let Some(cache_control) = &self.cache_control {
            map.serialize_entry("cache_control", cache_control)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SystemBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawSystemBlock::deserialize(deserializer)?;
        Ok(SystemBlock {
            text: raw.text,
            cache_control: raw.cache_control,
        })
    }
}

/// A system prompt: either a plain string or a list of text blocks.
///
/// The Anthropic API accepts both forms. The string form is the common case and
/// serializes to a **bare JSON string**, byte-for-byte what this field carried
/// when it was an `Option<String>`, so a recorded request keeps the same durable
/// hash. The block form serializes to the JSON array the API expects, which lets
/// a caller lead with a distinct first block. That leading block is the one
/// parity gap the string form cannot express: on the OAuth plan-token path the
/// system prompt must open with the Claude-Code identity as its own block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum System {
    /// A single run of system text. Serializes as a bare string.
    Text(String),
    /// An ordered list of system text blocks. Serializes as a JSON array.
    Blocks(Vec<SystemBlock>),
}

impl System {
    /// A system prompt built from explicit text blocks.
    pub fn blocks(blocks: impl IntoIterator<Item = SystemBlock>) -> Self {
        System::Blocks(blocks.into_iter().collect())
    }

    /// A system prompt whose first block is distinct from the rest.
    ///
    /// The `identity` block leads and the `rest` blocks follow in order. This is
    /// the shape the OAuth plan-token path needs: a single string cannot put the
    /// Claude-Code identity in its own leading block, but this can.
    pub fn leading(identity: SystemBlock, rest: impl IntoIterator<Item = SystemBlock>) -> Self {
        let mut blocks = vec![identity];
        blocks.extend(rest);
        System::Blocks(blocks)
    }
}

impl From<String> for System {
    fn from(text: String) -> Self {
        System::Text(text)
    }
}

impl From<&str> for System {
    fn from(text: &str) -> Self {
        System::Text(text.to_owned())
    }
}

/// A request to the Messages API.
///
/// Build one with [`MessageRequest::new`] and add optional pieces with the
/// builder methods. Only the fields v0.1 sends exist here; the struct is kept
/// small on purpose so growth is additive.
#[derive(Debug, Clone, Serialize)]
pub struct MessageRequest {
    /// The model id, for example `claude-opus-4-8`.
    pub model: String,
    /// The maximum number of tokens to generate.
    pub max_tokens: u32,
    /// An optional system prompt, as a plain string or a list of text blocks.
    /// A string serializes to a bare JSON string, identical on the wire to when
    /// this field was an `Option<String>`; see [`System`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<System>,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// The tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Custom sequences that stop generation when produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Whether to ask for a streaming (server-sent-events) response. Defaults to
    /// `false` and is omitted from the serialized body when false, so a
    /// non-streaming request is byte-for-byte what it was before this field
    /// existed. [`crate::Client::stream_message`] sets it internally; callers of
    /// [`crate::Client::send_message`] never touch it.
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
}

/// Whether a bool is false, for `skip_serializing_if`.
fn is_false(value: &bool) -> bool {
    !*value
}

impl MessageRequest {
    /// A request for `model` with a token cap and no messages yet.
    pub fn new(model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            system: None,
            messages: Vec::new(),
            tools: None,
            stop_sequences: None,
            stream: false,
        }
    }

    /// Set the system prompt.
    ///
    /// Accepts anything that converts into a [`System`]: a `String` or `&str`
    /// (the common case) becomes a plain-string system prompt, and a built
    /// [`System::Blocks`] passes through unchanged. Every existing string call
    /// site therefore compiles without change.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<System>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Replace the message list.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    /// Append one message to the conversation.
    #[must_use]
    pub fn push_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Set the tools the model may call.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set the custom stop sequences.
    #[must_use]
    pub fn with_stop_sequences(mut self, stop_sequences: Vec<String>) -> Self {
        self.stop_sequences = Some(stop_sequences);
        self
    }

    /// Set the streaming flag.
    ///
    /// Callers rarely need this: [`crate::Client::stream_message`] sets it and
    /// [`crate::Client::send_message`] leaves it off. It exists so the flag can
    /// be inspected or forced when building a request by hand.
    #[must_use]
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}

/// Why the model stopped generating.
///
/// Known reasons are modelled as their own variants. Any value the API adds
/// later is preserved in [`StopReason::Other`], so a new reason never breaks
/// deserialization. The custom `Serialize`/`Deserialize` impls are what let the
/// unknown string survive a round trip while the enum stays flat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished its turn naturally.
    EndTurn,
    /// Generation hit the `max_tokens` cap.
    MaxTokens,
    /// Generation hit a custom stop sequence.
    StopSequence,
    /// The model wants to call a tool.
    ToolUse,
    /// The turn paused and can be resumed.
    PauseTurn,
    /// The model refused for safety reasons.
    Refusal,
    /// A stop reason this client does not model, kept verbatim.
    Other(String),
}

impl StopReason {
    /// The wire string for this stop reason.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::Other(other) => other,
        }
    }
}

impl Serialize for StopReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "tool_use" => StopReason::ToolUse,
            "pause_turn" => StopReason::PauseTurn,
            "refusal" => StopReason::Refusal,
            _ => StopReason::Other(value),
        })
    }
}

/// Token usage reported with a response.
///
/// The Salvor runtime's budget enforcement consumes these numbers, so the type
/// is public and central. The two cache fields are optional because not every
/// response reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    /// Tokens counted against the input.
    pub input_tokens: u64,
    /// Tokens generated in the output.
    pub output_tokens: u64,
    /// Tokens written to the prompt cache, when reported.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens served from the prompt cache, when reported.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

/// A response from the Messages API.
///
/// Unknown top-level fields are ignored during deserialization, so a response
/// with fields this client does not know about still parses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageResponse {
    /// The response id.
    pub id: String,
    /// The model that produced the response.
    pub model: String,
    /// The author role, always the assistant for a response.
    pub role: Role,
    /// The generated content blocks.
    pub content: Vec<ContentBlock>,
    /// Why generation stopped. Optional to tolerate responses that omit it.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    /// The stop sequence that ended generation, when one did.
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// Token usage for the request and response.
    pub usage: Usage,
}

impl MessageResponse {
    /// The concatenation of every text block in the response.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect()
    }

    /// The `(id, name, input)` of every tool call in the response, in order.
    #[must_use]
    pub fn tool_uses(&self) -> Vec<(&str, &str, &Value)> {
        self.content
            .iter()
            .filter_map(ContentBlock::as_tool_use)
            .collect()
    }
}

/// Token usage reported by a streaming response.
///
/// The streaming protocol splits usage across events: `message_start` reports
/// the input tokens (with `output_tokens` still near zero), and `message_delta`
/// reports the final `output_tokens`. Every field is optional because each
/// event carries only the counts it knows. The accumulator merges these onto a
/// [`Usage`] so the assembled [`MessageResponse`] matches the non-streaming one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MessageDeltaUsage {
    /// Tokens counted against the input, when the event reports it.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Tokens generated in the output, when the event reports it.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Tokens written to the prompt cache, when reported.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens served from the prompt cache, when reported.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

/// One incremental update to a content block, carried by a
/// `content_block_delta` event.
///
/// The four known delta types map to the four block kinds that stream: text,
/// tool-call input JSON, thinking, and thinking signatures. An unfamiliar delta
/// type lands in [`ContentDelta::Unknown`], so a new one never breaks the
/// stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentDelta {
    /// A run of text appended to a [`ContentBlock::Text`] block.
    Text {
        /// The text fragment.
        text: String,
    },
    /// A fragment of the JSON input for a [`ContentBlock::ToolUse`] block. The
    /// fragments concatenate into the complete input, parsed when the block
    /// stops.
    InputJson {
        /// The partial JSON fragment.
        partial_json: String,
    },
    /// A run of reasoning text appended to a [`ContentBlock::Thinking`] block.
    Thinking {
        /// The thinking fragment.
        thinking: String,
    },
    /// A signature fragment for a [`ContentBlock::Thinking`] block.
    Signature {
        /// The signature fragment.
        signature: String,
    },
    /// A delta type this client does not model, kept verbatim.
    Unknown(Value),
}

impl ContentDelta {
    /// Parse the `delta` object of a `content_block_delta` event.
    fn from_value(value: Value) -> Self {
        match value.get("type").and_then(Value::as_str) {
            Some("text_delta") => ContentDelta::Text {
                text: string_field(&value, "text"),
            },
            Some("input_json_delta") => ContentDelta::InputJson {
                partial_json: string_field(&value, "partial_json"),
            },
            Some("thinking_delta") => ContentDelta::Thinking {
                thinking: string_field(&value, "thinking"),
            },
            Some("signature_delta") => ContentDelta::Signature {
                signature: string_field(&value, "signature"),
            },
            _ => ContentDelta::Unknown(value),
        }
    }
}

/// One typed server-sent event from a streaming Messages response.
///
/// The variants mirror the API's event names. Unknown event types land in
/// [`StreamEvent::Unknown`] and unknown delta types in
/// [`ContentDelta::Unknown`], so a stream that grows new event or delta kinds is
/// still consumed to the end. An `error` event does not appear here: it is
/// surfaced as an [`Error`] by the stream reader instead (see
/// [`StreamEvent::from_value`]).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Opens the message. Carries the response shell (id, model, role, input
    /// usage) with empty content.
    MessageStart(MessageResponse),
    /// A new content block begins at `index`.
    ContentBlockStart {
        /// The position of the block in the content list.
        index: usize,
        /// The block in its initial (usually empty) state.
        content_block: ContentBlock,
    },
    /// An incremental update to the block at `index`.
    ContentBlockDelta {
        /// The position of the block being updated.
        index: usize,
        /// The update to apply.
        delta: ContentDelta,
    },
    /// The block at `index` is complete.
    ContentBlockStop {
        /// The position of the block that finished.
        index: usize,
    },
    /// Top-level updates near the end of the message: the stop reason, the stop
    /// sequence, and the final output usage.
    MessageDelta {
        /// Why generation stopped, when the event reports it.
        stop_reason: Option<StopReason>,
        /// The stop sequence that ended generation, when one did.
        stop_sequence: Option<String>,
        /// The final usage counts for the message.
        usage: MessageDeltaUsage,
    },
    /// The message is complete.
    MessageStop,
    /// A keep-alive with no payload.
    Ping,
    /// An event type this client does not model, kept verbatim.
    Unknown(Value),
}

impl StreamEvent {
    /// Parse one SSE `data:` payload (already decoded as JSON) into a typed
    /// event.
    ///
    /// An `error` event maps to an [`Error`] rather than a variant: a
    /// mid-stream error is a failure to surface, not data to accumulate. It
    /// becomes an [`Error::Api`] carrying the event's error type and message,
    /// with status `200` since it arrived on a successful stream, so
    /// [`Error::is_retryable`] reports false and the reader surfaces it rather
    /// than retrying.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if a core event (`message_start`,
    /// `content_block_start`) carries a malformed payload, or [`Error::Api`] for
    /// an `error` event.
    pub(crate) fn from_value(value: Value) -> Result<Self, Error> {
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = value.get("message").cloned().unwrap_or(Value::Null);
                let message: MessageResponse =
                    serde_json::from_value(message).map_err(Error::Decode)?;
                Ok(StreamEvent::MessageStart(message))
            }
            Some("content_block_start") => {
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                let content_block: ContentBlock =
                    serde_json::from_value(block).map_err(Error::Decode)?;
                Ok(StreamEvent::ContentBlockStart {
                    index: index_of(&value),
                    content_block,
                })
            }
            Some("content_block_delta") => Ok(StreamEvent::ContentBlockDelta {
                index: index_of(&value),
                delta: ContentDelta::from_value(value.get("delta").cloned().unwrap_or(Value::Null)),
            }),
            Some("content_block_stop") => Ok(StreamEvent::ContentBlockStop {
                index: index_of(&value),
            }),
            Some("message_delta") => {
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                let stop_reason = delta
                    .get("stop_reason")
                    .filter(|reason| !reason.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(Error::Decode)?;
                let stop_sequence = delta
                    .get("stop_sequence")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let usage = value
                    .get("usage")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(Error::Decode)?
                    .unwrap_or_default();
                Ok(StreamEvent::MessageDelta {
                    stop_reason,
                    stop_sequence,
                    usage,
                })
            }
            Some("message_stop") => Ok(StreamEvent::MessageStop),
            Some("ping") => Ok(StreamEvent::Ping),
            Some("error") => Err(stream_error(&value)),
            _ => Ok(StreamEvent::Unknown(value)),
        }
    }
}

/// Read a string field, defaulting to empty when absent or not a string.
fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Read the `index` field of a content-block event, defaulting to zero.
fn index_of(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(0)
}

/// Build the crate error for an `error` SSE event.
fn stream_error(value: &Value) -> Error {
    let error = value.get("error");
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Error::Api(ApiError {
        status: 200,
        kind,
        message,
        request_id: None,
        retry_after: None,
    })
}

#[cfg(test)]
mod system_tests {
    use super::{CacheControl, Message, MessageRequest, System, SystemBlock};
    use serde_json::json;

    // Acceptance criterion 1 & 4: a plain-string system serializes to a BARE
    // STRING, byte-for-byte what the field carried as `Option<String>`. The
    // runtime hashes `serde_json::to_value(request)`, so identical bytes here
    // mean an identical durable replay hash. This pins the exact wire form.
    #[test]
    fn string_system_serializes_byte_identically() {
        let request = MessageRequest::new("claude-opus-4-8", 16).with_system("You are terse.");
        let wire = serde_json::to_string(&request).expect("serializes");
        assert_eq!(
            wire,
            r#"{"model":"claude-opus-4-8","max_tokens":16,"system":"You are terse.","messages":[]}"#
        );
    }

    // The hash input is the serialized Value tree. A string system must land as
    // a bare `Value::String`, exactly the node the old `Option<String>` produced,
    // so the hash cannot change.
    #[test]
    fn string_system_is_a_bare_json_string_value() {
        let request = MessageRequest::new("claude-opus-4-8", 16)
            .with_system("hi")
            .push_message(Message::user("Hello"));
        let value = serde_json::to_value(&request).expect("to_value");
        assert_eq!(value["system"], json!("hi"));
    }

    // Acceptance criterion 3: a `String` call site also compiles and produces the
    // same bare-string wire form (proving `Into<System>` covers owned strings).
    #[test]
    fn owned_string_call_site_is_bare_string() {
        let request = MessageRequest::new("claude-opus-4-8", 16).with_system(String::from("owned"));
        let value = serde_json::to_value(&request).expect("to_value");
        assert_eq!(value["system"], json!("owned"));
    }

    // Acceptance criterion 2: a block-array system serializes to the Anthropic
    // API shape, and round-trips. Source for the shape: the claude-api skill
    // (Prompt Caching sections), which documents system as
    // `[{"type":"text","text":"..."}]`.
    #[test]
    fn blocks_system_matches_api_shape_and_round_trips() {
        let system = System::blocks([SystemBlock::text("first"), SystemBlock::text("second")]);
        let value = serde_json::to_value(&system).expect("to_value");
        assert_eq!(
            value,
            json!([
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" },
            ])
        );
        let back: System = serde_json::from_value(value).expect("round-trips");
        assert_eq!(back, system);
    }

    // Acceptance criterion 2: a caller can build a system whose FIRST block is a
    // distinct text block, which is the OAuth identity-leading use case.
    #[test]
    fn leading_block_is_the_distinct_first_block() {
        let system = System::leading(
            SystemBlock::text("You are Claude Code."),
            [SystemBlock::text("Follow the user's instructions.")],
        );
        let value = serde_json::to_value(&system).expect("to_value");
        assert_eq!(value[0]["text"], json!("You are Claude Code."));
        assert_eq!(value[1]["text"], json!("Follow the user's instructions."));
    }

    // The cache-control marker is emitted on the API shape when set, and omitted
    // otherwise (so an identity-only block stays a bare text block).
    #[test]
    fn cache_control_serializes_and_is_optional() {
        let plain = serde_json::to_value(SystemBlock::text("x")).expect("to_value");
        assert_eq!(plain, json!({ "type": "text", "text": "x" }));

        let cached = SystemBlock::text("x").with_cache_control(CacheControl::ephemeral());
        assert_eq!(
            serde_json::to_value(&cached).expect("to_value"),
            json!({ "type": "text", "text": "x", "cache_control": { "type": "ephemeral" } })
        );

        let ttl = SystemBlock::text("x").with_cache_control(CacheControl::ephemeral_ttl("1h"));
        assert_eq!(
            serde_json::to_value(&ttl).expect("to_value"),
            json!({
                "type": "text",
                "text": "x",
                "cache_control": { "type": "ephemeral", "ttl": "1h" }
            })
        );
    }

    // Acceptance criterion 5: an unknown field inside a system block does not
    // break deserialization.
    #[test]
    fn unknown_field_in_block_is_ignored() {
        let block: SystemBlock =
            serde_json::from_value(json!({ "type": "text", "text": "x", "future": 42 }))
                .expect("tolerates unknown field");
        assert_eq!(block.text, "x");
        assert_eq!(block.cache_control, None);
    }
}
