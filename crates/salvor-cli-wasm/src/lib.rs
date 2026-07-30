//! A thin wasm-bindgen wrapper over the pure `salvor-cli-core` crate.
//!
//! A terminal drawn in a browser needs three things a real shell gets for
//! free: something to turn a typed line into a command, something to answer
//! `--help`, and something to draw the output. This crate exposes exactly
//! those, calling `salvor_cli_core::cli` and `salvor_cli_core::render`, the
//! same clap tree and the same renderer the `salvor` binary uses, so what the
//! page parses, refuses, and prints cannot drift from what the terminal does.
//!
//! # No stdout
//!
//! There is no standard output in wasm, so nothing here writes to a stream:
//! every surface returns a `String` and the caller decides where it goes.
//! clap's help comes back from [`clap::Command::render_long_help`], and its
//! refusals from [`clap::Error::to_string`] and [`clap::Error::render`], which
//! is why none of clap's own stream-writing helpers are reachable from this
//! crate. A grep for the printing macros and the standard streams over
//! `src/` finds nothing, and that is a property to keep.
//!
//! # Plain and styled
//!
//! Every text surface here comes in two forms, because a browser terminal
//! emulator wants the escape codes and a plain `<pre>` does not. Which form a
//! function returns is in its name, but the two halves get there by opposite
//! routes, so the names read differently:
//!
//! - Help and refusals are unstyled by default. Plain is what `.to_string()`
//!   yields on a [`clap::Error`] or a `clap::builder::StyledStr`: both strip
//!   styling regardless of the command's `ColorChoice`. The `*_ansi` variants
//!   ([`render_help_to_ansi_string`], and the `ansi` field of a parse message)
//!   call `.ansi()` explicitly, which is the only way real escape codes come
//!   out of clap.
//! - The list table is styled by default, because
//!   [`salvor_cli_core::render::list_table`] styles its status column
//!   unconditionally: stripping is the writer's job in the real CLI. So
//!   [`render_list_to_string`] is the styled form and
//!   [`render_list_to_plain_string`] is the stripped one, stripped with
//!   `anstream::adapter::strip_str`, the same pass `anstream` makes when the
//!   real CLI's stdout is a pipe. The plain form here is plain the way
//!   `salvor list | cat` is plain.
//!
//! # The command shape
//!
//! `salvor_cli_core::cli::Cli` and its subcommand tree derive no `Serialize`
//! (they are clap derive types), so this crate mirrors them into small
//! serializable DTOs whose wire shape is a stable contract for the page. The
//! mirror is exhaustive by construction: [`From<&Command>`] matches every verb,
//! so a verb added to `salvor-cli-core` fails this crate's build rather than
//! silently vanishing from the browser's view of the CLI.
//!
//! # Native and wasm from one source
//!
//! The parse and render cores ([`parse_argv_to_json`], [`render_help_to_string`],
//! [`render_list_to_string`], and their variants) are ordinary Rust that builds
//! for any target. `cargo build/test --workspace` compiles and tests them
//! natively with no wasm toolchain; wasm-pack compiles the very same code to
//! `wasm32-unknown-unknown` behind the bindings below. The same-render proof in
//! `tests/same_render.rs` exists to show that what crosses the wasm boundary is
//! byte for byte what `salvor-cli-core` produces on its own.

#![warn(missing_docs)]

use salvor_cli_core::cli::{
    AbandonArgs, AgentCommand, AgentHashArgs, AgentValidateArgs, BuildArgs, Cli, Command,
    CompletionsArgs, ForkArgs, GraphCommand, GraphEditArgs, GraphRunArgs, GraphValidateArgs,
    HistoryArgs, ListArgs, ReplayArgs, ResolveArgs, ResumeArgs, RunArgs, ServeArgs,
};
use salvor_cli_core::render;
use salvor_replay::RunSummary;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

