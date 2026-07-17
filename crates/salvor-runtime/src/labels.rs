//! Sanity bounds for run labels.
//!
//! A label is a short correlation tag on a run (a build id, an environment
//! name), not a payload: [`Event::RunStarted::labels`](salvor_core::Event)
//! carries whatever a caller supplies, with no cap of its own, because the
//! event type is a durable record, not a policy enforcer (an old or foreign
//! log is trusted and replayed exactly as recorded, whatever it holds). The
//! bounds live here instead, checked at every point a run is actually
//! created: [`crate::RunCtx::begin`] going live, the server's `POST
//! /v1/runs`, and the client-run event append that carries a fresh
//! `RunStarted`. One shared function means the three surfaces cannot drift.

use std::collections::BTreeMap;

/// The most labels one run may carry.
pub const MAX_LABELS: usize = 16;

/// The longest a label key may be, in bytes.
pub const MAX_LABEL_KEY_LEN: usize = 64;

/// The longest a label value may be, in bytes.
pub const MAX_LABEL_VALUE_LEN: usize = 256;

/// Checks `labels` against the bounds documented at module level.
///
/// # Errors
///
/// A human-readable description of the first violation found: too many
/// labels, or a key/value over its length cap. Checked in that order, so a
/// caller with both problems sees the count violation first.
pub fn validate_labels(labels: &BTreeMap<String, String>) -> Result<(), String> {
    if labels.len() > MAX_LABELS {
        return Err(format!(
            "a run may carry at most {MAX_LABELS} labels, got {}",
            labels.len()
        ));
    }
    for (key, value) in labels {
        if key.len() > MAX_LABEL_KEY_LEN {
            return Err(format!(
                "label key `{key}` is {} bytes, over the {MAX_LABEL_KEY_LEN}-byte cap",
                key.len()
            ));
        }
        if value.len() > MAX_LABEL_VALUE_LEN {
            return Err(format!(
                "label value for key `{key}` is {} bytes, over the {MAX_LABEL_VALUE_LEN}-byte cap",
                value.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn empty_and_small_label_sets_are_valid() {
        assert!(validate_labels(&BTreeMap::new()).is_ok());
        assert!(validate_labels(&map(&[("build", "42"), ("env", "prod")])).is_ok());
    }

    #[test]
    fn more_than_the_cap_is_rejected() {
        let labels: BTreeMap<String, String> = (0..=MAX_LABELS)
            .map(|i| (format!("k{i}"), "v".to_owned()))
            .collect();
        let err = validate_labels(&labels).expect_err("over the cap must be rejected");
        assert!(err.contains("at most 16 labels"), "{err}");
    }

    #[test]
    fn exactly_the_cap_is_valid() {
        let labels: BTreeMap<String, String> = (0..MAX_LABELS)
            .map(|i| (format!("k{i}"), "v".to_owned()))
            .collect();
        assert!(validate_labels(&labels).is_ok());
    }

    #[test]
    fn an_oversized_key_is_rejected() {
        let long_key = "k".repeat(MAX_LABEL_KEY_LEN + 1);
        let err =
            validate_labels(&map(&[(&long_key, "v")])).expect_err("a long key must be rejected");
        assert!(err.contains("over the 64-byte cap"), "{err}");
    }

    #[test]
    fn an_oversized_value_is_rejected() {
        let long_value = "v".repeat(MAX_LABEL_VALUE_LEN + 1);
        let err = validate_labels(&map(&[("k", &long_value)]))
            .expect_err("a long value must be rejected");
        assert!(err.contains("over the 256-byte cap"), "{err}");
    }

    #[test]
    fn keys_and_values_exactly_at_the_cap_are_valid() {
        let key = "k".repeat(MAX_LABEL_KEY_LEN);
        let value = "v".repeat(MAX_LABEL_VALUE_LEN);
        assert!(validate_labels(&map(&[(&key, &value)])).is_ok());
    }
}
