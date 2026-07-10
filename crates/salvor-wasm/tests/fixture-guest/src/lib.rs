//! The test guest: one component, several behaviors, selected by the
//! `"mode"` field of the JSON input. The well-behaved modes prove the JSON
//! boundary; the misbehaving ones (`spin`, `alloc`) exist so the host tests
//! can prove the sandbox's limits actually fire against real machine code.

// Generates the `Guest` trait and `export!` macro from the same WIT world the
// host compiles against. The path points at salvor-wasm's own `wit/`
// directory: one contract file, two sides.
wit_bindgen::generate!({ world: "tool", path: "../../wit" });

use serde_json::{Value, json};

struct Fixture;

impl Guest for Fixture {
    fn call(input: String) -> Result<String, String> {
        let request: Value =
            serde_json::from_str(&input).map_err(|err| format!("input is not JSON: {err}"))?;
        let mode = request
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing string field `mode`".to_owned())?;

        match mode {
            // Round-trip fidelity: hand back whatever JSON came in under
            // `value`, so the host test can assert structural equality on an
            // arbitrarily nested payload.
            "echo" => Ok(
                json!({ "echo": request.get("value").cloned().unwrap_or(Value::Null) }).to_string(),
            ),

            // The demo computation from the design spike.
            "wordcount" => {
                let text = request.get("text").and_then(Value::as_str).unwrap_or("");
                Ok(json!({
                    "words": text.split_whitespace().count(),
                    "chars": text.chars().count(),
                })
                .to_string())
            }

            // A guest-level failure: the `err` side of the WIT result. The
            // host must surface this as a handler error, not a crash.
            "fail" => Err(format!(
                "guest failure: {}",
                request
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("requested by test")
            )),

            // A runaway loop for the wall-time (epoch) and fuel tests.
            // black_box keeps the optimizer from folding the loop away: a
            // release-built counting loop without it collapses to its exit
            // value and the limit test would pass vacuously.
            "spin" => {
                let mut counter: u64 = 0;
                loop {
                    counter = std::hint::black_box(counter.wrapping_add(1));
                }
            }

            // An allocation bomb for the memory-cap test: keep 16 MiB
            // buffers alive until a memory.grow is denied and the allocator
            // aborts.
            "alloc" => {
                let mut buffers: Vec<Vec<u8>> = Vec::new();
                loop {
                    buffers.push(std::hint::black_box(vec![0xAB_u8; 16 * 1024 * 1024]));
                }
            }

            // Filesystem probes for the grant tests. Whether these succeed
            // is entirely the host's preopen decision; the guest just tries.
            "read_file" => {
                let path = request
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "read_file needs a string field `path`".to_owned())?;
                std::fs::read_to_string(path)
                    .map(|content| json!({ "content": content }).to_string())
                    .map_err(|err| format!("reading `{path}`: {err}"))
            }
            "write_file" => {
                let path = request
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "write_file needs a string field `path`".to_owned())?;
                let content = request.get("content").and_then(Value::as_str).unwrap_or("");
                std::fs::write(path, content)
                    .map(|()| json!({ "written": content.len() }).to_string())
                    .map_err(|err| format!("writing `{path}`: {err}"))
            }

            other => Err(format!("unknown mode: {other}")),
        }
    }
}

export!(Fixture);
