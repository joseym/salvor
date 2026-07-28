//! The command-line surface, as `clap` derive types.
//!
//! Keeping the parse tree in one module (separate from the handlers in
//! [`crate::commands`]) means the shape of the CLI reads top to bottom here,
//! and the handlers take already-parsed, typed arguments. The one global
//! option, `--store`, is defined once and shared by every subcommand.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Salvor: a durable execution runtime for AI agents.
#[derive(Debug, Parser)]
#[command(name = "salvor", version, about, long_about = None)]
pub struct Cli {
    /// Path to the SQLite event store.
    ///
    /// The precedence is flag, then the `SALVOR_STORE` environment variable,
    /// then the default: an explicit `--store` wins, else `SALVOR_STORE`,
    /// else `./salvor.db`.
    //
    // clap resolves that precedence itself from the `env` and `default_value`
    // attributes below; nothing in the handlers re-implements it.
    #[arg(
        long,
        global = true,
        env = "SALVOR_STORE",
        default_value = "./salvor.db",
        value_name = "PATH"
    )]
    pub store: PathBuf,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The verbs of the CLI.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a fresh run of an agent.
    Run(RunArgs),
    /// Continue an existing run: resume a parked one, or recover a crashed one.
    Resume(ResumeArgs),
    /// Fork a graph run from a node boundary into a NEW run, refusing to
    /// re-execute a recorded write the operator has not acknowledged.
    Fork(ForkArgs),
    /// Record the completion of a dangling write by hand, after verifying it.
    Resolve(ResolveArgs),
    /// Abandon a run: retire it by hand without finishing or failing it, for a
    /// run that is dead forever or no longer worth carrying.
    Abandon(AbandonArgs),
    /// List runs in the store, newest activity last. Filters narrow what is
    /// printed; with none, every run is listed.
    List(ListArgs),
    /// Print a shell completion script for `salvor` on stdout.
    Completions(CompletionsArgs),
    /// Print a run's event log.
    History(HistoryArgs),
    /// Re-derive a run's state from its log without executing anything.
    Replay(ReplayArgs),
    /// Run the control-plane HTTP + server-sent-events server over the store.
    Serve(ServeArgs),
    /// Build the whole product from a salvor checkout: the web dashboard, then
    /// the release binary that embeds it.
    Build(BuildArgs),
    /// Author-time graph document tools: validate a document, or print its
    /// JSON Schema. These read no store and drive no run.
    Graph {
        /// The graph subcommand to run.
        #[command(subcommand)]
        command: GraphCommand,
    },
}

/// The verbs under `salvor graph`.
#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Validate a graph document JSON file: parse it strictly and run every
    /// check, printing a summary on success or the precise node/edge errors on
    /// failure.
    Validate(GraphValidateArgs),
    /// Print the graph document JSON Schema to stdout.
    Schema,
    /// Drive a graph document locally over the store, exactly as `salvor run`
    /// drives an agent run: each `agent` node resolves to a provided `--agent`
    /// file (keyed by its computed definition hash), and each `tool` node
    /// resolves from the tools those agents carry.
    Run(GraphRunArgs),
}

/// Arguments to `graph validate`.
#[derive(Debug, Args)]
pub struct GraphValidateArgs {
    /// Path to the graph document (JSON).
    #[arg(value_name = "FILE")]
    pub path: PathBuf,
}

/// Arguments to `graph run`.
#[derive(Debug, Args)]
pub struct GraphRunArgs {
    /// Path to the graph document (JSON).
    #[arg(value_name = "FILE")]
    pub graph: PathBuf,
    /// The run input: a JSON value, or `@path` to read JSON from a file.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: String,
    /// An agent definition (TOML) an `agent` node may reference. Repeatable:
    /// each file is built and keyed by its computed definition hash, and a
    /// graph `agent_hash` that matches none of them fails with a precise
    /// message listing the hashes that were provided.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// A correlation tag `key=value`, recorded once on the run's
    /// `GraphRunStarted`. Repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
}

