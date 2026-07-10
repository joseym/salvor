//! The example guest: text statistics behind the `salvor:tool@0.1.0` world.
//!
//! This is the whole tool. One export, JSON in, JSON out; the sandbox host
//! decides everything else (what the model is told about it, what it may
//! touch, how long it may run). Note what is absent: no tool name, no
//! description, no schema, no effect declaration. The guest has no channel
//! for any of those, on purpose.

// Generates the `Guest` trait and `export!` macro from the WIT world in
// wit/, which is a copy of crates/salvor-wasm/wit so this crate stays
// self-contained when copied out of the repository.
wit_bindgen::generate!({ world: "tool", path: "wit" });

use serde_json::{Value, json};

struct Wordcount;

impl Guest for Wordcount {
    fn call(input: String) -> Result<String, String> {
        // The input is whatever JSON the model produced. The operator's
        // declared schema steers the model toward `{"text": ...}`, but the
        // guest still validates: it trusts nothing, least of all that the
        // schema was enforced.
        let request: Value =
            serde_json::from_str(&input).map_err(|err| format!("input is not JSON: {err}"))?;
        let text = request
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing string field `text`".to_owned())?;

        let words: Vec<&str> = text.split_whitespace().collect();
        let longest = words
            .iter()
            .max_by_key(|word| word.chars().count())
            .copied()
            .unwrap_or("");

        // The ok side of the WIT result must be a JSON string; returning
        // `Err(message)` instead surfaces to the runtime as a handler error.
        Ok(json!({
            "words": words.len(),
            "chars": text.chars().count(),
            "lines": text.lines().count(),
            "longest_word": longest,
        })
        .to_string())
    }
}

export!(Wordcount);
