//! A hermetic scripted model server for recording the demo.
//!
//! This is demo-support code behind the crate's default-on `fixture` feature,
//! not part of the `salvor` product binary. It exists for one reason: the demo
//! GIF must record without a network or an API key, deterministically, every
//! time. It serves the same twenty-turn scripted conversation as the
//! `demo_run` integration test (both read it from [`salvor_cli::demo_script`],
//! so the recording and the test that guards it cannot disagree), over HTTP,
//! speaking just enough of the Messages API for the demo agent to drive.
//!
//! The switch between this and the real model is one environment variable.
//! `demo/agent.toml` declares `base_url_env = "SALVOR_DEMO_BASE_URL"`; export
//! it pointing at this server and every model call lands here with no key,
//! unset it and export `ANTHROPIC_API_KEY` and the same file targets the
//! public endpoint.
//!
//! # What it speaks
//!
//! One route, `POST /v1/messages`. It counts the `messages` array in the
//! request body and returns the scripted response for that count (turn `k`
//! carries `2k - 1` messages; see [`salvor_cli::demo_script`]). An unmatched
//! count returns a `500` shaped like the API's error envelope, the same
//! contract the test's wiremock model uses, so a script or endpoint mismatch
//! fails loudly rather than hanging. Selection is stateless, which is what
//! lets one running server serve both the pre-kill run and the resume: the
//! resume's first live turn arrives with exactly the message count it would
//! have had uninterrupted.
//!
//! # The per-turn delay
//!
//! Each response is held for a configurable delay (default a few hundred
//! milliseconds) so the run is slow enough to watch and to `kill -9` on
//! camera. Because the delay sits on the *model* call and the demo's tool
//! calls are near-instant local file operations, a kill at an arbitrary wall
//! clock lands almost always while the process waits on a model call, which
//! is a recoverable crash (awaiting-model), never the one unrecoverable
//! window of a write tool call in flight.
//!
//! # Configuration
//!
//! Port and delay come from flags, then environment, then a default:
//! `--port` / `SALVOR_DEMO_MODEL_PORT` (default `8899`), and `--delay-ms` /
//! `SALVOR_DEMO_MODEL_DELAY_MS` (default `300`). Binding `--port 0` picks a
//! free port; the chosen port is printed so a caller can read it back.
//!
//! The script to serve comes from `--script` / `SALVOR_DEMO_MODEL_SCRIPT`
//! (flag first, then environment), with no default value of its own: when
//! neither is set, the server serves the built-in
//! [`salvor_cli::demo_script`] exactly as it does today. When one is set, it
//! names a file holding a script in the format described below, and every
//! turn is matched against that file instead.
//!
//! # The script file format
//!
//! A `--script` file holds one of two shapes.
//!
//! **A JSON array**: one conversation, served whatever the request's system
//! prompt says. Element `i` (zero-based) is the full Messages API response
//! body served for turn `i + 1`, the turn that arrives with `2i + 1`
//! messages, the same indexing [`salvor_cli::demo_script`] uses internally.
//! Each element is written out exactly as the API sends it, including any
//! `tool_use` content blocks and the `usage` object, since the file is read
//! at full fidelity rather than through some friendlier shorthand.
//!
//! **A JSON object**: several named conversations, mapping a name to an
//! array of responses in exactly the form above. This is the shape a graph
//! needs. Selection by message count alone cannot serve a graph, because
//! every agent node is its own run with its own message list, so all of them
//! make their first model call carrying exactly one message and would
//! collide on element 0. Naming the conversations and selecting on the
//! system prompt separates them, since that is the thing agent nodes
//! genuinely differ on.
//!
//! # Selecting a named conversation
//!
//! A request selects the conversation whose name appears as a **substring**
//! of the request's top-level `system` string (a system prompt sent as an
//! array of text blocks is joined before matching, so either wire form
//! selects alike). Selection is deterministic and never depends on the order
//! the names happen to appear in the file: exactly one name must match.
//!
//! - Exactly one match: that conversation answers, indexed by `2i + 1`.
//! - No match: a `500` error envelope saying no conversation name matched,
//!   quoting a short head of the system prompt so the mismatch is
//!   diagnosable.
//! - More than one match: a `500` error envelope naming every conversation
//!   that matched. An ambiguous script is an authoring mistake, and quietly
//!   picking one would leave an example that works by luck.
//!
//! # Failing loudly
//!
//! An unreadable path, JSON that fails to parse, a top-level value that is
//! neither an array nor an object, a named conversation whose value is not
//! an array, an array holding a non-object element, an empty array, or an
//! empty object each panic at startup naming the path and the problem, the
//! same loud-failure convention `--port` and `--delay-ms` already follow. A
//! script that can never answer a turn is a mistake, not a configuration.
//! Past startup, a turn count the selected conversation does not cover still
//! answers with the `500` error envelope described above.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use salvor_cli::demo_script;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The default port, matching what `docs/demo.tape` points the agent at.
const DEFAULT_PORT: u16 = 8899;

