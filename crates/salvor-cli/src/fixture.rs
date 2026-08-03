//! `salvor run --fixture <DIR>`: a whole run from a directory, offline.
//!
//! A fixture directory is the smallest complete thing a reader can execute:
//! three files that together stand a run up with no API key, no network, and
//! nothing to configure. `examples/hero` is the one this was built for.
//!
//! | file | what it is |
//! |------|------------|
//! | `agent.toml` | an ordinary agent definition, unmodified |
//! | `input.json` | the run input, verbatim |
//! | `model.json` | the recorded model conversation |
//!
//! The agent file is not special-cased anywhere: it is loaded, built, and
//! driven by exactly the code path `--agent`/`--input` uses. The only thing
//! `--fixture` adds is a model that answers locally.
//!
//! # How the model gets pointed at the fixture
//!
//! [`AgentConfigExt::client_config`](crate::agent_config::AgentConfigExt::client_config)
//! resolves the endpoint from the agent file's `[llm] base_url_env`: when that
//! variable is set and non-empty its value wins, otherwise the file's own
//! `base_url` (or the public Anthropic endpoint) does. So one agent file
//! serves both modes, and switching between them is exporting a variable
//! rather than editing anything.
//!
//! `--fixture` binds a scripted model on a free local port and exports that
//! variable itself, before the agent is built. An agent file that declares no
//! `base_url_env` has no such switch, and [`Fixture::start_model`] says so
//! plainly rather than silently running against the real endpoint.
//!
//! # Why the script is keyed by message count
//!
//! `model.json` keys each turn by the number of messages in the request body
//! when that turn reaches the model. Every turn appends to the conversation,
//! so turn `k` carries `2k - 1` messages. This is the same scheme
//! [`crate::demo_script`] uses, for the same reason: selection is **stateless**,
//! and therefore replay-safe. After a `kill -9` and a `resume` the earlier
//! turns are read from the durable log and never reach the model, and the
//! first live turn arrives with exactly the message count it would have had
//! uninterrupted. So one running server serves the pre-kill run and the
//! resume identically, with no state to rewind.
//!
//! An unmatched count is answered with a `500` shaped like the API's error
//! envelope, the same contract the CLI tests' wiremock model uses. A fixture
//! whose script has drifted from its agent fails loudly on the first
//! mismatched turn instead of hanging or quietly improvising.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::agent_config::AgentConfig;

/// The agent definition inside a fixture directory.
const AGENT_FILE: &str = "agent.toml";
/// The run input inside a fixture directory.
const INPUT_FILE: &str = "input.json";
/// The recorded model conversation inside a fixture directory.
const MODEL_FILE: &str = "model.json";

/// One turn of a recorded conversation: the request it answers, identified by
/// its message count, and the response body to return verbatim.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptTurn {
    /// The number of messages in the request body when this turn reaches the
    /// model. Turn `k` carries `2k - 1`; see the module docs.
    messages: usize,
    /// An Anthropic Messages API response body, served as-is.
    response: Value,
}

/// The parsed `model.json`.
///
/// Unknown fields are rejected, matching how the agent file is parsed: a typo
/// like `turn` instead of `turns` should be a loud error, not a script that
/// silently answers nothing. `_comment` is the one exception, declared here so
/// a fixture can document itself inside the file it documents.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelScript {
    /// The recorded turns, in any order (selection is by message count, not
    /// by position).
    turns: Vec<ScriptTurn>,
    /// Free-form documentation, ignored. The leading underscore is both the
    /// JSON key and the Rust field name, so it is exempt from the dead-code
    /// lint without a suppression attribute.
    #[serde(default)]
    _comment: Option<Value>,
}

/// A loaded fixture directory: everything read off disk, nothing started yet.
///
/// Loading is separated from starting so a malformed fixture fails before any
/// port is bound, any environment variable is exported, or any run head is
/// written to the store.
pub struct Fixture {
    /// The fixture's agent definition, an ordinary path handed to the ordinary
    /// loader.
    agent_path: PathBuf,
    /// The run input, already parsed. Parsed here rather than routed through
    /// [`crate::commands::parse_input`] because that function's `@path`
    /// convention belongs to the `--input` flag; a fixture's input is always
    /// the literal contents of `input.json`.
    input: Value,
    /// The recorded turns, shared with the server task once it starts.
    script: Arc<Vec<ScriptTurn>>,
}

