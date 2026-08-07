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
//! **A JSON object**: several named conversations, mapping a name to a
//! conversation. This is the shape a graph needs. Selection by message count
//! alone cannot serve a graph, because every agent node is its own run with
//! its own message list, so all of them make their first model call carrying
//! exactly one message and would collide on element 0. Naming the
//! conversations and selecting on the system prompt separates them, since
//! that is the thing agent nodes genuinely differ on.
//!
//! A conversation is written one of two ways:
//!
//! - **an array of responses**, exactly the form above. The conversation is
//!   selected by its NAME appearing in the request's system prompt.
//! - **an object** `{"when": "<needle>", "turns": [ ... ]}`. `turns` is that
//!   same array; `when` is a substring that must appear ANYWHERE IN THE
//!   REQUEST BODY for this conversation to answer, and it REPLACES the name
//!   as the selection rule, leaving the name a label the error messages use.
//!
//! # Why a request-body needle exists
//!
//! A `fold` node runs one body agent several times, and every pass is a fresh
//! conversation carrying exactly one message under the SAME system prompt, so
//! neither the message count nor the system prompt can tell one pass from the
//! next. What does tell them apart is the pass input: pass 0 folds over the
//! graph's routed input and every later pass folds over the previous pass's
//! output, and both land verbatim in the request body. A `when` needle picks
//! a pass out by a marker its own input carries, which is the same rule the
//! engine's own fold tests select scripted answers by.
//!
//! # Selecting a named conversation
//!
//! A conversation with no `when` is selected when its name appears as a
//! **substring** of the request's top-level `system` string (a system prompt
//! sent as an array of text blocks is joined before matching, so either wire
//! form selects alike). A conversation WITH a `when` is selected when that
//! needle appears as a substring of the raw request body. Selection is
//! deterministic and never depends on the order the names happen to appear in
//! the file: exactly one conversation must match, whichever rule matched it.
//!
//! - Exactly one match: that conversation answers, indexed by `2i + 1`.
//! - No match: a `500` error envelope saying no conversation matched,
//!   quoting a short head of the system prompt so the mismatch is
//!   diagnosable.
//! - More than one match: a `500` error envelope naming every conversation
//!   that matched. An ambiguous script is an authoring mistake, and quietly
//!   picking one would leave an example that works by luck.
//!
//! # Failing loudly
//!
//! An unreadable path, JSON that fails to parse, a top-level value that is
//! neither an array nor an object, a named conversation whose value is
//! neither an array nor a `turns` object, a conversation object carrying a
//! key that is not `when` or `turns`, a `when` that is not a non-empty
//! string, an array holding a non-object element, an empty array, or an
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

/// The flags this binary recognizes that take a value, `--port`, `--delay-ms`,
/// and `--script`. Shared between [`resolve_u64`]/[`resolve_string`]'s lookup
/// and [`unknown_flag`]'s check, so the two cannot drift apart.
const VALUE_FLAGS: [&str; 3] = ["--port", "--delay-ms", "--script"];

/// The two spellings of the help flag, checked first in `main` so `--help`
/// exits before any port binds or environment variable is read.
const HELP_FLAGS: [&str; 2] = ["--help", "-h"];