/// The default per-turn delay in milliseconds: watchable, and long enough to
/// fire a `kill -9` while the run waits on a model call.
const DEFAULT_DELAY_MS: u64 = 300;

/// The resolved settings this server runs with.
struct Settings {
    port: u16,
    delay: Duration,
    /// Path to a `--script` file, if `--script` or `SALVOR_DEMO_MODEL_SCRIPT`
    /// named one. `None` means serve the built-in `demo_script::script()`.
    script_path: Option<String>,
}

/// Resolves a `u64` setting from a flag, then an environment variable, then a
/// default. Panics with a clear message on an unparsable value, since a
/// mistyped port or delay should fail loudly at startup, not silently.
fn resolve_u64(args: &[String], flag: &str, env: &str, default: u64) -> u64 {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        let raw = args
            .get(index + 1)
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        return raw
            .parse()
            .unwrap_or_else(|_| panic!("{flag} value `{raw}` is not a number"));
    }
    if let Ok(raw) = std::env::var(env)
        && !raw.is_empty()
    {
        return raw
            .parse()
            .unwrap_or_else(|_| panic!("{env}=`{raw}` is not a number"));
    }
    default
}

/// Resolves an optional string setting from a flag, then an environment
/// variable. Unlike [`resolve_u64`] there is no default: `None` means
/// neither was set, and the caller decides what "unset" means.
fn resolve_string(args: &[String], flag: &str, env: &str) -> Option<String> {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        let raw = args
            .get(index + 1)
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        return Some(raw.clone());
    }
    if let Ok(raw) = std::env::var(env)
        && !raw.is_empty()
    {
        return Some(raw);
    }
    None
}

/// Reads flags and environment into [`Settings`].
fn settings() -> Settings {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = resolve_u64(
        &args,
        "--port",
        "SALVOR_DEMO_MODEL_PORT",
        u64::from(DEFAULT_PORT),
    );
    let delay = resolve_u64(
        &args,
        "--delay-ms",
        "SALVOR_DEMO_MODEL_DELAY_MS",
        DEFAULT_DELAY_MS,
    );
    let script_path = resolve_string(&args, "--script", "SALVOR_DEMO_MODEL_SCRIPT");
    Settings {
        port: u16::try_from(port).expect("port fits in u16"),
        delay: Duration::from_millis(delay),
        script_path,
    }
}

/// One conversation's turns as `(message_count, response)` pairs, the same
/// shape `demo_script::script()` returns: turn `k` keyed by `2k - 1`.
type Turns = Vec<(usize, Value)>;

/// A loaded script: one conversation, or several selected by system prompt.
#[derive(Debug, PartialEq)]
enum Script {
    /// One conversation, served whatever the system prompt says. The JSON
    /// array file form, and what the built-in `demo_script` is.
    One(Turns),
    /// Named conversations, selected by matching a name against the request's
    /// system prompt. Held as a sorted vector rather than a map so error
    /// messages list names in a fixed order and nothing depends on the order
    /// the names happened to appear in the file.
    Named(Vec<(String, Turns)>),
}

impl Script {
    /// A one-line description of what was loaded, for the startup log.
    fn summary(&self) -> String {
        match self {
            Script::One(turns) => format!("{} scripted turns", turns.len()),
            Script::Named(conversations) => {
                let names: Vec<String> = conversations
                    .iter()
                    .map(|(name, turns)| format!("{name} ({} turns)", turns.len()))
                    .collect();
                format!(
                    "{} named conversations: {}",
                    conversations.len(),
                    names.join(", ")
                )
            }
        }
    }
}