/// The error a call into this crate can return: bad input from the caller,
/// never a failure of the parser or the renderer themselves.
///
/// A command line clap refuses is NOT one of these. A refusal is an answer, not
/// an error: it comes back inside the parse envelope as clap's own text, the
/// same way a shell shows it.
#[derive(Debug)]
pub enum CliError {
    /// `argv_json` was not a JSON array of strings.
    Argv(serde_json::Error),
    /// `rows_json` was not a JSON array of list rows.
    Rows(serde_json::Error),
    /// The help path named a subcommand this build does not have. `path` is the
    /// full path as given, `unknown` the segment that did not resolve.
    UnknownSubcommand {
        /// The path the caller asked for, e.g. `graph valdiate`.
        path: String,
        /// The first segment of that path with no matching subcommand.
        unknown: String,
    },
    /// The parse envelope failed to serialize (which cannot happen for the DTOs
    /// below, but is surfaced rather than unwrapped).
    Serialize(serde_json::Error),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::Argv(e) => write!(f, "argv is not a JSON array of strings: {e}"),
            CliError::Rows(e) => write!(f, "rows are not a JSON array of list rows: {e}"),
            CliError::UnknownSubcommand { path, unknown } => {
                write!(f, "no subcommand `{unknown}` in the path `{path}`")
            }
            CliError::Serialize(e) => write!(f, "the parse result failed to serialize: {e}"),
        }
    }
}

impl std::error::Error for CliError {}

/// What a parse produced: either a command, or clap's own text.
///
/// Exactly one of `command` and `message` is present. `ok` says which, so a
/// caller can branch on one boolean instead of probing for keys.
#[derive(Serialize)]
struct ParseEnvelope {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<CliDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<ClapMessage>,
}

/// Text clap produced instead of a parse: a refusal, or the help or version it
/// displays when asked for one.
///
/// `plain` is clap's real text with no styling, byte for byte what
/// `clap::Error::to_string()` yields, custom `did you mean` tips included.
/// `ansi` is the same text with escape codes, from `render().ansi()`.
#[derive(Serialize)]
struct ClapMessage {
    /// clap's own name for what happened, e.g. `InvalidValue`, `DisplayHelp`.
    kind: String,
    /// True for a refusal, false when clap is displaying help or a version.
    /// Mirrors which stream a shell would write this to.
    is_error: bool,
    /// The process exit code a shell would use: 0 for help or version, 2 for a
    /// refusal.
    exit_code: i32,
    /// clap's text, unstyled.
    plain: String,
    /// clap's text, with ANSI styling.
    ansi: String,
}

/// The parsed command line: the one global option, and the verb.
#[derive(Serialize)]
struct CliDto {
    store: String,
    command: CommandDto,
}