/// The usage text `--help`/`-h` prints, and an unknown flag's refusal
/// accompanies. Names every flag, every environment variable, and the shape
/// of a `--script` file, including the substring selection rule: nothing else
/// in the binary's own output tells an author how a named conversation gets
/// picked, so a tester debugging a graph script has nowhere else to learn it.
fn usage() -> String {
    format!(
        "salvor-demo-model: a hermetic scripted model server for the demo GIF and offline runs\n\
         \n\
         USAGE:\n\
         \x20   salvor-demo-model [--port <PORT>] [--delay-ms <MS>] [--script <PATH>]\n\
         \x20   salvor-demo-model --help\n\
         \n\
         FLAGS:\n\
         \x20   --port <PORT>        Port to listen on (default {DEFAULT_PORT}; 0 picks a free\n\
         \x20                        port and prints the one chosen)\n\
         \x20   --delay-ms <MS>      Per turn response delay in milliseconds (default\n\
         \x20                        {DEFAULT_DELAY_MS})\n\
         \x20   --script <PATH>      Path to a script file to serve instead of the built in\n\
         \x20                        demo script (see SCRIPT FILE below)\n\
         \x20   -h, --help           Print this usage and exit\n\
         \n\
         ENVIRONMENT:\n\
         \x20   SALVOR_DEMO_MODEL_PORT        Same as --port; the flag wins when both are set\n\
         \x20   SALVOR_DEMO_MODEL_DELAY_MS    Same as --delay-ms; the flag wins when both are\n\
         \x20                                 set\n\
         \x20   SALVOR_DEMO_MODEL_SCRIPT      Same as --script; the flag wins when both are set\n\
         \n\
         SCRIPT FILE:\n\
         \x20   A --script file holds a JSON array or a JSON object. An array is one\n\
         \x20   conversation, served no matter what the request's system prompt says. An\n\
         \x20   object maps a name to a conversation in that same array form; this named form\n\
         \x20   is what a graph needs, since every agent node's first model call carries\n\
         \x20   exactly one message, so message count alone cannot tell the nodes apart.\n\
         \n\
         \x20   A request selects the named conversation whose name IS A SUBSTRING OF THE\n\
         \x20   REQUEST'S SYSTEM PROMPT. Exactly one name must match: no match, or more than\n\
         \x20   one, answers with a 500 naming the problem rather than guessing.\n\
         \n\
         \x20   A conversation may instead be written as an object,\n\
         \x20   {{\"when\": \"<needle>\", \"turns\": [ ... ]}}. Then it is selected when <needle>\n\
         \x20   IS A SUBSTRING OF THE WHOLE REQUEST BODY, and its name is only a label. That\n\
         \x20   is what a fold needs: every pass of one body agent carries one message under\n\
         \x20   the same system prompt, and only the pass input in the body tells them apart.\n"
    )
}

