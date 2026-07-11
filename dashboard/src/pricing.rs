//! The dollar-cost estimate layer for the inspector.
//!
//! # Why this module exists (tracker gap #2: cost is derived, not stored)
//!
//! A run's log records token counts, never dollars. Token totals are therefore
//! EXACT: they come straight from the real [`derive_state`](crate::replay::derive_state)
//! fold. Dollars are an ESTIMATE this module derives by multiplying those tokens
//! by a small, hand-maintained price table keyed by model id.
//!
//! The price table below is the assumed pricing source. It is a snapshot, not a
//! live feed: prices change, and a run may have used a model that is not in the
//! table. So every dollar figure the UI shows is labelled an estimate, and when
//! the model behind a completed call cannot be identified or priced, the UI
//! degrades to tokens-only rather than inventing a number.
//!
//! # Where the model id comes from
//!
//! The log's [`Event::ModelCallCompleted`](crate::replay::Event::ModelCallCompleted)
//! stores the provider's response verbatim as JSON. The providers this targets
//! (the Anthropic Messages API shape) put the model id in a top-level `"model"`
//! string on that response, so [`model_id_of_response`] reads it from there.
//! The request-correlation `request_hash` is not the model id, and the token
//! counts carry no model, so the response body is the only place the id lives.

use serde_json::Value;

use crate::replay::{Event, EventEnvelope, TokenUsage};

/// One model's per-million-token prices, in US dollars. Both an ESTIMATE input:
/// the numbers are a maintained snapshot, not fetched from the provider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPrice {
    /// USD per one million input (prompt) tokens.
    pub input_per_mtok: f64,
    /// USD per one million output (completion) tokens.
    pub output_per_mtok: f64,
}

/// The assumed pricing source: model id, input $/Mtok, output $/Mtok.
///
/// A deliberately small, current-as-of table (Anthropic list prices, 2026-06).
/// It is not exhaustive and is not a live feed; treat everything derived from it
/// as an estimate. Extend it as models are added. Matching is exact first, then
/// a prefix match so a dated snapshot id (`claude-haiku-4-5-20251001`) still
/// resolves to its base entry.
const PRICE_TABLE: &[(&str, f64, f64)] = &[
    ("claude-opus-4-8", 5.00, 25.00),
    ("claude-opus-4-7", 5.00, 25.00),
    ("claude-opus-4-6", 5.00, 25.00),
    ("claude-sonnet-5", 3.00, 15.00),
    ("claude-sonnet-4-6", 3.00, 15.00),
    ("claude-haiku-4-5", 1.00, 5.00),
    ("claude-fable-5", 10.00, 50.00),
];

/// Looks a model id up in [`PRICE_TABLE`], returning `None` when it is unknown.
///
/// Exact match wins; otherwise a table id that is a prefix of `model_id` matches
/// (so a dated snapshot resolves to its base model). `None` is the honest
/// answer for a model this build does not price, and it drives the tokens-only
/// degradation upstream.
#[must_use]
pub fn price_for(model_id: &str) -> Option<ModelPrice> {
    PRICE_TABLE
        .iter()
        .find(|(id, _, _)| model_id == *id || model_id.starts_with(id))
        .map(|(_, input, output)| ModelPrice {
            input_per_mtok: *input,
            output_per_mtok: *output,
        })
}

/// Reads the provider's model id from a completion response, if present.
///
/// The Anthropic Messages API response carries the model id as a top-level
/// `"model"` string. Anything else (a response with no `model`, or a non-string
/// there) yields `None`, which the caller reports as an unknown model.
#[must_use]
pub fn model_id_of_response(response: &Value) -> Option<&str> {
    response.get("model").and_then(Value::as_str)
}

/// The estimated dollar cost of one model call, or `None` when the model is
/// unknown (no id on the response, or an id absent from the price table).
#[must_use]
pub fn call_cost_usd(model_id: Option<&str>, usage: TokenUsage) -> Option<f64> {
    let price = price_for(model_id?)?;
    Some(cost(usage, &price))
}