impl Fixture {
    /// Reads a fixture directory: the agent path, the input, and the script.
    ///
    /// # Errors
    ///
    /// Fails when the directory or any of its three files is missing or
    /// unreadable, when `input.json` is not JSON, or when `model.json` is not
    /// a valid script document. Every message names the file, since the point
    /// of a fixture is that a reader can fix it by opening one file.
    pub fn load(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            bail!(
                "--fixture {} is not a directory; it should hold {AGENT_FILE}, {INPUT_FILE}, and \
                 {MODEL_FILE} (try `salvor run --fixture examples/hero`)",
                dir.display()
            );
        }

        let agent_path = dir.join(AGENT_FILE);
        if !agent_path.is_file() {
            bail!(
                "fixture {} has no {AGENT_FILE}; a fixture directory holds {AGENT_FILE}, \
                 {INPUT_FILE}, and {MODEL_FILE}",
                dir.display()
            );
        }

        let input_path = dir.join(INPUT_FILE);
        let input_text = std::fs::read_to_string(&input_path)
            .with_context(|| format!("reading fixture input {}", input_path.display()))?;
        let input: Value = serde_json::from_str(&input_text)
            .with_context(|| format!("{} is not valid JSON", input_path.display()))?;

        let model_path = dir.join(MODEL_FILE);
        let model_text = std::fs::read_to_string(&model_path)
            .with_context(|| format!("reading fixture model script {}", model_path.display()))?;
        let script: ModelScript = serde_json::from_str(&model_text).with_context(|| {
            format!(
                "{} is not a valid model script (expected {{ \"turns\": [ {{ \"messages\": <n>, \
                 \"response\": {{ .. }} }} ] }})",
                model_path.display()
            )
        })?;
        if script.turns.is_empty() {
            bail!(
                "fixture model script {} has no turns; it would answer nothing",
                model_path.display()
            );
        }
        for turn in &script.turns {
            // `response` is stored and served as a bare `Value` (the model
            // returns it verbatim, unknown fields and all), but a value that
            // does not even carry the fields every Messages API response has
            // would otherwise fail far from here: deep in `salvor-llm`'s own
            // decode, with no mention of `model.json` or which turn. Catching
            // it here, at load, is what makes the failure point at the file a
            // reader can actually fix, the same way a malformed `turns` key
            // does above.
            serde_json::from_value::<salvor_llm::MessageResponse>(turn.response.clone())
                .with_context(|| {
                    format!(
                        "{}: turn for {} message(s) has a \"response\" that is not a valid \
                         Messages API response (expected {{ \"id\": \"...\", \"model\": \"...\", \
                         \"role\": \"assistant\", \"content\": [ {{ \"type\": \"text\", \"text\": \
                         \"...\" }} ], \"usage\": {{ \"input_tokens\": <n>, \"output_tokens\": \
                         <n> }} }})",
                        model_path.display(),
                        turn.messages
                    )
                })?;
        }

        Ok(Self {
            agent_path,
            input,
            script: Arc::new(script.turns),
        })
    }

    /// The fixture's agent definition path, for the ordinary load-and-build
    /// path and for any report that names the agent file.
    #[must_use]
    pub fn agent_path(&self) -> &Path {
        &self.agent_path
    }

    /// The fixture's run input.
    #[must_use]
    pub fn input(&self) -> &Value {
        &self.input
    }

    /// Binds the scripted model on a free local port and exports the agent's
    /// declared `base_url_env` variable at it.
    ///
    /// Must be called **before** the agent is built: the client config reads
    /// that variable once, at build time.
    ///
    /// # Errors
    ///
    /// Fails when the agent declares no `[llm] base_url_env` (there is then no
    /// switch to flip, and running against the real endpoint would be a silent
    /// surprise), or when the loopback port cannot be bound.
    pub async fn start_model(&self, config: &AgentConfig) -> Result<FixtureModel> {
        let Some(env_name) = config.llm.base_url_env.as_deref() else {
            bail!(
                "fixture agent {} declares no `[llm] base_url_env`, so there is no variable to \
                 point at the fixture's model; add one (for example `base_url_env = \
                 \"SALVOR_HERO_BASE_URL\"` under [llm]) so the same file can target a real model \
                 when it is unset",
                self.agent_path.display()
            );
        };

        // Port 0 lets the OS pick a free port, so concurrent fixture runs (the
        // test suite runs several at once) never collide on a fixed one.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding a local port for the fixture model")?;
        let addr = listener
            .local_addr()
            .context("reading the fixture model's bound address")?;
        let url = format!("http://{addr}");

        // SAFETY: `set_var` is unsafe under edition 2024 because a concurrent
        // `getenv` in another thread is a data race. This call is the last
        // thing before the server task is spawned and the agent is built, and
        // at this point in `salvor run` nothing else in the process is reading
        // or writing the environment: the store is already open, the model
        // server task below never touches the environment, and the MCP
        // children that do inherit it are spawned later, from the agent build
        // that follows this call. An environment variable is also the only
        // channel available: the agent file names the variable, and
        // `client_config` reads it from the real environment, which is exactly
        // what makes one agent file serve both the fixture and a real model.
        unsafe { std::env::set_var(env_name, &url) };
        tracing::info!(%url, env = env_name, turns = self.script.len(), "fixture model listening");

        let (stop, stopped) = oneshot::channel::<()>();
        let router = Router::new()
            .route("/v1/messages", post(messages))
            .with_state(Arc::clone(&self.script));
        let handle = tokio::spawn(async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    // A dropped sender shuts the server down too, so a caller
                    // that forgets to call `shutdown` still tears down rather
                    // than hanging the process.
                    let _ = stopped.await;
                })
                .await;
            if let Err(error) = served {
                tracing::warn!(%error, "the fixture model server stopped with an error");
            }
        });

        Ok(FixtureModel { stop, handle })
    }
}