/// The first command line argument that is neither a recognized flag nor the
/// value slot right after one, scanned left to right; `None` if every
/// argument is accounted for. The caller must already have handled
/// `--help`/`-h` (this function does not recognize them, so an unhandled help
/// flag would itself come back as unknown).
fn unknown_flag(args: &[String]) -> Option<&str> {
    let mut expect_value = false;
    for arg in args {
        if expect_value {
            expect_value = false;
            continue;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            expect_value = true;
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

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

/// One named conversation and the rule a request selects it by.
#[derive(Debug, PartialEq)]
struct Conversation {
    /// The name it was written under. The selection rule when `when` is
    /// absent, and a label the error messages quote either way.
    name: String,
    /// A substring that must appear anywhere in the raw request body for this
    /// conversation to answer. `Some` REPLACES the name as the selection rule;
    /// `None` keeps it.
    when: Option<String>,
    /// Its turns, keyed by message count.
    turns: Turns,
}

impl Conversation {
    /// Whether this conversation answers a request carrying `system` as its
    /// system prompt and `body` as its raw body.
    fn matches(&self, system: &str, body: &str) -> bool {
        match &self.when {
            Some(needle) => body.contains(needle.as_str()),
            None => system.contains(self.name.as_str()),
        }
    }

    /// How this conversation names itself in an error message: its name, and
    /// the needle when it has one, since a `when` conversation's name says
    /// nothing about why it did or did not match.
    fn label(&self) -> String {
        match &self.when {
            Some(needle) => format!("{} (when `{needle}`)", self.name),
            None => self.name.clone(),
        }
    }
}

/// A loaded script: one conversation, or several selected per request.
#[derive(Debug, PartialEq)]
enum Script {
    /// One conversation, served whatever the system prompt says. The JSON
    /// array file form, and what the built-in `demo_script` is.
    One(Turns),
    /// Named conversations, each selected by its name against the request's
    /// system prompt or by its `when` needle against the whole request body.
    /// Held as a sorted vector rather than a map so error messages list names
    /// in a fixed order and nothing depends on the order the names happened to
    /// appear in the file.
    Named(Vec<Conversation>),
}

impl Script {
    /// A one-line description of what was loaded, for the startup log.
    fn summary(&self) -> String {
        match self {
            Script::One(turns) => format!("{} scripted turns", turns.len()),
            Script::Named(conversations) => {
                let names: Vec<String> = conversations
                    .iter()
                    .map(|conversation| {
                        format!(
                            "{} ({} turns)",
                            conversation.label(),
                            conversation.turns.len()
                        )
                    })
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

/// The keys an object-form conversation may carry. Anything else is an
/// authoring mistake: a misspelled `when` that was quietly ignored would leave
/// a conversation selecting on its name and an example working by luck.
const CONVERSATION_KEYS: [&str; 2] = ["turns", "when"];

/// Turns one named entry of a script file into a [`Conversation`]: the array
/// form (selected by name against the system prompt) or the object form
/// (`turns` plus an optional `when` needle selected against the request body).
/// Panics naming `path` and `name` on every way the entry can be wrong.
fn conversation_from(path: &str, name: String, entry: Value) -> Conversation {
    match entry {
        Value::Array(entries) => {
            let turns = turns_from(path, Some(&name), entries);
            Conversation {
                name,
                when: None,
                turns,
            }
        }
        Value::Object(mut map) => {
            if let Some(unknown) = map
                .keys()
                .find(|key| !CONVERSATION_KEYS.contains(&key.as_str()))
            {
                panic!(
                    "--script path `{path}` conversation `{name}` carries unknown key `{unknown}`; a conversation object holds `turns` and an optional `when`"
                );
            }
            let when = match map.remove("when") {
                None => None,
                Some(Value::String(needle)) if !needle.is_empty() => Some(needle),
                Some(Value::String(_)) => panic!(
                    "--script path `{path}` conversation `{name}` holds an empty `when`; a needle every request contains selects nothing"
                ),
                Some(_) => panic!(
                    "--script path `{path}` conversation `{name}` must hold `when` as a string"
                ),
            };
            let Some(Value::Array(entries)) = map.remove("turns") else {
                panic!(
                    "--script path `{path}` conversation `{name}` must hold `turns` as a JSON array of responses"
                );
            };
            let turns = turns_from(path, Some(&name), entries);
            Conversation { name, when, turns }
        }
        _ => panic!(
            "--script path `{path}` conversation `{name}` must hold a JSON array of responses, or an object carrying `turns`"
        ),
    }
}

/// Loads a `--script` file: either a JSON array holding one conversation, or
/// a JSON object mapping names to conversations (see the module doc for the
/// full format and the selection rule). Every element is a full Messages API
/// response body, read at full fidelity. Panics with a message naming `path`
/// and the specific problem on every way the file can be wrong: unreadable,
/// not JSON, neither an array nor an object, a named conversation that is
/// neither an array nor a `turns` object, an element that is not a JSON
/// object, an empty array, or an empty object, since a script that can never
/// answer a turn is a mistake, not a configuration.
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
            let mut conversations: Vec<Conversation> = map
                .into_iter()
                .map(|(name, entry)| conversation_from(path, name, entry))
                .collect();
            // Sorted so an ambiguity message lists names in a fixed order,
            // whatever order the parser produced them in.
            conversations.sort_by(|left, right| left.name.cmp(&right.name));
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

/// Picks the conversation that answers a request carrying `system` as its
/// system prompt and `body` as its raw body, or the error envelope explaining
/// why none can. A one-conversation script always answers; a named script
/// requires exactly one conversation to match its own rule (a name in the
/// system prompt, or a `when` needle in the body), since zero is a script that
/// does not cover this call and more than one is an authoring mistake that
/// must not be resolved by luck.
fn conversation_for<'a>(
    script: &'a Script,
    system: &str,
    body: &str,
) -> Result<&'a Turns, (u16, Value)> {
    let conversations = match script {
        Script::One(turns) => return Ok(turns),
        Script::Named(conversations) => conversations,
    };

    let matched: Vec<&'a Conversation> = conversations
        .iter()
        .filter(|conversation| conversation.matches(system, body))
        .collect();

    if matched.len() == 1 {
        return Ok(&matched[0].turns);
    }

    if matched.is_empty() {
        let known: Vec<String> = conversations.iter().map(Conversation::label).collect();
        return Err(error_envelope(
            "demo_script_no_conversation",
            format!(
                "no conversation matched the request whose system prompt is `{}`; the script names {}",
                system_head(system),
                known.join(", ")
            ),
        ));
    }

    let names: Vec<String> = matched.iter().map(|matched| matched.label()).collect();
    Err(error_envelope(
        "demo_script_ambiguous_conversation",
        format!(
            "{} conversations matched the same request, whose system prompt is `{}` ({}); exactly one must match",
            names.len(),
            system_head(system),
            names.join(", ")
        ),
    ))
}

/// The scripted response for a request whose body holds `count` messages,
/// carries `system` as its system prompt, and reads as `body` on the wire.
/// Mirrors the wiremock model in the tests: a hit is `200` with the response,
/// a miss is `500` with an error envelope. Conversation selection happens
/// first, then the existing per-turn lookup within the selected conversation,
/// unchanged.
fn response_for(script: &Script, count: usize, system: &str, body: &str) -> (u16, Value) {
    let turns = match conversation_for(script, system, body) {
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
            // The raw body is what a `when` needle is matched against: a pass
            // input reaches the model verbatim inside it, which is the only
            // thing that tells two passes of one fold body apart.
            let raw = String::from_utf8_lossy(&body);
            let nth = requests.fetch_add(1, Ordering::SeqCst) + 1;
            let (status, response) = response_for(&script, count, &system, &raw);
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
    // Checked before anything else touches a port or an environment
    // variable, so `--help` (in any position) always exits 0 promptly.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| HELP_FLAGS.contains(&arg.as_str())) {
        print!("{}", usage());
        return Ok(());
    }
    if let Some(bad) = unknown_flag(&args) {
        eprintln!("salvor-demo-model: unrecognized argument `{bad}`\n");
        eprint!("{}", usage());
        std::process::exit(2);
    }

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

        let (status, first) = response_for(&script, 1, "", "");
        assert_eq!(status, 200);
        assert_eq!(first["content"][0]["text"], "first turn");

        let (status, second) = response_for(&script, 3, "", "");
        assert_eq!(status, 200);
        assert_eq!(second["content"][0]["text"], "second turn");

        // The existing per-turn miss behaviour still holds against a loaded
        // script: an uncovered count is still the 500 error envelope.
        let (status, miss) = response_for(&script, 5, "", "");
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
            let (status, response) = response_for(&script, 1, system, "");
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
        let (status, planner) = response_for(&script, 1, "You are the planner node.", "");
        assert_eq!(status, 200);
        assert_eq!(planner["content"][0]["text"], "planning");

        let (status, writer) = response_for(&script, 1, "You are the writer node.", "");
        assert_eq!(status, 200);
        assert_eq!(writer["content"][0]["text"], "writing");

        // Within the selected conversation, 2i + 1 indexing is unchanged.
        let (status, second) = response_for(&script, 3, "You are the planner node.", "");
        assert_eq!(status, 200);
        assert_eq!(second["content"][0]["text"], "planned");

        // A turn the selected conversation does not cover is still the
        // existing per-turn miss, not a selection error.
        let (status, miss) = response_for(&script, 3, "You are the writer node.", "");
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
        let (status, response) = response_for(&script, 1, &system_text(&request), "");
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

        let (status, miss) = response_for(&script, 1, "You are the reviewer node.", "");
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

        let (status, miss) = response_for(&script, 1, "You are the ghostwriter node.", "");
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script_ambiguous_conversation");
        let message = miss["error"]["message"]
            .as_str()
            .expect("the envelope carries a message");
        assert!(message.contains("ghostwriter"), "{message}");
        assert!(message.contains("writer"), "{message}");

        // An unambiguous prompt against the same script still answers.
        let (status, response) = response_for(&script, 1, "You are the writer node.", "");
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
                response_for(&forward, 1, system, ""),
                response_for(&reversed, 1, system, "")
            );
        }
    }

    /// A request body as the wire carries it: a system prompt and one user
    /// message. The `when` form matches against exactly this text, so the
    /// tests below build it rather than asserting against a fragment.
    fn request_body(system: &str, user: &str) -> String {
        json!({
            "system": system,
            "messages": [{"role": "user", "content": user}]
        })
        .to_string()
    }

    #[test]
    fn a_when_needle_selects_on_the_request_body_not_the_system_prompt() {
        // The conversation's name appears nowhere in the system prompt, so
        // the name rule could never have selected it. The needle in the user
        // message does.
        let body = json!({
            "opening": {"when": "ADJ-7741", "turns": [text_turn("msg_1", "first draft")]},
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let system = "You are the tailor on a payroll desk.";
        let request = request_body(system, r#"{"notice_id":"ADJ-7741"}"#);
        let (status, response) = response_for(&script, 1, system, &request);
        assert_eq!(status, 200);
        assert_eq!(response["content"][0]["text"], "first draft");

        // The same system prompt with no needle in the body selects nothing.
        let other = request_body(system, r#"{"notice_id":"ADJ-9002"}"#);
        let (status, miss) = response_for(&script, 1, system, &other);
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script_no_conversation");
    }

    #[test]
    fn two_when_conversations_answer_a_folds_two_passes_apart() {
        // The case the needle exists for. Both passes of one fold body are
        // turn one of a fresh conversation under the SAME system prompt, so
        // neither the message count nor the prompt tells them apart. The pass
        // input in the body does: pass 0 folds over the routed input, pass 1
        // over what pass 0 answered.
        let body = json!({
            "pass-0": {"when": "ADJ-7741", "turns": [text_turn("msg_1", "rev 1, score 0.55")]},
            "pass-1": {"when": "rev A1", "turns": [text_turn("msg_2", "rev 2, score 0.85")]},
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let system = "You are the tailor on a payroll desk.";
        let first = request_body(system, r#"{"notice_id":"ADJ-7741"}"#);
        let (status, opening) = response_for(&script, 1, system, &first);
        assert_eq!(status, 200);
        assert_eq!(opening["content"][0]["text"], "rev 1, score 0.55");

        let second = request_body(system, r#"{"draft":"... rev A1 ...","score":0.55}"#);
        let (status, revised) = response_for(&script, 1, system, &second);
        assert_eq!(status, 200);
        assert_eq!(revised["content"][0]["text"], "rev 2, score 0.85");
    }

    #[test]
    fn two_needles_in_one_body_are_the_ambiguity_500_naming_both() {
        // Two conversations whose needles both appear is the same authoring
        // mistake two matching names are, and answers the same way.
        let body = json!({
            "pass-0": {"when": "ADJ-7741", "turns": [text_turn("msg_1", "first")]},
            "pass-1": {"when": "rev A1", "turns": [text_turn("msg_2", "second")]},
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let system = "You are the tailor.";
        let both = request_body(system, r#"{"notice_id":"ADJ-7741","draft":"rev A1"}"#);
        let (status, miss) = response_for(&script, 1, system, &both);
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script_ambiguous_conversation");
        let message = miss["error"]["message"]
            .as_str()
            .expect("the envelope carries a message");
        // Both the labels and the needles, since a `when` conversation's name
        // says nothing about why it matched.
        assert!(message.contains("ADJ-7741"), "{message}");
        assert!(message.contains("rev A1"), "{message}");
    }

    #[test]
    fn a_name_selected_and_a_needle_selected_conversation_share_one_script() {
        // The two rules coexist: one graph can hold an ordinary agent node
        // selected by its system prompt and a fold body selected by its pass
        // input, in the same file.
        let body = json!({
            "notify": [text_turn("msg_n1", "notified")],
            "opening": {"when": "ADJ-7741", "turns": [text_turn("msg_1", "first draft")]},
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let notify = "You are the notify agent.";
        let (status, notified) = response_for(&script, 1, notify, &request_body(notify, "{}"));
        assert_eq!(status, 200);
        assert_eq!(notified["content"][0]["text"], "notified");

        let tailor = "You are the tailor.";
        let request = request_body(tailor, r#"{"notice_id":"ADJ-7741"}"#);
        let (status, drafted) = response_for(&script, 1, tailor, &request);
        assert_eq!(status, 200);
        assert_eq!(drafted["content"][0]["text"], "first draft");
    }

    #[test]
    fn a_when_conversation_still_indexes_its_turns_by_message_count() {
        // The needle chooses the conversation; within it nothing changes, so a
        // body agent that takes a tool call before answering still walks 2i+1.
        let body = json!({
            "opening": {
                "when": "ADJ-7741",
                "turns": [text_turn("msg_1", "first turn"), text_turn("msg_2", "second turn")],
            },
        });
        let file = temp_script(&body.to_string());
        let script = load_script(file.path().to_str().expect("temp path is utf8"));

        let system = "You are the tailor.";
        let request = request_body(system, r#"{"notice_id":"ADJ-7741"}"#);
        let (status, first) = response_for(&script, 1, system, &request);
        assert_eq!(status, 200);
        assert_eq!(first["content"][0]["text"], "first turn");

        let (status, second) = response_for(&script, 3, system, &request);
        assert_eq!(status, 200);
        assert_eq!(second["content"][0]["text"], "second turn");

        let (status, miss) = response_for(&script, 5, system, &request);
        assert_eq!(status, 500);
        assert_eq!(miss["error"]["type"], "demo_script");
    }

    #[test]
    #[should_panic(expected = "carries unknown key `whn`")]
    fn a_conversation_object_with_an_unknown_key_panics() {
        // A misspelled `when` that was ignored would leave the conversation
        // selecting on its name and an example passing by luck.
        let file = temp_script(r#"{"opening": {"whn": "x", "turns": []}}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "must hold `when` as a string")]
    fn a_conversation_whose_when_is_not_a_string_panics() {
        let file = temp_script(r#"{"opening": {"when": 7, "turns": []}}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "holds an empty `when`")]
    fn a_conversation_with_an_empty_when_panics() {
        // The empty string is a substring of every body, so this conversation
        // would match everything and make any second conversation ambiguous.
        let file = temp_script(r#"{"opening": {"when": "", "turns": []}}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "must hold `turns` as a JSON array")]
    fn a_conversation_object_without_turns_panics() {
        let file = temp_script(r#"{"opening": {"when": "x"}}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
    }

    #[test]
    #[should_panic(expected = "conversation `opening` holds an empty array")]
    fn a_conversation_object_with_no_turns_panics() {
        let file = temp_script(r#"{"opening": {"when": "x", "turns": []}}"#);
        load_script(file.path().to_str().expect("temp path is utf8"));
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

    #[test]
    fn usage_names_every_flag_every_env_var_and_the_substring_rule() {
        let text = usage();
        for flag in ["--port", "--delay-ms", "--script", "--help", "-h"] {
            assert!(text.contains(flag), "usage is missing `{flag}`: {text}");
        }
        for var in [
            "SALVOR_DEMO_MODEL_PORT",
            "SALVOR_DEMO_MODEL_DELAY_MS",
            "SALVOR_DEMO_MODEL_SCRIPT",
        ] {
            assert!(text.contains(var), "usage is missing `{var}`: {text}");
        }
        // The named-conversation form and the rule a tester could not
        // discover anywhere else: a name matches as a substring of the
        // system prompt.
        assert!(text.contains("SUBSTRING OF THE"), "{text}");
        assert!(text.contains("SYSTEM PROMPT"), "{text}");
        // And the second rule, which a fold author has nowhere else to learn:
        // a `when` needle is matched against the whole request body.
        assert!(text.contains("\"when\""), "{text}");
        assert!(text.contains("WHOLE REQUEST BODY"), "{text}");
    }

    #[test]
    fn every_recognized_flag_and_its_value_is_not_unknown() {
        let args: Vec<String> = [
            "--port",
            "18940",
            "--delay-ms",
            "5",
            "--script",
            "/tmp/script.json",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(unknown_flag(&args), None);
    }

    #[test]
    fn an_unrecognized_flag_is_reported_by_name() {
        let args: Vec<String> = ["--port", "18940", "--bogus"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(unknown_flag(&args), Some("--bogus"));
    }

    #[test]
    fn a_bare_positional_argument_is_unknown() {
        let args: Vec<String> = ["surprise".to_owned()].into();
        assert_eq!(unknown_flag(&args), Some("surprise"));
    }

    #[test]
    fn no_arguments_is_not_unknown() {
        assert_eq!(unknown_flag(&[]), None);
    }
}