/// Turns one conversation's JSON array into [`Turns`], keying element `i` to
/// the `2i + 1` messages turn `i + 1` arrives with. `conversation` names the
/// enclosing conversation for a named script and is `None` for the plain
/// array form, which keeps the array form's messages exactly as they were.
/// Panics on an empty array or a non-object element.
fn turns_from(path: &str, conversation: Option<&str>, entries: Vec<Value>) -> Turns {
    if entries.is_empty() {
        match conversation {
            None => panic!(
                "--script path `{path}` holds an empty array; a script that can never answer a turn is not valid"
            ),
            Some(name) => panic!(
                "--script path `{path}` conversation `{name}` holds an empty array; a conversation that can never answer a turn is not valid"
            ),
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        if !entry.is_object() {
            match conversation {
                None => panic!("--script path `{path}` element {index} is not a JSON object"),
                Some(name) => panic!(
                    "--script path `{path}` conversation `{name}` element {index} is not a JSON object"
                ),
            }
        }
    }
    entries
        .into_iter()
        .enumerate()
        .map(|(index, response)| (2 * index + 1, response))
        .collect()
}

/// Loads a `--script` file: either a JSON array holding one conversation, or
/// a JSON object mapping names to conversations (see the module doc for the
/// full format and the selection rule). Every element is a full Messages API
/// response body, read at full fidelity. Panics with a message naming `path`
/// and the specific problem on every way the file can be wrong: unreadable,
/// not JSON, neither an array nor an object, a named conversation that is not
/// an array, an element that is not a JSON object, an empty array, or an
/// empty object, since a script that can never answer a turn is a mistake,
/// not a configuration.
fn load_script(path: &str) -> Script {
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("--script path `{path}` could not be read: {error}"));
    let value: Value = serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("--script path `{path}` does not parse as JSON: {error}"));
    match value {
        Value::Array(entries) => Script::One(turns_from(path, None, entries)),
        Value::Object(map) => {
            if map.is_empty() {
                panic!(
                    "--script path `{path}` holds an empty object; a script with no conversations can never answer a turn"
                );
            }
            let mut conversations: Vec<(String, Turns)> = map
                .into_iter()
                .map(|(name, entry)| {
                    let Value::Array(entries) = entry else {
                        panic!(
                            "--script path `{path}` conversation `{name}` must hold a JSON array of responses"
                        );
                    };
                    let turns = turns_from(path, Some(&name), entries);
                    (name, turns)
                })
                .collect();
            // Sorted so an ambiguity message lists names in a fixed order,
            // whatever order the parser produced them in.
            conversations.sort_by(|left, right| left.0.cmp(&right.0));
            Script::Named(conversations)
        }
        _ => panic!(
            "--script path `{path}` must hold a JSON array or a JSON object of named conversations; found a different JSON value"
        ),
    }
}

/// Resolves the script to serve: the file at `script_path` if one was given,
/// the built-in `demo_script::script()` otherwise. This is the only place
/// that decision is made, so the default path (`None`) is provably identical
/// to serving `demo_script::script()` directly.
fn resolve_script(script_path: Option<&str>) -> Script {
    match script_path {
        Some(path) => load_script(path),
        None => Script::One(demo_script::script()),
    }
}

/// How many characters of a system prompt an unmatched-conversation error
/// quotes back. Long enough to recognize which agent sent the request,
/// short enough to stay one readable line.
const SYSTEM_HEAD_CHARS: usize = 80;

/// A short, quotable head of a system prompt for an error message. Takes
/// characters (not bytes) so a multi-byte prompt cannot panic on a slice, and
/// flattens newlines so the message stays one line.
fn system_head(system: &str) -> String {
    let flattened: String = system
        .chars()
        .take(SYSTEM_HEAD_CHARS)
        .map(|character| if character == '\n' { ' ' } else { character })
        .collect();
    if system.chars().count() > SYSTEM_HEAD_CHARS {
        format!("{flattened}...")
    } else {
        flattened
    }
}

/// The `500` error envelope, shaped like the API's own. Every miss, whether
/// no conversation matched, several did, or the selected conversation has no
/// response for this turn, answers in this one shape.
fn error_envelope(kind: &str, message: String) -> (u16, Value) {
    (
        500,
        json!({
            "error": {
                "type": kind,
                "message": message
            }
        }),
    )
}

