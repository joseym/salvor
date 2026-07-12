//! The response-and-usage conversion a recorded model call writes.
//!
//! A [`ModelCallCompleted`](salvor_core::Event::ModelCallCompleted) event
//! carries two derived values: the response as a JSON [`Value`] and the
//! [`TokenUsage`] counted for the call. [`RunCtx`](crate::RunCtx) computes both
//! at the live IO edge, and the durable log's byte form depends on exactly how
//! it does so, so the conversion lives here as small public functions rather
//! than inline in `ctx.rs`.
//!
//! # Why it is its own module
//!
//! The server-performed model step (`salvor-server`'s client-driven runs) has
//! to record the *same* completion `RunCtx::model_call` records, because a run
//! must replay identically whether the runtime drove the model call natively or
//! the server performed it over the wire. Sharing this conversion, rather than
//! reimplementing it in the server, is what keeps the two paths byte-identical.
//! The functions are pure and side-effect-free: they read a
//! [`MessageResponse`] and return a value, touching no store, clock, or
//! provider.

use salvor_core::TokenUsage;
use salvor_llm::MessageResponse;
use serde_json::{Value, json};

/// Rebuilds the wire JSON of a response so the recorded value deserializes back
/// into an equal [`MessageResponse`]. Built by hand because the response type
/// is deserialize-only in `salvor-llm`.
///
/// This is the exact `response` field a `ModelCallCompleted` carries.
#[must_use]
pub fn response_value(response: &MessageResponse) -> Value {
    json!({
        "id": response.id,
        "model": response.model,
        "role": response.role,
        "content": response.content,
        "stop_reason": response.stop_reason,
        "stop_sequence": response.stop_sequence,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
            "cache_creation_input_tokens": response.usage.cache_creation_input_tokens,
            "cache_read_input_tokens": response.usage.cache_read_input_tokens,
        },
    })
}

/// Narrows a provider-reported token count to the event log's `u32`,
/// saturating rather than failing on a count that cannot occur in practice.
#[must_use]
pub fn clamp_tokens(count: u64) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The [`TokenUsage`] a completion records for `response`: its input and output
/// counts, each narrowed with [`clamp_tokens`]. This is the exact `usage` field
/// a `ModelCallCompleted` carries.
#[must_use]
pub fn usage_of(response: &MessageResponse) -> TokenUsage {
    TokenUsage {
        input_tokens: clamp_tokens(response.usage.input_tokens),
        output_tokens: clamp_tokens(response.usage.output_tokens),
    }
}