/// Arguments to `run`.
///
/// Two mutually exclusive ways to say what to run: the ordinary
/// `--agent`/`--input` pair, or a single `--fixture <DIR>` naming a
/// self-contained fixture directory. The pair stays required when `--fixture`
/// is absent, and passing both is a clap error rather than a runtime surprise,
/// so an operator who mixes them is told at parse time which flags conflict.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the agent definition (TOML). Required unless `--fixture` is
    /// given, which supplies the agent itself.
    #[arg(
        long,
        value_name = "FILE",
        required_unless_present = "fixture",
        conflicts_with = "fixture"
    )]
    pub agent: Option<PathBuf>,
    /// The run input: a JSON value, or `@path` to read JSON from a file.
    /// Required unless `--fixture` is given, which supplies the input itself.
    #[arg(
        long,
        value_name = "JSON|@FILE",
        required_unless_present = "fixture",
        conflicts_with = "fixture"
    )]
    pub input: Option<String>,
    /// Run a self-contained fixture directory offline: no API key, no network.
    ///
    /// The directory holds `agent.toml`, `input.json`, and `model.json` (the
    /// recorded model conversation). Salvor serves that conversation from an
    /// in-process HTTP server on a free local port and points the agent's
    /// declared `[llm] base_url_env` variable at it, so the agent file needs
    /// no edit to switch between the fixture and a real model. Everything
    /// after that is an ordinary run: the same store, the same event log, the
    /// same kill/resume guarantee. See [`crate::fixture`].
    #[arg(long, value_name = "DIR")]
    pub fixture: Option<PathBuf>,
}

/// Arguments to `resume`.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// The run id (a UUID) to continue.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Path to an agent definition (TOML), needed to rebuild the agent.
    /// Repeatable: an agent run needs exactly one; a graph run needs the files
    /// its `agent` nodes reference (zero or more).
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Path to the graph document (JSON), needed to re-drive a GRAPH run. The
    /// run's log records only the graph's hash, not the document, exactly as it
    /// records an agent by hash and not its definition; so a graph run's resume
    /// re-supplies the document here, the same way an agent run re-supplies its
    /// definition through `--agent`. Its hash must match the one the run
    /// recorded. Omit for an ordinary agent run.
    #[arg(long, value_name = "FILE")]
    pub graph: Option<PathBuf>,
    /// The resume input, required for a parked run: a JSON value, or `@path`.
    /// Ignored (with a warning) when recovering a crashed run.
    #[arg(long, value_name = "JSON|@FILE")]
    pub input: Option<String>,
}

/// Arguments to `fork`.
#[derive(Debug, Args)]
pub struct ForkArgs {
    /// The origin run id (a UUID) to fork.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// The node boundary to restart the fork from: the fork re-walks from this
    /// node, carrying the origin's events below it as an identical prefix.
    #[arg(long = "from-node", value_name = "NODE")]
    pub from_node: String,
    /// Path to the graph document (JSON) the origin ran. Re-supplied the same way
    /// a graph resume re-supplies it (the log records only the hash); its hash
    /// must match the recorded one, since a fork reuses the origin's graph
    /// unchanged.
    #[arg(long, value_name = "FILE")]
    pub graph: PathBuf,
    /// An agent definition (TOML) the graph's `agent` nodes reference.
    /// Repeatable, exactly as `graph run` and a graph `resume` take them.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Acknowledge the writes the re-walked segment would re-fire: a
    /// comma-separated list of origin log positions (`4,7`), or `all` to
    /// acknowledge the full hazard set. Recorded permanently into the child's
    /// fork origin. Omit when the fork boundary sits before any write.
    #[arg(long = "acknowledge-writes", value_name = "SEQ,SEQ|all")]
    pub acknowledge_writes: Option<String>,
    /// Print what the fork WOULD do (the hazard list and the would-be prefix
    /// summary) without creating a run.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `resolve`.
#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// The run id (a UUID) that needs reconciliation.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// The output to record for the dangling write, after verifying externally
    /// what it did: a JSON value, or `@path` to read JSON from a file. It is
    /// recorded verbatim as the tool's output, so replay never re-runs the
    /// write.
    #[arg(long, value_name = "JSON|@FILE")]
    pub output: String,
}

