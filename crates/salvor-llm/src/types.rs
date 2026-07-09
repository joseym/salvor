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
    /// An optional system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// The tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Custom sequences that stop generation when produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
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
        }
    }

    /// Set the system prompt.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
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
#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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