/// Multiplies exact token counts by a price. The token counts are exact; the
/// product is an estimate because the price is.
fn cost(usage: TokenUsage, price: &ModelPrice) -> f64 {
    (f64::from(usage.input_tokens) / 1_000_000.0) * price.input_per_mtok
        + (f64::from(usage.output_tokens) / 1_000_000.0) * price.output_per_mtok
}

/// The running cost estimate across a log, plus the counts that decide whether
/// the estimate can be shown at all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CostSummary {
    /// Summed estimated dollars across every priced completed model call.
    pub usd: f64,
    /// Completed model calls whose model was in the price table.
    pub priced_call_count: u32,
    /// Completed model calls whose model could not be identified or priced.
    pub unpriced_call_count: u32,
}

impl CostSummary {
    /// Whether a dollar figure is safe to show.
    ///
    /// True when no completed call went unpriced. A log with zero model calls
    /// is available at `$0.00` (nothing was spent). A single unpriced call
    /// makes the total incomplete, so the UI shows tokens only instead of a
    /// number that silently omits part of the spend.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.unpriced_call_count == 0
    }
}

/// Walks a log and estimates its total cost.
///
/// Token totals for display come from the real fold, not from here; this walk
/// exists only because cost needs the per-call model id (which the fold's
/// [`TokenTotals`](crate::replay::TokenTotals) does not carry). Every completed
/// model call is priced from its response's model id, or counted as unpriced.
#[must_use]
pub fn estimate_cost(log: &[EventEnvelope]) -> CostSummary {
    let mut summary = CostSummary::default();
    for envelope in log {
        if let Event::ModelCallCompleted {
            response, usage, ..
        } = &envelope.event
        {
            match model_id_of_response(response).and_then(price_for) {
                Some(price) => {
                    summary.usd += cost(*usage, &price);
                    summary.priced_call_count += 1;
                }
                None => summary.unpriced_call_count += 1,
            }
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(frames: &[&str]) -> Vec<EventEnvelope> {
        frames
            .iter()
            .map(|frame| serde_json::from_str(frame).expect("frame is valid envelope JSON"))
            .collect()
    }

    // A completed model call whose response names the model, with one million
    // tokens each way so the arithmetic is a clean per-Mtok readout.
    const OPUS_MEGATOKEN: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":2,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallCompleted","payload":{"seq":1,"response":{"model":"claude-opus-4-8","text":"hi"},"usage":{"input_tokens":1000000,"output_tokens":1000000}}}}"#;
    // A completed call whose response names no model: unpriceable.
    const UNKNOWN_MODEL: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallCompleted","payload":{"seq":3,"response":{"text":"hi"},"usage":{"input_tokens":10,"output_tokens":10}}}}"#;

    #[test]
    fn exact_and_prefix_model_ids_resolve() {
        assert!(price_for("claude-opus-4-8").is_some());
        // A dated snapshot resolves via the prefix rule.
        assert!(price_for("claude-haiku-4-5-20251001").is_some());
        assert!(price_for("some-other-model").is_none());
    }

    #[test]
    fn call_cost_is_tokens_times_price() {
        // 1M in * $5 + 1M out * $25 = $30.
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };
        assert_eq!(call_cost_usd(Some("claude-opus-4-8"), usage), Some(30.0));
        assert_eq!(call_cost_usd(None, usage), None);
        assert_eq!(call_cost_usd(Some("mystery"), usage), None);
    }

    #[test]
    fn priced_log_is_available() {
        let summary = estimate_cost(&log(&[OPUS_MEGATOKEN]));
        assert_eq!(summary.priced_call_count, 1);
        assert_eq!(summary.unpriced_call_count, 0);
        assert!(summary.is_available());
        assert_eq!(summary.usd, 30.0);
    }

    #[test]
    fn empty_log_is_available_at_zero() {
        let summary = estimate_cost(&[]);
        assert!(summary.is_available());
        assert_eq!(summary.usd, 0.0);
    }

    #[test]
    fn any_unpriced_call_makes_the_total_unavailable() {
        let summary = estimate_cost(&log(&[OPUS_MEGATOKEN, UNKNOWN_MODEL]));
        assert_eq!(summary.priced_call_count, 1);
        assert_eq!(summary.unpriced_call_count, 1);
        assert!(!summary.is_available());
    }
}