/// Arguments to `abandon`.
#[derive(Debug, Args)]
pub struct AbandonArgs {
    /// The run id (a UUID) to abandon.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// An optional note for why the run is being abandoned, recorded on the
    /// terminal event. Omit it to abandon with no reason.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
}

/// Arguments to `history`.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// The run id (a UUID) whose log to print.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Print the raw event envelopes as JSON instead of the pretty log.
    #[arg(long)]
    pub json: bool,
}

/// Arguments to `replay`.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// The run id (a UUID) to re-derive state for.
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    /// Re-derive state from the log without executing anything. Required in
    /// this version: live replay is not yet available.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// After building, install the release binary onto the PATH with
    /// `cargo install --path crates/salvor-cli`, so the `salvor` you run from
    /// anywhere carries the dashboard just built.
    #[arg(long)]
    pub install: bool,
}

#[cfg(test)]
mod group_parser_tests {

    use clap::Parser;

    fn parse(args: &[&str]) -> Result<super::Cli, clap::Error> {
        super::Cli::try_parse_from(args)
    }

    /// The mistake this exists for: reaching for `--group` with a status name. The refusal has to
    /// name the status flag AND the status's real group, because clap's own similarity guess
    /// ("a similar value exists: 'waiting'") points at the WRONG group for `awaiting-model`.
    #[test]
    fn a_status_passed_as_a_group_is_told_which_flag_to_use() {
        let err = parse(&["salvor", "list", "--group", "awaiting-model"])
            .expect_err("a status is not a group")
            .to_string();
        assert!(err.contains("--status awaiting-model"), "names the right flag: {err}");
        assert!(err.contains("--group progress"), "names the real group: {err}");
        assert!(
            !err.contains("similar value exists"),
            "clap's similarity guess must not survive alongside the real answer: {err}"
        );
    }

    /// The set of legal values has to reach `--help` and the shell, which is why the parser
    /// implements `possible_values` rather than validating in a plain function.
    #[test]
    fn the_legal_values_are_advertised_not_just_enforced() {
        let err = parse(&["salvor", "list", "--group", "sideways"])
            .expect_err("nonsense is refused")
            .to_string();
        for group in super::GROUPS {
            assert!(err.contains(group), "{group} is offered in the refusal: {err}");
        }
    }

    /// The mirror of the group test: `--status waiting` is the other easy confusion, and the
    /// refusal has to hand back the flag that does take it.
    #[test]
    fn a_group_passed_as_a_status_is_told_which_flag_to_use() {
        let err = parse(&["salvor", "list", "--status", "waiting"])
            .expect_err("a group is not a status")
            .to_string();
        assert!(err.contains("--group waiting"), "names the right flag: {err}");
    }

    /// Every label the STATUS column can print must be offered to the shell and accepted by the
    /// flag; a state you can see but cannot filter for is the bug this guards.
    #[test]
    fn every_printable_status_is_a_legal_filter_value() {
        for status in crate::render::STATUS_LABELS {
            assert!(
                parse(&["salvor", "list", "--status", status]).is_ok(),
                "{status} is printed by the table, so --status must accept it"
            );
        }
    }

    #[test]
    fn the_three_groups_parse() {
        for group in super::GROUPS {
            assert!(
                parse(&["salvor", "list", "--group", group]).is_ok(),
                "{group} is a legal value"
            );
        }
    }
}

/// Parses `--status`: completes every label the STATUS column can print, and refuses a GROUP name
/// with the flag that does take it.
///
/// The mirror image of [`GroupParser`], and for the same reason. The two flags sit next to each
/// other in `--help` and answer neighbouring questions, so each is the natural wrong guess for the
/// other; whichever one a caller reaches for, the refusal should hand them the other rather than
/// making them re-read the help.
#[derive(Clone, Debug)]
struct StatusParser;