/// Picks the conversation that answers a request carrying `system`, or the
/// error envelope explaining why none can. A one-conversation script always
/// answers; a named script requires exactly one name to be a substring of
/// `system`, since zero is a script that does not cover this agent and more
/// than one is an authoring mistake that must not be resolved by luck.
fn conversation_for<'a>(script: &'a Script, system: &str) -> Result<&'a Turns, (u16, Value)> {
    let conversations = match script {
        Script::One(turns) => return Ok(turns),
        Script::Named(conversations) => conversations,
    };

    let matched: Vec<&'a (String, Turns)> = conversations
        .iter()
        .filter(|(name, _)| system.contains(name.as_str()))
        .collect();

    if matched.len() == 1 {
        return Ok(&matched[0].1);
    }

    if matched.is_empty() {
        let known: Vec<&str> = conversations
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        return Err(error_envelope(
            "demo_script_no_conversation",
            format!(
                "no conversation name matched the system prompt `{}`; the script names {}",
                system_head(system),
                known.join(", ")
            ),
        ));
    }

    let names: Vec<&str> = matched.iter().map(|(name, _)| name.as_str()).collect();
    Err(error_envelope(
        "demo_script_ambiguous_conversation",
        format!(
            "{} conversation names matched the same system prompt `{}` ({}); exactly one must match",
            names.len(),
            system_head(system),
            names.join(", ")
        ),
    ))
}

/// The scripted response for a request whose body holds `count` messages and
/// carries `system` as its system prompt. Mirrors the wiremock model in the
/// tests: a hit is `200` with the response, a miss is `500` with an error
/// envelope. Conversation selection happens first, then the existing
/// per-turn lookup within the selected conversation, unchanged.
fn response_for(script: &Script, count: usize, system: &str) -> (u16, Value) {
    let turns = match conversation_for(script, system) {
        Ok(turns) => turns,
        Err(envelope) => return envelope,
    };
    for (expected, response) in turns {
        if *expected == count {
            return (200, response.clone());
        }
    }
    error_envelope(
        "demo_script",
        format!("no scripted response for {count} messages"),
    )
}

