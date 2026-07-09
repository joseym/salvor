//! Canonical JSON serialization and content hashing.
//!
//! Two recorded event fields are content hashes: `agent_def_hash` on
//! `RunStarted` and `request_hash` on `ModelCallRequested`. Both must be
//! reproducible bit for bit on replay, because the replay cursor compares
//! them against the recorded values, so both are computed over a *canonical*
//! serialization rather than whatever byte order `serde_json` happens to
//! produce.
//!
//! # The canonical form
//!
//! [`canonical_json`] renders a [`Value`] as compact JSON with one added
//! rule: **object keys are emitted in ascending byte order**. Everything else
//! matches `serde_json`'s compact output: arrays keep their order, strings
//! are JSON-escaped by `serde_json`, numbers use `serde_json::Number`'s own
//! display, and `null`/`true`/`false` are literals. The sort is applied
//! recursively.
//!
//! The explicit sort exists because `serde_json`'s map ordering is a *build*
//! property, not a guarantee: its `preserve_order` cargo feature swaps the
//! sorted `BTreeMap` for an insertion-ordered map, and any dependency
//! anywhere in the graph can switch that feature on through feature
//! unification. A hash that changed because an unrelated crate toggled a
//! feature would break every stored run, so this module never relies on map
//! iteration order.
//!
//! # The hash form
//!
//! [`hash_value`] is `"sha256:" + hex(sha256(canonical_json(value)))`. The
//! prefix names the algorithm in the stored string, so a future algorithm
//! change is detectable in old logs instead of silently colliding.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Renders a JSON value in the canonical form documented at module level:
/// compact, with object keys recursively sorted in ascending byte order.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// The recursive writer behind [`canonical_json`].
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        // serde_json::Number's Display is its wire form; reusing it keeps
        // number formatting identical to serde_json's own output.
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => {
            out.push_str(
                &serde_json::to_string(text).expect("serializing a string to JSON cannot fail"),
            );
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Collect and sort the keys explicitly instead of trusting map
            // iteration order; see the module docs for why.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(key).expect("serializing a string to JSON cannot fail"),
                );
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
    }
}

/// Lowercase hex of the SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The content hash of a JSON value: `sha256:` plus the hex SHA-256 of its
/// canonical serialization. This is the exact string recorded in
/// `agent_def_hash` and `request_hash` event fields.
#[must_use]
pub fn hash_value(value: &Value) -> String {
    format!("sha256:{}", sha256_hex(canonical_json(value).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Key order in the input must not affect the canonical form.
    #[test]
    fn canonical_json_sorts_object_keys_recursively() {
        let a: Value =
            serde_json::from_str(r#"{"b": {"z": 1, "a": 2}, "a": [3, {"y": 4, "x": 5}]}"#).unwrap();
        let b: Value =
            serde_json::from_str(r#"{"a": [3, {"x": 5, "y": 4}], "b": {"a": 2, "z": 1}}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            canonical_json(&a),
            r#"{"a":[3,{"x":5,"y":4}],"b":{"a":2,"z":1}}"#
        );
    }

    /// Strings are escaped exactly as serde_json escapes them.
    #[test]
    fn canonical_json_escapes_strings() {
        let value = json!({"text": "line\none \"two\""});
        assert_eq!(canonical_json(&value), r#"{"text":"line\none \"two\""}"#);
    }

    /// The hash string carries its algorithm prefix and is stable.
    #[test]
    fn hash_value_is_prefixed_and_stable() {
        let hash = hash_value(&json!({"a": 1}));
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash, hash_value(&json!({"a": 1})));
        assert_ne!(hash, hash_value(&json!({"a": 2})));
    }
}