impl clap::builder::TypedValueParser for StatusParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let value = value.to_string_lossy();
        if crate::render::STATUS_LABELS.contains(&value.as_ref()) {
            return Ok(value.into_owned());
        }

        let mut err = clap::Error::new(clap::error::ErrorKind::InvalidValue).with_cmd(cmd);
        if let Some(arg) = arg {
            err.insert(
                clap::error::ContextKind::InvalidArg,
                clap::error::ContextValue::String(arg.to_string()),
            );
        }
        err.insert(
            clap::error::ContextKind::InvalidValue,
            clap::error::ContextValue::String(value.clone().into_owned()),
        );
        err.insert(
            clap::error::ContextKind::ValidValue,
            clap::error::ContextValue::Strings(
                crate::render::STATUS_LABELS.map(str::to_owned).to_vec(),
            ),
        );
        if GROUPS.contains(&value.as_ref()) {
            let tip: clap::builder::StyledStr = format!(
                "`{value}` is a group, not a status. Use `--group {value}` for every state in it."
            )
            .into();
            err.insert(
                clap::error::ContextKind::Suggested,
                clap::error::ContextValue::StyledStrs(vec![tip]),
            );
        }
        Err(err)
    }

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(
            crate::render::STATUS_LABELS
                .into_iter()
                .map(clap::builder::PossibleValue::new),
        ))
    }
}

/// The three group names, in the order they are offered.
const GROUPS: [&str; 3] = ["waiting", "progress", "terminal"];

/// Parses `--group`: completes the three names, and refuses a STATUS with the command the caller
/// actually wanted.
///
/// A plain `value_parser = [..]` would give completion and `--help` values but clap's generic
/// refusal, which for `awaiting-model` suggests "a similar value exists: 'waiting'" — string
/// similarity pointing at the WRONG group, since `awaiting-model` is `progress`. A plain parser
/// function would give the right message but no completion candidates, because a function cannot
/// enumerate what it accepts. Implementing the trait is how you get both: `possible_values` feeds
/// help and the shell, `parse_ref` owns the refusal.
#[derive(Clone, Debug)]
struct GroupParser;

impl clap::builder::TypedValueParser for GroupParser {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let value = value.to_string_lossy();
        if GROUPS.contains(&value.as_ref()) {
            return Ok(value.into_owned());
        }

        let mut err = clap::Error::new(clap::error::ErrorKind::InvalidValue).with_cmd(cmd);
        if let Some(arg) = arg {
            err.insert(
                clap::error::ContextKind::InvalidArg,
                clap::error::ContextValue::String(arg.to_string()),
            );
        }
        // The rejected value goes in the value slot and the legal set in the values slot, so clap
        // renders its usual "invalid value X ... [possible values: ...]" frame.
        err.insert(
            clap::error::ContextKind::InvalidValue,
            clap::error::ContextValue::String(value.clone().into_owned()),
        );
        err.insert(
            clap::error::ContextKind::ValidValue,
            clap::error::ContextValue::Strings(GROUPS.map(str::to_owned).to_vec()),
        );
        // The advice belongs in the tip slot, NOT the value slot: a status passed where a group
        // belongs is the common mistake, and clap's own suggestion would be string-similarity
        // ("did you mean 'waiting'?") pointing at the wrong group.
        if let Some(group) = crate::render::status_group(&value) {
            let tip: clap::builder::StyledStr = format!(
                "`{value}` is a status, not a group. Use `--status {value}` for that one state, \
                 or `--group {}` for every state that behaves like it.",
                group.as_str()
            )
            .into();
            err.insert(
                clap::error::ContextKind::Suggested,
                clap::error::ContextValue::StyledStrs(vec![tip]),
            );
        }
        Err(err)
    }

    /// What the shell completes and `--help` lists. Separate from `parse_ref` on purpose: the set
    /// of legal values is a fact about the flag, while the refusal is a conversation with a caller
    /// who got it wrong.
    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        Some(Box::new(
            GROUPS.into_iter().map(clap::builder::PossibleValue::new),
        ))
    }
}