/// The request's system prompt as plain text. The API accepts it as a bare
/// string (what `salvor` sends: `MessageRequest::system` is a `System::Text`
/// for an agent's `system_prompt`) or as an array of text blocks, so both are
/// flattened here and select alike. A request with no system prompt yields an
/// empty string, which no non-empty conversation name can match.
fn system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<&str>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Serves one keep-alive connection: read a request, answer it, repeat until
/// the client closes. reqwest (the client `salvor` uses) pools connections, so
/// a run's twenty requests can arrive on one socket; the loop handles that.
async fn serve_connection(
    stream: TcpStream,
    script: Arc<Script>,
    delay: Duration,
    requests: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        // The request line (e.g. `POST /v1/messages HTTP/1.1`). A zero-length
        // read is a clean client close: end the connection.
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_owned();

        // Headers until the blank line; the only one we need is Content-Length.
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }

        // The JSON body, exactly Content-Length bytes.
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;

        let (status, payload) = if path == "/v1/messages" {
            // Parsed once: the message count keys the turn, and the system
            // prompt selects the conversation in a named script.
            let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
            let count = parsed
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let system = system_text(&parsed);
            let nth = requests.fetch_add(1, Ordering::SeqCst) + 1;
            let (status, response) = response_for(&script, count, &system);
            eprintln!(
                "[salvor-demo-model] request #{nth}: {count} messages -> {}",
                if status == 200 {
                    "scripted"
                } else {
                    "unscripted (500)"
                }
            );
            (status, response)
        } else {
            (
                404,
                json!({ "error": { "type": "not_found", "message": path } }),
            )
        };

        // The demo's watchable pace: hold every model answer for the delay.
        tokio::time::sleep(delay).await;

        let serialized = serde_json::to_vec(&payload).expect("response serializes");
        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             content-type: application/json\r\n\
             content-length: {}\r\n\
             connection: keep-alive\r\n\r\n",
            serialized.len()
        );
        write_half.write_all(head.as_bytes()).await?;
        write_half.write_all(&serialized).await?;
        write_half.flush().await?;
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let settings = settings();
    let script = Arc::new(resolve_script(settings.script_path.as_deref()));
    let requests = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind(("127.0.0.1", settings.port)).await?;
    let port = listener.local_addr()?.port();
    // Printed so a caller that bound port 0 can read the chosen port back.
    println!("salvor-demo-model listening on http://127.0.0.1:{port}");
    eprintln!(
        "[salvor-demo-model] serving {} from {}, {:?} per turn",
        script.summary(),
        settings
            .script_path
            .as_deref()
            .unwrap_or("the built-in demo_script"),
        settings.delay
    );

    loop {
        let (stream, _) = listener.accept().await?;
        let script = script.clone();
        let requests = requests.clone();
        let delay = settings.delay;
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, script, delay, requests).await {
                eprintln!("[salvor-demo-model] connection ended: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Writes `contents` to a fresh temp file and returns the handle; the
    /// path stays valid as long as the handle is alive.
    fn temp_script(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file creates");
        file.write_all(contents.as_bytes())
            .expect("temp file writes");
        file
    }

    /// A minimal end-of-turn response carrying `text`, enough to tell one
    /// scripted answer from another.
    fn text_turn(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    }

    /// The turns of a script that must have loaded as one conversation.
    fn one(script: &Script) -> &Turns {
        match script {
            Script::One(turns) => turns,
            Script::Named(_) => panic!("expected one conversation, got a named script"),
        }
    }

    #[test]
    fn script_flag_serves_at_least_two_turns_from_the_file() {
        let body = json!([
            text_turn("msg_1", "first turn"),
            text_turn("msg_2", "second turn"),
        ]);
        let file = temp_script(&body.to_string());

        let script = load_script(file.path().to_str().expect("temp path is utf8"));
        let turns = one(&script);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].0, 1);
        assert_eq!(turns[1].0, 3);

        let (status, first) = response_for(&script, 1, "");
        assert_eq!(status, 200);
        assert_eq!(first["content"][0]["text"], "first turn");

        let (status, second) = response_for(&script, 3, "");
        assert_eq!(status, 200);
        assert_eq!(second["content"][0]["text"], "second turn");

        // The existing per-turn miss behaviour still holds against a loaded
        // script: an uncovered count is still the 500 error envelope.
        let (status, miss) = response_for(&script, 5, "");
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script");
    }

    #[test]
    fn the_array_form_answers_whatever_the_system_prompt_says() {
        // The array form predates conversation names and must keep ignoring
        // the system prompt entirely: one conversation always answers.
        let file = temp_script(&json!([text_turn("msg_1", "only turn")]).to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        for system in ["", "You are the planner.", "You are the writer."] {
            let (status, response) = response_for(&script, 1, system);
            assert_eq!(status, 200, "the array form answers for `{system}`");
            assert_eq!(response["content"][0]["text"], "only turn");
        }
    }

    #[test]
    fn no_script_setting_serves_the_built_in_script() {
        // The default path (neither `--script` nor `SALVOR_DEMO_MODEL_SCRIPT`
        // set) must stay byte-identical to today: exactly
        // `demo_script::script()`, with no file involved.
        assert_eq!(resolve_script(None), Script::One(demo_script::script()));
    }

    #[test]
    fn named_conversations_answer_the_same_turn_differently() {
        // The multi-agent graph case: every agent node's first model call
        // carries exactly one message, so message count alone cannot tell
        // them apart. The system prompt can, and must.
        let body = json!({
            "planner": [text_turn("msg_p1", "planning"), text_turn("msg_p2", "planned")],
            "writer": [text_turn("msg_w1", "writing")],
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        // Both are turn one, one message each, and they answer differently.
        let (status, planner) = response_for(&script, 1, "You are the planner node.");
        assert_eq!(status, 200);
        assert_eq!(planner["content"][0]["text"], "planning");

        let (status, writer) = response_for(&script, 1, "You are the writer node.");
        assert_eq!(status, 200);
        assert_eq!(writer["content"][0]["text"], "writing");

        // Within the selected conversation, 2i + 1 indexing is unchanged.
        let (status, second) = response_for(&script, 3, "You are the planner node.");
        assert_eq!(status, 200);
        assert_eq!(second["content"][0]["text"], "planned");

        // A turn the selected conversation does not cover is still the
        // existing per-turn miss, not a selection error.
        let (status, miss) = response_for(&script, 3, "You are the writer node.");
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script");
    }

    #[test]
    fn a_system_prompt_sent_as_text_blocks_selects_alike() {
        // The API accepts a system prompt as a block array as well as a
        // string; both must select the same conversation.
        let body = json!({ "writer": [text_turn("msg_w1", "writing")] });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let request = json!({
            "system": [{"type": "text", "text": "You are the writer node."}],
            "messages": [{"role": "user"}]
        });
        let (status, response) = response_for(&script, 1, &system_text(&request));
        assert_eq!(status, 200);
        assert_eq!(response["content"][0]["text"], "writing");

        // And the plain-string form salvor actually sends.
        let request = json!({ "system": "You are the writer node." });
        assert_eq!(system_text(&request), "You are the writer node.");
    }

    #[test]
    fn no_matching_conversation_is_a_500_quoting_the_system_prompt() {
        let body = json!({
            "planner": [text_turn("msg_p1", "planning")],
            "writer": [text_turn("msg_w1", "writing")],
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let (status, miss) = response_for(&script, 1, "You are the reviewer node.");
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script_no_conversation");
        let message = miss["error"]["message"]
            .as_str()
            .expect("the envelope carries a message");
        // The message is diagnosable: it quotes the prompt that matched
        // nothing and lists the names the script does know.
        assert!(message.contains("You are the reviewer node."), "{message}");
        assert!(message.contains("planner"), "{message}");
        assert!(message.contains("writer"), "{message}");
    }

    #[test]
    fn several_matching_conversations_are_a_500_naming_all_of_them() {
        // `writer` is a substring of `ghostwriter`, so a ghostwriter prompt
        // matches both names. That is an authoring mistake, and picking one
        // by luck would leave an example that works by accident.
        let body = json!({
            "writer": [text_turn("msg_w1", "writing")],
            "ghostwriter": [text_turn("msg_g1", "ghostwriting")],
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let (status, miss) = response_for(&script, 1, "You are the ghostwriter node.");
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script_ambiguous_conversation");
        let message = miss["error"]["message"]
            .as_str()
            .expect("the envelope carries a message");
        assert!(message.contains("ghostwriter"), "{message}");
        assert!(message.contains("writer"), "{message}");

        // An unambiguous prompt against the same script still answers.
        let (status, response) = response_for(&script, 1, "You are the writer node.");
        assert_eq!(status, 200);
        assert_eq!(response["content"][0]["text"], "writing");
    }

    #[test]
    fn conversation_selection_does_not_depend_on_key_order() {
        // The same two conversations written in either order select the
        // same way, so nothing rests on the order the parser produced.
        let forward = temp_script(
            &json!({
                "planner": [text_turn("msg_p1", "planning")],
                "writer": [text_turn("msg_w1", "writing")],
            })
            .to_string(),
        );
        let reversed = temp_script(
            &json!({
                "writer": [text_turn("msg_w1", "writing")],
                "planner": [text_turn("msg_p1", "planning")],
            })
            .to_string(),
        );

        let forward = load_script(forward.path().to_str().expect("temp path is utf8"));
        let reversed = load_script(reversed.path().to_str().expect("temp path is utf8"));
        assert_eq!(forward, reversed);

        for system in ["You are the planner node.", "You are the writer node."] {
            assert_eq!(
                response_for(&forward, 1, system),
                response_for(&reversed, 1, system)
            );
        }
    }

    #[test]
    #[should_panic(expected = "could not be read")]
    fn a_script_path_that_cannot_be_read_panics_naming_the_path() {
        load_script("/nonexistent/path/salvor-demo-model-test-script.json");
    }

    #[test]
    #[should_panic(expected = "does not parse as JSON")]
    fn a_script_path_with_malformed_json_panics() {
        let file = temp_script("not json at all {{{");
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "must hold a JSON array or a JSON object")]
    fn a_script_path_that_is_neither_array_nor_object_panics() {
        // A bare scalar is neither shape. (An object is now the named form,
        // so it must not be used as the not-an-array case.)
        let file = temp_script(r#""just a string""#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "empty array")]
    fn a_script_path_with_an_empty_array_panics() {
        let file = temp_script("[]");
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "empty object")]
    fn a_script_path_with_an_empty_object_panics() {
        let file = temp_script("{}");
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "conversation `writer` must hold a JSON array")]
    fn a_named_conversation_that_is_not_an_array_panics() {
        let file = temp_script(r#"{"writer": "not an array"}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "conversation `writer` holds an empty array")]
    fn a_named_conversation_that_is_empty_panics() {
        let file = temp_script(r#"{"writer": []}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "element 0 is not a JSON object")]
    fn a_non_object_element_panics() {
        let file = temp_script("[1, 2]");
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    fn a_long_system_prompt_is_quoted_as_a_short_single_line_head() {
        // Multi-byte characters must not panic a slice, and the head must
        // stay one line so the error reads.
        let long = format!("{}\nsecond line", "e\u{301}".repeat(200));
        let head = system_head(&long);
        assert!(head.ends_with("..."), "{head}");
        assert!(!head.contains('\n'), "{head}");
        assert_eq!(head.chars().count(), SYSTEM_HEAD_CHARS + 3);

        // A short prompt is quoted whole, with newlines flattened.
        assert_eq!(system_head("one\ntwo"), "one two");
    }
}