/// A running fixture model server. Held for the length of the run and shut
/// down afterward, whatever the outcome.
pub struct FixtureModel {
    /// Signals the graceful shutdown.
    stop: oneshot::Sender<()>,
    /// The serving task, awaited so the port is genuinely released before the
    /// command returns.
    handle: JoinHandle<()>,
}

impl FixtureModel {
    /// Stops the server and waits for the task to finish.
    ///
    /// Errors are logged, not propagated, exactly as MCP server teardown is:
    /// the run has already finished, so a teardown hiccup must not turn a
    /// successful run into a failed command.
    pub async fn shutdown(self) {
        let _ = self.stop.send(());
        if let Err(error) = self.handle.await {
            tracing::warn!(%error, "the fixture model server did not shut down cleanly");
        }
    }
}

/// `POST /v1/messages`: the one route the fixture model answers.
///
/// The body is taken as raw bytes rather than through the `Json` extractor so
/// that a body which is not JSON at all still gets the scripted-miss envelope
/// below instead of axum's own rejection: one shape of failure, whatever went
/// wrong.
async fn messages(
    State(script): State<Arc<Vec<ScriptTurn>>>,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let count = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0);

    for turn in script.iter() {
        if turn.messages == count {
            tracing::debug!(messages = count, "fixture model: scripted turn served");
            return (StatusCode::OK, Json(turn.response.clone()));
        }
    }

    // Fail loudly. A silent or empty answer here would look like a stuck run;
    // an error envelope surfaces as a model-call failure naming the exact
    // count the script has no turn for.
    let expected: Vec<String> = script
        .iter()
        .map(|turn| turn.messages.to_string())
        .collect();
    tracing::warn!(
        messages = count,
        scripted = %expected.join(", "),
        "fixture model: no scripted response for this turn"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "type": "fixture_script",
                "message": format!(
                    "no scripted response for {count} messages (the script answers: {})",
                    expected.join(", ")
                )
            }
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a fixture directory with the three required files (`agent.toml`
    /// gets a minimal but valid body; the caller supplies `model.json`'s
    /// text), and returns it for [`Fixture::load`].
    fn write_fixture(model_json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(AGENT_FILE), "model = \"claude-opus-4-8\"\n")
            .expect("write agent.toml");
        std::fs::write(dir.path().join(INPUT_FILE), "\"hello\"").expect("write input.json");
        std::fs::write(dir.path().join(MODEL_FILE), model_json).expect("write model.json");
        dir
    }

    /// A turn's `response` that is missing fields every Messages API response
    /// has (here, `usage`) fails at load, naming `model.json`, the turn (by
    /// its message count), and the expected minimal shape, exactly as a
    /// malformed `turns` key already does for the whole file.
    #[test]
    fn a_response_missing_required_fields_teaches_the_expected_shape() {
        let dir = write_fixture(
            r#"{"turns": [{"messages": 1, "response": {"id": "msg_1", "model": "m", "role": "assistant", "content": []}}]}"#,
        );
        let error = match Fixture::load(dir.path()) {
            Ok(_) => panic!("a response with no usage must fail"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains(MODEL_FILE), "names the file: {message}");
        assert!(
            message.contains("\"response\""),
            "names the field: {message}"
        );
        assert!(
            message.contains("\"usage\""),
            "teaches the expected shape: {message}"
        );
    }

    /// A turn whose `response` has every required field loads cleanly.
    #[test]
    fn a_well_formed_response_loads() {
        let dir = write_fixture(
            r#"{"turns": [{"messages": 1, "response": {
                "id": "msg_1", "model": "m", "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }}]}"#,
        );
        Fixture::load(dir.path()).expect("a well-formed response must load");
    }
}