/// Arguments to `completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to generate for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Arguments to `list`.
///
/// The filter vocabulary matches the web UI's, deliberately: `status` and `group` mean the same
/// things in both, so an operator who learns one surface can read the other.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Keep only runs with this status. Repeatable; a run matching ANY of them
    /// is kept. Values are the labels the STATUS column prints. To filter by a
    /// whole group of states instead, use `--group`.
    #[arg(long, value_name = "STATUS", value_parser = StatusParser)]
    pub status: Vec<String>,
    /// Keep only runs in this group: `waiting` (needs a person), `progress`
    /// (moving on its own), or `terminal` (finished, one way or another).
    /// These are the same three groups the status colours use. To filter by a
    /// single state instead, use `--status`.
    #[arg(long, value_name = "GROUP", value_parser = GroupParser)]
    pub group: Option<String>,
    /// Keep only runs whose agent identity contains this text: a definition
    /// hash, or `graph run` for a graph.
    #[arg(long, value_name = "TEXT")]
    pub agent: Option<String>,
    /// Print at most N runs, keeping the most recently active ones. The order
    /// on screen is unchanged, so the newest still sit at the bottom.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

/// Arguments to `serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The address to bind, host and port.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub bind: String,
    /// The NAME of an environment variable holding a shared-secret bearer
    /// token. When set (and the variable is non-empty), every request must
    /// carry `Authorization: Bearer <that value>`. When omitted, the server
    /// runs without auth, trusting a reverse proxy to guard it. Never the
    /// token itself, matching how agent files name key variables.
    #[arg(long, value_name = "ENV_VAR")]
    pub auth_token: Option<String>,
    /// Kill the running `salvor serve` instead of serving. With no value,
    /// discovers every running `salvor serve` (by inspecting the process
    /// table, since there are no pid files): zero found is reported and this
    /// exits; exactly one is killed; multiple print a numbered table and
    /// prompt for a choice. Given a value (a pid or a listening port), kills
    /// that one directly with no prompt. When present at all, this
    /// short-circuits before `--bind` or `--store` are acted on: the process
    /// never binds a port.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "PID|PORT")]
    pub kill: Option<String>,
    /// Also start the Angular dev server (`ng serve`) for `bridge/`, hot
    /// module reloading included, with `/v1` proxied to this API so a
    /// browser at the dev server's own URL calls straight through. This
    /// process's own bind/store handling is otherwise unchanged: the API
    /// binds and serves exactly as plain `serve` does. Requires a salvor
    /// checkout with a `bridge/` directory alongside it; the dashboard an
    /// installed `salvor` embeds is prebuilt and does not hot-reload.
    #[arg(long)]
    pub dev: bool,
    /// Register a small set of deterministic demo tools (`lookup_invoice`
    /// read, `issue_refund` write, `send_email` idempotent) instead of the
    /// stock empty tool registry.
    ///
    /// Off by default: a plain `salvor serve` ships NO tools of its own (see
    /// `salvor_server::ToolRegistry`'s own docs), so a `tool` node or a
    /// client-driven tool step is a clean `unknown_tool` until a host
    /// registers something — the honest default for a library other hosts
    /// (aarg's own render tool, for one) compose their own registry into.
    /// This flag is that one host, built in for demos and for the served
    /// end-to-end suite: with it, a graph carrying `tool` nodes can actually
    /// run against `salvor serve` with no embedding host at all. The demo
    /// tools are deterministic and hermetic (no network); see
    /// `salvor_cli::demo_tools` for what each one does and why it exists.
    /// Requires the crate's `fixture` feature (on by default; a
    /// `--no-default-features` build refuses this flag).
    #[arg(long)]
    pub demo_tools: bool,
}
