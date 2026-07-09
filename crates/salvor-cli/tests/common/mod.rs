//! Shared harness for the `salvor-cli` integration tests.
//!
//! The tests drive the real `salvor` binary (via `assert_cmd`) against a
//! scripted model (via `wiremock`) and a real MCP server (the in-repo
//! `salvor-mcp-count-fixture` binary). Nothing here touches the network or any
//! program outside this repository, so the suite is hermetic.
//!
//! Two facts make that work:
//!
//! - The model is a `wiremock` server the test controls, and the agent TOML
//!   the test writes points the client's `base_url` at it. The script selects
//!   a response by the number of `messages` in the request body, which stays
//!   correct across a resume where earlier turns replay from the log and never
//!   reach the server.
//! - The MCP server is this crate's `salvor-mcp-count-fixture` bin, located
//!   through the `CARGO_BIN_EXE_salvor-mcp-count-fixture` variable Cargo sets
//!   for the tests. Its `record` tool appends a line to a file, so a test can
//!   count real executions across a kill and resume by counting lines.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The counting MCP fixture binary, located by Cargo for the test build.
pub const COUNT_FIXTURE: &str = env!("CARGO_BIN_EXE_salvor-mcp-count-fixture");

/// A fresh `assert_cmd` handle for the `salvor` binary, with the store flag set
/// and tracing quieted so stderr does not drown the assertions.
pub fn salvor(store: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("salvor").expect("salvor binary builds");
    command.arg("--store").arg(store).env("RUST_LOG", "warn");
    command
}

/// Runs the `salvor` binary to completion off the async runtime, so a blocking
/// process wait does not starve the in-process `wiremock` server. Returns the
/// captured output.
pub async fn run_salvor(store: &Path, args: &[&str]) -> std::process::Output {
    let store = store.to_owned();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let mut command = salvor(&store);
        command.args(&args);
        command.output().expect("salvor runs")
    })
    .await
    .expect("blocking task joins")
}

/// A canned text (end-of-turn) response with fixed token usage.
pub fn text_response(text: &str, input_tokens: u64, output_tokens: u64) -> Value {
    json!({
        "id": "msg_text",
        "model": "test-model",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A canned response asking to call one tool.
pub fn tool_use_response(
    tool_use_id: &str,
    tool: &str,
    input: Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Value {
    json!({
        "id": format!("msg_tool_{tool_use_id}"),
        "model": "test-model",
        "role": "assistant",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": tool, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    })
}

/// A model that answers `POST /v1/messages` by matching the number of
/// `messages` in the request against the script, optionally gating one turn
/// behind a long delay until a shared flag is released (the kill test uses the
/// gate to hold the process alive at a chosen point).
pub struct GateModel {
    script: Vec<(usize, Value)>,
    gated_count: Option<usize>,
    released: Arc<AtomicBool>,
    delay: Duration,
}

impl GateModel {
    /// Mounts an ungated script.
    pub async fn mount(script: Vec<(usize, Value)>) -> MockServer {
        Self::mount_gated(
            script,
            None,
            Arc::new(AtomicBool::new(true)),
            Duration::ZERO,
        )
        .await
    }

    /// Mounts a script that delays the `gated_count`-message turn until
    /// `released` is set, returning the server.
    pub async fn mount_gated(
        script: Vec<(usize, Value)>,
        gated_count: Option<usize>,
        released: Arc<AtomicBool>,
        delay: Duration,
    ) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(Self {
                script,
                gated_count,
                released,
                delay,
            })
            .mount(&server)
            .await;
        server
    }
}

impl Respond for GateModel {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => return ResponseTemplate::new(400),
        };
        let count = body
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (expected, response) in &self.script {
            if *expected == count {
                let mut template = ResponseTemplate::new(200).set_body_json(response.clone());
                if self.gated_count == Some(count) && !self.released.load(Ordering::SeqCst) {
                    template = template.set_delay(self.delay);
                }
                return template;
            }
        }
        ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "test_script", "message": format!("no scripted response for {count} messages")}
        }))
    }
}

/// Writes an agent TOML into `dir` pointing the model at `model_uri` and
/// wiring the counting MCP fixture at `count_file`. `extra` is appended
/// verbatim (a `[budgets]` table, for instance). Returns the file path.
pub fn write_agent(dir: &Path, model_uri: &str, count_file: &Path, extra: &str) -> PathBuf {
    let toml = format!(
        "model = \"test-model\"\n\
         system_prompt = \"You are a test agent.\"\n\
         \n\
         [llm]\n\
         base_url = \"{model_uri}\"\n\
         max_retries = 0\n\
         \n\
         [[mcp_servers]]\n\
         command = \"{fixture}\"\n\
         args = [\"{count}\"]\n\
         effect_overrides = {{ record = \"write\" }}\n\
         {extra}\n",
        fixture = COUNT_FIXTURE,
        count = count_file.display(),
    );
    let path = dir.join("agent.toml");
    std::fs::write(&path, toml).expect("write agent toml");
    path
}

/// The number of lines in the count file (each is one real `record`
/// execution). Zero if the file does not exist yet.
pub fn count_lines(count_file: &Path) -> usize {
    std::fs::read_to_string(count_file)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}