/// The verb, `verb`-tagged so a caller can switch on `command.verb`.
#[derive(Serialize)]
#[serde(tag = "verb", rename_all = "kebab-case")]
enum CommandDto {
    Run {
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fixture: Option<String>,
    },
    Resume {
        run_id: String,
        agents: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        graph: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
    Fork {
        run_id: String,
        from_node: String,
        graph: String,
        agents: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        acknowledge_writes: Option<String>,
        dry_run: bool,
    },
    Resolve {
        run_id: String,
        output: String,
    },
    Abandon {
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    List {
        status: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Completions {
        shell: String,
    },
    History {
        run_id: String,
        json: bool,
    },
    Replay {
        run_id: String,
        dry_run: bool,
    },
    Serve {
        bind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        auth_token: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        kill: Option<String>,
        dev: bool,
        demo_tools: bool,
        client_tools: Vec<String>,
    },
    Build {
        install: bool,
    },
    Agent {
        command: AgentCommandDto,
    },
    Graph {
        command: GraphCommandDto,
    },
}

/// The verbs under `salvor agent`, tagged like their parent.
#[derive(Serialize)]
#[serde(tag = "agent_verb", rename_all = "kebab-case")]
enum AgentCommandDto {
    Hash { agents: Vec<String> },
    Validate { agents: Vec<String> },
}

/// The verbs under `salvor graph`, tagged like their parent.
#[derive(Serialize)]
#[serde(tag = "graph_verb", rename_all = "kebab-case")]
enum GraphCommandDto {
    Edit {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        script: Option<String>,
    },
    Validate {
        path: String,
    },
    Schema,
    Run {
        graph: String,
        input: String,
        agents: Vec<String>,
        labels: Vec<String>,
    },
}

/// A path as the browser should read it. `PathBuf`'s own `Serialize` refuses a
/// non-UTF-8 path; argv arrives here as JSON strings, which are always UTF-8,
/// so lossy display is exact and cannot fail.
fn path(value: &std::path::Path) -> String {
    value.display().to_string()
}

/// Paths, in order.
fn paths(values: &[std::path::PathBuf]) -> Vec<String> {
    values.iter().map(|p| path(p)).collect()
}

impl From<&Cli> for CliDto {
    fn from(cli: &Cli) -> Self {
        CliDto {
            store: path(&cli.store),
            command: (&cli.command).into(),
        }
    }
}

impl From<&Command> for CommandDto {
    fn from(command: &Command) -> Self {
        match command {
            Command::Run(RunArgs {
                agent,
                input,
                fixture,
            }) => CommandDto::Run {
                agent: agent.as_deref().map(path),
                input: input.clone(),
                fixture: fixture.as_deref().map(path),
            },
            Command::Resume(ResumeArgs {
                run_id,
                agents,
                graph,
                input,
            }) => CommandDto::Resume {
                run_id: run_id.clone(),
                agents: paths(agents),
                graph: graph.as_deref().map(path),
                input: input.clone(),
            },
            Command::Fork(ForkArgs {
                run_id,
                from_node,
                graph,
                agents,
                acknowledge_writes,
                dry_run,
            }) => CommandDto::Fork {
                run_id: run_id.clone(),
                from_node: from_node.clone(),
                graph: path(graph),
                agents: paths(agents),
                acknowledge_writes: acknowledge_writes.clone(),
                dry_run: *dry_run,
            },
            Command::Resolve(ResolveArgs { run_id, output }) => CommandDto::Resolve {
                run_id: run_id.clone(),
                output: output.clone(),
            },
            Command::Abandon(AbandonArgs { run_id, reason }) => CommandDto::Abandon {
                run_id: run_id.clone(),
                reason: reason.clone(),
            },
            Command::List(ListArgs {
                status,
                group,
                agent,
                limit,
            }) => CommandDto::List {
                status: status.clone(),
                group: group.clone(),
                agent: agent.clone(),
                limit: *limit,
            },
            Command::Completions(CompletionsArgs { shell }) => CommandDto::Completions {
                shell: shell.to_string(),
            },
            Command::History(HistoryArgs { run_id, json }) => CommandDto::History {
                run_id: run_id.clone(),
                json: *json,
            },
            Command::Replay(ReplayArgs { run_id, dry_run }) => CommandDto::Replay {
                run_id: run_id.clone(),
                dry_run: *dry_run,
            },
            Command::Serve(ServeArgs {
                bind,
                auth_token,
                kill,
                dev,
                demo_tools,
                client_tools,
            }) => CommandDto::Serve {
                bind: bind.clone(),
                auth_token: auth_token.clone(),
                kill: kill.clone(),
                dev: *dev,
                demo_tools: *demo_tools,
                client_tools: paths(client_tools),
            },
            Command::Build(BuildArgs { install }) => CommandDto::Build { install: *install },
            Command::Agent { command } => CommandDto::Agent {
                command: command.into(),
            },
            Command::Graph { command } => CommandDto::Graph {
                command: command.into(),
            },
        }
    }
}

impl From<&AgentCommand> for AgentCommandDto {
    fn from(command: &AgentCommand) -> Self {
        match command {
            AgentCommand::Hash(AgentHashArgs { agents }) => AgentCommandDto::Hash {
                agents: paths(agents),
            },
            AgentCommand::Validate(AgentValidateArgs { agents }) => AgentCommandDto::Validate {
                agents: paths(agents),
            },
        }
    }
}

impl From<&GraphCommand> for GraphCommandDto {
    fn from(command: &GraphCommand) -> Self {
        match command {
            GraphCommand::Edit(GraphEditArgs { path: file, script }) => GraphCommandDto::Edit {
                path: file.as_deref().map(path),
                script: script.as_deref().map(path),
            },
            GraphCommand::Validate(GraphValidateArgs { path: file }) => {
                GraphCommandDto::Validate { path: path(file) }
            }
            GraphCommand::Schema => GraphCommandDto::Schema,
            GraphCommand::Run(GraphRunArgs {
                graph,
                input,
                agents,
                labels,
            }) => GraphCommandDto::Run {
                graph: path(graph),
                input: input.clone(),
                agents: paths(agents),
                labels: labels.clone(),
            },
        }
    }
}

/// One row of the `list` table: a store summary plus the status folded from
/// that run's log.
///
/// Flattened on the wire, so a row is the JSON `RunSummary` the store and the
/// control plane already serialize with one extra `status` key, rather than a
/// nested shape a caller would have to build by hand.
#[derive(Deserialize)]
struct ListRow {
    #[serde(flatten)]
    summary: RunSummary,
    status: String,
}

/// Parses an argv into the command it names, returned as canonical JSON.
///
/// This is the parse core, callable from any target. `argv_json` is a JSON array
/// of strings: the FULL argv, program name at index 0, exactly as a shell hands
/// it over (`["salvor", "list", "--group", "waiting"]`).
///
/// The returned JSON is a parse envelope. On a successful parse it is
/// `{"ok":true,"command":{...}}`. When clap refuses, or displays help or a
/// version, it is `{"ok":false,"message":{...}}`, whose `plain` field is clap's
/// real text (custom `did you mean` tips included) and whose `ansi` field is
/// the same text with escape codes. A refused command line is a normal
/// outcome here, not an error.
///
/// # Errors
///
/// Returns [`CliError::Argv`] if `argv_json` is not a JSON array of strings.
pub fn parse_argv_to_json(argv_json: &str) -> Result<String, CliError> {
    let argv: Vec<String> = serde_json::from_str(argv_json).map_err(CliError::Argv)?;
    let envelope = match <Cli as clap::Parser>::try_parse_from(argv) {
        Ok(cli) => ParseEnvelope {
            ok: true,
            command: Some((&cli).into()),
            message: None,
        },
        Err(err) => ParseEnvelope {
            ok: false,
            command: None,
            message: Some(ClapMessage {
                kind: format!("{:?}", err.kind()),
                is_error: err.use_stderr(),
                exit_code: err.exit_code(),
                // `to_string` is the unstyled form regardless of the command's
                // ColorChoice; `ansi()` is the only way to keep the styling.
                plain: err.to_string(),
                ansi: err.render().ansi().to_string(),
            }),
        },
    };
    serde_json::to_string(&envelope).map_err(CliError::Serialize)
}

/// The command a help path names, built and ready to render.
///
/// `path` is a space-separated subcommand path: `""` for the root, `"list"` for
/// a verb, `"graph validate"` for a nested one. The root is built before the
/// walk so the global `--store` option and the generated `--help`/`--version`
/// flags have been propagated down the tree, which is what makes a subcommand's
/// rendered help match what `salvor <verb> --help` prints rather than a bare
/// shell of it.
fn command_at(path: &str) -> Result<clap::Command, CliError> {
    let mut command = <Cli as clap::CommandFactory>::command();
    command.build();
    for segment in path.split_whitespace() {
        command = command.find_subcommand(segment).cloned().ok_or_else(|| {
            CliError::UnknownSubcommand {
                path: path.to_owned(),
                unknown: segment.to_owned(),
            }
        })?;
    }
    Ok(command)
}

/// Renders `--help` for the root command or for a named subcommand, unstyled.
///
/// `path` is a space-separated subcommand path: `""` for `salvor --help`,
/// `"list"` for `salvor list --help`, `"graph validate"` for the nested one.
///
/// # Errors
///
/// Returns [`CliError::UnknownSubcommand`] if a segment of `path` names no
/// subcommand in this build.
pub fn render_help_to_string(path: &str) -> Result<String, CliError> {
    // `StyledStr`'s Display strips styling whatever the ColorChoice is, which
    // is exactly the plain form wanted here.
    Ok(command_at(path)?.render_long_help().to_string())
}

/// Renders `--help` for the root command or for a named subcommand, with ANSI
/// styling, for a terminal emulator that can draw it.
///
/// The same text [`render_help_to_string`] returns, escape codes included.
///
/// # Errors
///
/// Returns [`CliError::UnknownSubcommand`] if a segment of `path` names no
/// subcommand in this build.
pub fn render_help_to_ansi_string(path: &str) -> Result<String, CliError> {
    // `.ansi()` explicitly: plain Display would strip the styling.
    Ok(command_at(path)?.render_long_help().ansi().to_string())
}

/// Renders the `list` table from a JSON array of rows, with ANSI styling.
///
/// This is the render core, callable from any target, and it is what `salvor
/// list` itself writes: [`salvor_cli_core::render::list_table`] styles the
/// STATUS column unconditionally. Each row is a `RunSummary` with an added
/// `status` key, the label folded from that run's log:
///
/// ```json
/// [{"run_id":"...","event_count":7,
///   "first_recorded_at":"2025-07-15T08:00:00Z",
///   "last_recorded_at":"2025-07-15T08:05:00Z",
///   "status":"completed"}]
/// ```
///
/// Rows are rendered in the order given; ordering and filtering are the
/// caller's, exactly as they are the command handler's in the real CLI.
///
/// # Errors
///
/// Returns [`CliError::Rows`] if `rows_json` is not a JSON array of rows.
pub fn render_list_to_string(rows_json: &str) -> Result<String, CliError> {
    let rows: Vec<ListRow> = serde_json::from_str(rows_json).map_err(CliError::Rows)?;
    let rows: Vec<(RunSummary, String)> = rows
        .into_iter()
        .map(|row| (row.summary, row.status))
        .collect();
    Ok(render::list_table(&rows))
}

/// Renders the `list` table with the ANSI styling stripped, the way a piped
/// `salvor list` reads.
///
/// The stripping is `anstream::adapter::strip_str`, the same pass
/// `anstream::print!` makes over the table when the real CLI's stdout is not a
/// terminal, so this is plain in the same way `salvor list | cat` is plain.
///
/// # Errors
///
/// Returns [`CliError::Rows`] if `rows_json` is not a JSON array of rows.
pub fn render_list_to_plain_string(rows_json: &str) -> Result<String, CliError> {
    let styled = render_list_to_string(rows_json)?;
    Ok(anstream::adapter::strip_str(&styled).to_string())
}

/// Parses a `salvor` command line and returns what it means, as JSON.
///
/// `argvJson` is the full argv as a JSON array of strings, program name
/// included: `["salvor", "list", "--group", "waiting"]`.
///
/// Returns `{"ok":true,"command":{...}}` when the line parses. When clap
/// refuses it, or the line asked for `--help` or `--version`, returns
/// `{"ok":false,"message":{...}}` carrying clap's own text as `plain` (no
/// styling) and `ansi` (with escape codes), plus its `kind`, its `exit_code`,
/// and `is_error` for whether a shell would have written it to stderr.
///
/// Throws only if `argvJson` is not a JSON array of strings. A command line
/// clap refuses is a message, not a throw.
#[wasm_bindgen(js_name = parseArgv)]
pub fn parse_argv_js(argv_json: &str) -> Result<String, JsError> {
    parse_argv_to_json(argv_json).map_err(|e| JsError::new(&e.to_string()))
}

/// Returns the `--help` text for the root command or a named subcommand,
/// unstyled.
///
/// `path` is a space-separated subcommand path: `""` for `salvor --help`,
/// `"list"` for `salvor list --help`, `"graph validate"` for the nested one.
///
/// Throws if a segment of `path` names no subcommand.
#[wasm_bindgen(js_name = helpText)]
pub fn help_text_js(path: &str) -> Result<String, JsError> {
    render_help_to_string(path).map_err(|e| JsError::new(&e.to_string()))
}

/// Returns the `--help` text for the root command or a named subcommand, with
/// ANSI styling, for a terminal emulator that can draw it.
///
/// Throws if a segment of `path` names no subcommand.
#[wasm_bindgen(js_name = helpTextAnsi)]
pub fn help_text_ansi_js(path: &str) -> Result<String, JsError> {
    render_help_to_ansi_string(path).map_err(|e| JsError::new(&e.to_string()))
}

/// Renders the `salvor list` table from a JSON array of rows, with the ANSI
/// styling `salvor list` itself emits.
///
/// Each row is a `RunSummary` with an added `status` key. Throws if `rowsJson`
/// is not a JSON array of rows.
#[wasm_bindgen(js_name = renderList)]
pub fn render_list_js(rows_json: &str) -> Result<String, JsError> {
    render_list_to_string(rows_json).map_err(|e| JsError::new(&e.to_string()))
}

/// Renders the `salvor list` table with the ANSI styling stripped, the way a
/// piped `salvor list` reads.
///
/// Throws if `rowsJson` is not a JSON array of rows.
#[wasm_bindgen(js_name = renderListPlain)]
pub fn render_list_plain_js(rows_json: &str) -> Result<String, JsError> {
    render_list_to_plain_string(rows_json).map_err(|e| JsError::new(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> String {
        serde_json::to_string(words).unwrap()
    }

    fn parse(words: &[&str]) -> serde_json::Value {
        serde_json::from_str(&parse_argv_to_json(&argv(words)).unwrap()).unwrap()
    }

    /// The simplest full parse pins the envelope shape and the two things a
    /// caller reads first: the global `--store` and the `verb` tag.
    #[test]
    fn surface_pin_list_parses() {
        let out = parse_argv_to_json(&argv(&["salvor", "list"])).unwrap();
        assert_eq!(
            out,
            r#"{"ok":true,"command":{"store":"./salvor.db","command":{"verb":"list","status":[]}}}"#
        );
    }

    /// Repeatable and optional flags pin the list verb's full shape: `status`
    /// is always an array, the three optionals appear only when given.
    #[test]
    fn surface_pin_list_with_every_filter() {
        let out = parse_argv_to_json(&argv(&[
            "salvor",
            "--store",
            "/tmp/s.db",
            "list",
            "--status",
            "completed",
            "--status",
            "failed",
            "--agent",
            "graph run",
            "--limit",
            "5",
        ]))
        .unwrap();
        assert_eq!(
            out,
            r#"{"ok":true,"command":{"store":"/tmp/s.db","command":{"verb":"list","status":["completed","failed"],"agent":"graph run","limit":5}}}"#
        );
    }

    /// The nested verb pins the `graph_verb` tag, so a caller switching on
    /// `command.verb` knows where to look next.
    #[test]
    fn surface_pin_graph_validate() {
        let out = parse_argv_to_json(&argv(&["salvor", "graph", "validate", "flow.json"])).unwrap();
        assert_eq!(
            out,
            r#"{"ok":true,"command":{"store":"./salvor.db","command":{"verb":"graph","command":{"graph_verb":"validate","path":"flow.json"}}}}"#
        );
    }

    /// Booleans are always present, so a caller never has to distinguish
    /// "false" from "absent" on a flag.
    #[test]
    fn surface_pin_fork_carries_its_flags() {
        let parsed = parse(&[
            "salvor",
            "fork",
            "abc",
            "--from-node",
            "n2",
            "--graph",
            "g.json",
            "--acknowledge-writes",
            "all",
            "--dry-run",
        ]);
        let command = &parsed["command"]["command"];
        assert_eq!(command["verb"], "fork");
        assert_eq!(command["from_node"], "n2");
        assert_eq!(command["acknowledge_writes"], "all");
        assert_eq!(command["dry_run"], true);
        assert_eq!(command["agents"], serde_json::json!([]));
    }

    /// The mistake the CLI's own tests guard has to survive the boundary: a
    /// status passed to `--group` must come back with the tip naming the flag
    /// that takes it and the group the status really lives in.
    #[test]
    fn a_status_passed_as_a_group_keeps_its_tip() {
        let parsed = parse(&["salvor", "list", "--group", "awaiting-model"]);
        assert_eq!(parsed["ok"], false);
        let message = &parsed["message"];
        assert_eq!(message["kind"], "InvalidValue");
        assert_eq!(message["is_error"], true);
        assert_eq!(message["exit_code"], 2);
        let plain = message["plain"].as_str().unwrap();
        assert!(plain.contains("--status awaiting-model"), "{plain}");
        assert!(plain.contains("--group progress"), "{plain}");
        assert!(!plain.contains("similar value exists"), "{plain}");
    }

    /// The plain form carries no escape codes and the ANSI form carries the
    /// same text with them. This is the distinction the whole two-form surface
    /// rests on, so it is asserted rather than assumed.
    #[test]
    fn the_plain_and_ansi_forms_differ_only_in_styling() {
        let parsed = parse(&["salvor", "list", "--group", "awaiting-model"]);
        let plain = parsed["message"]["plain"].as_str().unwrap();
        let ansi = parsed["message"]["ansi"].as_str().unwrap();
        assert!(!plain.contains('\u{1b}'), "the plain form is unstyled");
        assert!(ansi.contains('\u{1b}'), "the ANSI form is styled");
        assert_eq!(
            anstream::adapter::strip_str(ansi).to_string(),
            plain,
            "stripping the ANSI form yields the plain one"
        );
    }

    /// Asking for help is not a refusal: clap reports it as a display, with a
    /// zero exit code and stdout as its stream.
    #[test]
    fn help_is_a_display_not_an_error() {
        let parsed = parse(&["salvor", "--help"]);
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["message"]["kind"], "DisplayHelp");
        assert_eq!(parsed["message"]["is_error"], false);
        assert_eq!(parsed["message"]["exit_code"], 0);
    }

    /// The root help names the binary and every verb, so a browser terminal's
    /// first `salvor --help` is the real thing.
    #[test]
    fn root_help_lists_the_verbs() {
        let help = render_help_to_string("").unwrap();
        assert!(help.contains("Usage: salvor"), "{help}");
        for verb in [
            "run",
            "resume",
            "fork",
            "resolve",
            "abandon",
            "list",
            "completions",
            "history",
            "replay",
            "serve",
            "build",
            "graph",
        ] {
            assert!(help.contains(verb), "root help offers {verb}: {help}");
        }
        assert!(!help.contains('\u{1b}'), "the plain form is unstyled");
    }

    /// A subcommand's help has to carry the globally propagated `--store`, not
    /// just its own flags, or the browser would show a narrower command than
    /// the terminal has.
    #[test]
    fn subcommand_help_carries_the_global_option() {
        let help = render_help_to_string("list").unwrap();
        assert!(help.contains("Usage: salvor list"), "{help}");
        assert!(help.contains("--status"), "{help}");
        assert!(help.contains("--group"), "{help}");
        assert!(help.contains("--store"), "globals propagate: {help}");
    }

    /// The nested path resolves through two levels.
    #[test]
    fn nested_subcommand_help_resolves() {
        let help = render_help_to_string("graph validate").unwrap();
        assert!(help.contains("Usage: salvor graph validate"), "{help}");
    }

    /// The ANSI help is the plain help plus styling, the same relationship the
    /// two error forms have.
    #[test]
    fn ansi_help_strips_back_to_plain_help() {
        let plain = render_help_to_string("list").unwrap();
        let ansi = render_help_to_ansi_string("list").unwrap();
        assert!(ansi.contains('\u{1b}'), "the ANSI form is styled");
        assert_eq!(anstream::adapter::strip_str(&ansi).to_string(), plain);
    }

    /// A typo in a help path is a caller error, not a panic or an empty page.
    #[test]
    fn unknown_help_path_errors() {
        assert!(matches!(
            render_help_to_string("valdiate").unwrap_err(),
            CliError::UnknownSubcommand { .. }
        ));
        assert!(matches!(
            render_help_to_string("graph nope").unwrap_err(),
            CliError::UnknownSubcommand { .. }
        ));
    }

    const ONE_ROW: &str = r#"[{"run_id":"00000000-0000-4000-8000-0000000000aa","event_count":7,"first_recorded_at":"2025-07-15T08:00:00Z","last_recorded_at":"2025-07-15T08:05:00Z","status":"completed"}]"#;

    /// The plain table pins the header and the column layout: 36 for the id, 20
    /// for the status, 6 right-aligned for the count, 20 for each timestamp.
    #[test]
    fn surface_pin_list_table_plain() {
        let table = render_list_to_plain_string(ONE_ROW).unwrap();
        assert_eq!(
            table,
            "RUN ID                                STATUS                EVENTS  STARTED               LAST ACTIVITY       \n\
             00000000-0000-4000-8000-0000000000aa  completed                  7  2025-07-15 08:00:00Z  2025-07-15 08:05:00Z\n"
        );
    }

    /// The styled table is the one `salvor list` writes; stripping it yields
    /// the plain one, so the two never disagree about the layout.
    #[test]
    fn the_styled_table_strips_to_the_plain_one() {
        let styled = render_list_to_string(ONE_ROW).unwrap();
        assert!(styled.contains('\u{1b}'), "the status column is styled");
        assert_eq!(
            anstream::adapter::strip_str(&styled).to_string(),
            render_list_to_plain_string(ONE_ROW).unwrap()
        );
    }

    /// No rows is a header and nothing else, not a panic and not an empty
    /// string: the real CLI decides separately whether to print a table at all.
    #[test]
    fn an_empty_table_is_just_its_header() {
        let table = render_list_to_plain_string("[]").unwrap();
        assert_eq!(table.lines().count(), 1);
        assert!(table.starts_with("RUN ID"));
    }

    /// Malformed input from the caller is an error, not a panic.
    #[test]
    fn bad_input_errors() {
        assert!(matches!(
            parse_argv_to_json("not json").unwrap_err(),
            CliError::Argv(_)
        ));
        assert!(matches!(
            parse_argv_to_json("[1, 2]").unwrap_err(),
            CliError::Argv(_)
        ));
        assert!(matches!(
            render_list_to_string(r#"[{"nope":true}]"#).unwrap_err(),
            CliError::Rows(_)
        ));
    }
}
