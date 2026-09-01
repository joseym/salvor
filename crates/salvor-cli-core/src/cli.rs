//! The command-line surface, as `clap` derive types.
//!
//! Keeping the parse tree in one module (separate from the handlers in
//! `salvor_cli::commands`) means the shape of the CLI reads top to bottom here,
//! and the handlers take already-parsed, typed arguments. The two global
//! options, `--store` and `--caller`, are defined once and shared by every
//! subcommand; [`command_hiding_unusable_globals`] keeps them out of the help
//! of the one verb that reads no store and writes no event.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Salvor: a durable execution runtime for AI agents.
//
// `about` is written out rather than left bare: bare `about` expands to this
// crate's own `CARGO_PKG_DESCRIPTION`, which describes the parse tree, whereas
// the line `salvor --help` opens with has to describe the binary. `version`
// stays bare because every crate here inherits the one workspace version, so
// there is only one number it could resolve to.
#[derive(Debug, Parser)]
#[command(
    name = "salvor",
    version,
    about = "Salvor CLI: run, resume, list, history, and replay for durable agent runs",
    long_about = None
)]
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

    /// The name to record as the caller on the events this command writes.
    ///
    /// A verb that starts, resumes, resolves, or abandons a run records who
    /// asked for it. Left unset, that is the operating system user this
    /// process runs as; this flag names someone else, for a wrapper script or
    /// a job runner that knows better than the account it happens to run
    /// under. The precedence is the flag, then `SALVOR_CALLER`, then the
    /// operating system user, which `salvor_cli::caller_name` resolves.
    ///
    /// It is a label on the events, never a credential: nothing checks it and
    /// nothing is granted by it. A store written by whoever can write the file
    /// carries whatever name they chose, which is why the server takes its own
    /// name from a verified token instead.
    #[arg(long, global = true, env = "SALVOR_CALLER", value_name = "NAME")]
    pub caller: Option<String>,

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
    /// Wake every run whose durable timer has come due, then exit. Nothing
    /// wakes a sleeping run on its own, so run this from cron when no server
    /// is doing it.
    Wake(WakeArgs),
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
    ///
    /// Reading is the integrity check: every log listed is read back through
    /// its whole hash chain, and a store holding a run it refuses to read
    /// fails here rather than listing the rest. The store is read, never
    /// created, so a `--store` path with no database at it is refused (exit 2)
    /// instead of printing `no runs in <path>` and exiting 0.
    List(ListArgs),
    /// Print a shell completion script for `salvor` on stdout.
    Completions(CompletionsArgs),
    /// Print a run's event log.
    ///
    /// The store is read, never created: a `--store` path with no database at
    /// it is refused (exit 2), rather than reading back as a store in which
    /// this run does not exist.
    History(HistoryArgs),
    /// Re-derive a run's state from its log without executing anything. The
    /// only mode; nothing is ever executed.
    ///
    /// The store is read, never created: a `--store` path with no database at
    /// it is refused (exit 2), rather than reading back as a store in which
    /// this run does not exist.
    Replay(ReplayArgs),
    /// Take an anchor over this store: one line per run naming how many events
    /// it holds and the hash that commits to them. Keep the file somewhere the
    /// store cannot reach.
    ///
    /// The store is read, never created: a `--store` path with no database at
    /// it is refused, so a typo cannot produce an anchor over an empty store
    /// this command just made.
    ///
    /// Every run's log is read before anything is written, and a store holding
    /// a run this store itself refuses is not anchored at all: an anchor must
    /// not record a head for a run nobody can read. `--force` does not lift
    /// that one.
    ///
    /// Exit codes. 0: the anchor was written. 1: the store holds a run it
    /// refuses to read, or the file at `--out` is an anchor this store no
    /// longer verifies against, so it was not overwritten. 2: no anchor was
    /// taken (no store at the path, a store holding no runs without
    /// `--allow-empty`, a file at `--out` that is not an anchor, or a write
    /// that failed, such as a `--out` under a directory that is not there).
    /// Exit 2 never means the store is suspect.
    Anchor(AnchorArgs),
    /// Check this store against an anchor taken earlier: every anchored run
    /// must still hold, unchanged, the events it was anchored at.
    ///
    /// The store is read, never created: a `--store` path with no database at
    /// it is refused, so a typo cannot read back as a store in which every
    /// anchored run is missing.
    ///
    /// Exit codes. 0: every anchored run is intact. 1: at least one is
    /// missing, shortened, rewritten, or broken. 2: the check did not run (no
    /// store at the path, or an anchor file that is missing, unreadable, not
    /// JSON, written under another spec, carrying a malformed entry, or
    /// committing to no runs at all without `--allow-empty`). Treat 2 as "I
    /// still do not know", never as a pass.
    ///
    /// With `--json` a check that did not run still prints a document on
    /// stdout, carrying `"checked": false` and the reason, so one parser reads
    /// every outcome.
    Verify(VerifyArgs),
    /// Run the control-plane HTTP + server-sent-events server over the store.
    Serve(ServeArgs),
    /// Build the whole product from a salvor checkout: the web dashboard, then
    /// the release binary that embeds it.
    Build(BuildArgs),
    /// Author-time agent definition tools: print the content hash a graph
    /// `agent` node has to reference. Reads no store and drives no run.
    Agent {
        /// The agent subcommand to run.
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Graph document tools. `edit`, `validate` and `schema` are author-time:
    /// they read no store and drive no run. `run` drives a document over the
    /// store, exactly as `salvor run` drives an agent run.
    Graph {
        /// The graph subcommand to run.
        #[command(subcommand)]
        command: GraphCommand,
    },
    /// Bearer token file tools, over the file `salvor serve --token-file`
    /// reads. Reads no store and starts no server.
    Token {
        /// The token subcommand to run.
        #[command(subcommand)]
        command: TokenCommand,
    },
}

/// The verbs under `salvor token`.
#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Add a named bearer token to a token file: mint one from the OS CSPRNG
    /// (or read one from stdin with `--stdin`), print it once, and append its
    /// SHA-256 under `[tokens.<name>]`.
    New(TokenNewArgs),
}

/// Arguments to `token new`.
#[derive(Debug, Args)]
pub struct TokenNewArgs {
    /// The name to add: `[a-z0-9-]`, 1 to 64 characters. This is the name an
    /// auth-failure or auth-success log line carries; it is never the token
    /// itself.
    #[arg(value_name = "NAME")]
    pub name: String,
    /// The token file to append to (the same file `--token-file` names).
    /// Refused unless it is mode 0600 or tighter and owned by the user
    /// running this command, the same rules `salvor serve --token-file`
    /// checks on every read.
    #[arg(long, value_name = "FILE")]
    pub file: PathBuf,
    /// Create `--file` at mode 0600 if it does not already exist, rather than
    /// refusing on a missing file.
    #[arg(long)]
    pub create: bool,
    /// Read the token from stdin instead of minting one, for importing a
    /// token minted elsewhere (a value another tool generated, or one moved
    /// from another token file). Held to the same 16-byte floor
    /// `--auth-token` checks, and to printable ASCII with no space, which is
    /// what an `Authorization` header can carry; trailing newline is trimmed.
    #[arg(long)]
    pub stdin: bool,
}

/// The verbs under `salvor agent`.
//
// Grouped by the document a verb reads, matching `salvor graph`: a hash is a
// fact about an agent definition, so it belongs beside the agent file rather
// than under a verb-first `salvor hash` that would then have to house the graph
// document's own hash too.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Print an agent definition's content hash, the `sha256:<64 hex>` string a
    /// graph `agent` node references.
    ///
    /// A graph names an agent by hash and never by path, because a run's log
    /// records only the hash and a replay has to mean the same agent. This is
    /// how an author learns that hash before writing the node.
    ///
    /// The hash covers the BUILT definition (model, system prompt, tool
    /// schemas, budgets, pricing), not the file's bytes, so any MCP server the
    /// file declares is connected to collect its tool schemas, exactly as a run
    /// would. Nothing is written and no run is started.
    Hash(AgentHashArgs),
    /// Build an agent definition and report what it declares, or the precise
    /// field-level error that stops it from building.
    ///
    /// By default this CONNECTS: it runs the exact build every other verb
    /// that takes `--agent` runs, which is a strict parse (an unknown field,
    /// a missing `model`, or a wrong type names the offending field)
    /// followed by connecting to each declared MCP server to introspect its
    /// tools, so a server that will not start is caught here rather than at
    /// run time. `--no-connect` skips that connection step: it checks only
    /// that the file parses and its fields have the right shape, and spawns
    /// no process and dials no socket, so it also works for a server whose
    /// command is not installed on this machine. Nothing else about the
    /// check differs from `salvor agent hash`; this verb only exists to be
    /// asked for by name and to report more than a hash.
    Validate(AgentValidateArgs),
}

/// Arguments to `agent hash`.
#[derive(Debug, Args)]
pub struct AgentHashArgs {
    /// An agent definition (TOML) to hash. Repeatable.
    ///
    /// One file prints the bare hash and nothing else, so it reads straight
    /// into a shell substitution: `--arg h "$(salvor agent hash a.toml)"`.
    /// Several files print `<path>: <hash>` per line, in the order given,
    /// because then the question being asked is which file carries which hash.
    #[arg(value_name = "FILE", required = true)]
    pub agents: Vec<PathBuf>,
}

/// Arguments to `agent validate`.
#[derive(Debug, Args)]
pub struct AgentValidateArgs {
    /// An agent definition (TOML) to validate. Repeatable.
    ///
    /// Each file is built independently: one file that fails to build does
    /// not stop the rest from being checked. Every file is reported, in the
    /// order given, and the command exits non-zero if any one of them fails.
    #[arg(value_name = "FILE", required = true)]
    pub agents: Vec<PathBuf>,
    /// Check fields and shape only; do not connect to any declared MCP
    /// server.
    ///
    /// Without this flag, validation connects to each declared MCP server
    /// (spawns a `command` transport, dials a `url` transport) to introspect
    /// its tools, the same connection a real run makes. With this flag, no
    /// process is spawned and no socket is dialed: only the TOML's fields and
    /// shape are checked, so a declared server whose command is not
    /// installed on this machine still passes. The report says which MCP
    /// servers were skipped this way, and the printed hash is omitted, since
    /// an agent's hash depends on MCP tool schemas this mode never collects.
    #[arg(long = "no-connect")]
    pub no_connect: bool,
}

/// The verbs under `salvor graph`.
#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Build a graph document one line at a time, reading commands from stdin.
    ///
    /// The document is a fold of the commands applied to it, so `undo` steps
    /// back without any command needing an inverse, and `history` dumps the
    /// session as a script that rebuilds it. Type `help` at the prompt for the
    /// grammar; it is printed from the same table the parser is written
    /// against, so the editor documents itself.
    ///
    /// Nothing is saved until a line names a file, and the only files touched
    /// are the ones a line names: `read <PATH>`, `write <PATH>`, and an agent
    /// node's `--file <PATH>`, which is resolved to a definition hash by
    /// building the agent exactly as `salvor agent hash` does. No store is
    /// opened and no run is driven.
    Edit(GraphEditArgs),
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

/// Arguments to `graph edit`.
///
/// Two ways to start somewhere other than an empty document, and they compose:
/// the positional FILE is the document to begin from, `--script` is the
/// commands to apply to it. Both are applied in that order before the first
/// line is read from stdin, which is the order the two mean together (open a
/// document, then edit it).
///
/// There is no flag for the commands themselves: they arrive on stdin, so a
/// person types them and a script is redirected in, with no second grammar for
/// the difference. Nor is there one for the output document, because `write
/// <PATH>` is a line of that grammar already.
#[derive(Debug, Args)]
pub struct GraphEditArgs {
    /// An existing graph document (JSON) to open. Applied as the `read <FILE>`
    /// line the author would otherwise type first, so it is recorded in the
    /// history like any other command and `undo` steps back across it. Omit to
    /// start from an empty document.
    #[arg(value_name = "FILE")]
    pub path: Option<PathBuf>,
    /// A file of editor commands to apply before reading stdin, in the form
    /// `history` dumps.
    ///
    /// Every line is applied as if typed, so a dumped session replays into the
    /// identical document and the author carries on from where it left off.
    /// Because a dumped script only ever names already-resolved values, it
    /// replays with no agent definition present.
    #[arg(long, value_name = "FILE")]
    pub script: Option<PathBuf>,
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
    /// same kill/resume guarantee.
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

/// Arguments to `wake`.
///
/// The definition flags are `resume`'s, with the same meanings and the same
/// validation, because waking a run IS resuming it: every due run is re-driven
/// through `salvor resume`'s own path, so whatever that verb needs to rebuild a
/// run, this verb needs too. One sweep can therefore only cover runs the files
/// given here describe; a due run this invocation cannot rebuild is reported
/// and left asleep, still due for the next sweep.
#[derive(Debug, Args)]
pub struct WakeArgs {
    /// Path to an agent definition (TOML) a due run may need to rebuild its
    /// agent. Repeatable, exactly as on `resume`: an agent run is woken under
    /// exactly one, and a graph run under the files its `agent` nodes
    /// reference.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Path to the graph document (JSON), needed to wake a GRAPH run, whose
    /// log records only the graph's hash. Its hash must match the one the run
    /// recorded. Omit when no due run is a graph run.
    #[arg(long, value_name = "FILE")]
    pub graph: Option<PathBuf>,
    /// Print which runs are due, what each one recorded, and whether the files
    /// given would wake it, then exit without driving anything. Exits 1 when a
    /// due run could not be woken with these files, so a crontab line can be
    /// checked before it is saved.
    #[arg(long)]
    pub dry_run: bool,
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
    /// Path to the agent definition (TOML) this run used. Repeatable, same
    /// flag `resume` takes. `resolve` neither reads nor builds it: the only
    /// use is echoing it back into the resume command printed on success, so
    /// that hint is a complete, real command rather than a `--agent <FILE>`
    /// placeholder the operator has to fill in by hand.
    #[arg(long = "agent", value_name = "FILE")]
    pub agents: Vec<PathBuf>,
    /// Path to the graph document (JSON) this run used, for the same reason
    /// `--agent` is accepted: echoed into the printed resume command for a
    /// graph run. Omit for an ordinary agent run.
    #[arg(long, value_name = "FILE")]
    pub graph: Option<PathBuf>,
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
    /// Accepted and ignored. Re-deriving state without executing anything is
    /// the only mode `replay` has ever run in; this flag stays only so a
    /// script written against an earlier version that passed it does not
    /// break.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `anchor`.
#[derive(Debug, Args)]
pub struct AnchorArgs {
    /// Write the anchor to this file instead of standard output.
    ///
    /// An existing file is read before it is replaced. If it is an anchor this
    /// store no longer verifies against, the write is refused (exit 1) rather
    /// than recording the rewrite over the evidence of it; if it is not an
    /// anchor at all, the write is refused too (exit 2). `--force` overwrites
    /// either. A write that fails, such as one under a directory that is not
    /// there, is exit 2 as well: no anchor was taken. Write it somewhere the
    /// store cannot reach: an anchor kept beside the database it describes is
    /// rewritten by whoever rewrites the database, and answers nothing.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Take an anchor over a store that holds no runs.
    ///
    /// Without it, an empty store is refused (exit 2) and nothing is written.
    /// An anchor over zero runs commits to nothing, and a later verify against
    /// it passes having checked nothing, which is the one failure mode an
    /// anchor exists to rule out. Pass this when a store that is legitimately
    /// empty still has to produce a file on a schedule.
    #[arg(long)]
    pub allow_empty: bool,
    /// Overwrite the file at `--out` whatever it holds.
    ///
    /// The store is still verified against it first and the answer still
    /// prints, as a warning rather than a refusal: this is the last moment
    /// anything can say what the old heads were. It does not lift the one
    /// refusal that is not about this file, a run this store cannot read,
    /// because no answer about the file at `--out` makes a run readable.
    #[arg(long)]
    pub force: bool,
}

/// Arguments to `verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// The anchor file to check this store against, as `salvor anchor` wrote
    /// it.
    #[arg(long, value_name = "FILE")]
    pub against: PathBuf,
    /// Print the result as JSON instead of the human report.
    ///
    /// The exit code is the same either way. A check that did not run prints a
    /// document too, carrying `"checked": false` and the reason, so a consumer
    /// parses one shape whatever happened.
    #[arg(long)]
    pub json: bool,
    /// Accept an anchor that commits to no runs.
    ///
    /// Without it, such an anchor is refused (exit 2) rather than passing: a
    /// pass over zero runs is not evidence about a store, and it looks
    /// identical to a real all-clear.
    #[arg(long)]
    pub allow_empty: bool,
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
        assert!(
            err.contains("--status awaiting-model"),
            "names the right flag: {err}"
        );
        assert!(
            err.contains("--group progress"),
            "names the real group: {err}"
        );
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
            assert!(
                err.contains(group),
                "{group} is offered in the refusal: {err}"
            );
        }
    }

    /// The mirror of the group test: `--status waiting` is the other easy confusion, and the
    /// refusal has to hand back the flag that does take it.
    #[test]
    fn a_group_passed_as_a_status_is_told_which_flag_to_use() {
        let err = parse(&["salvor", "list", "--status", "waiting"])
            .expect_err("a group is not a status")
            .to_string();
        assert!(
            err.contains("--group waiting"),
            "names the right flag: {err}"
        );
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
/// refusal, which for `awaiting-model` suggests "a similar value exists: 'waiting'": string
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
    /// token. The variable must be set and non-empty, and its value must
    /// carry at least 16 bytes: every request must then carry
    /// `Authorization: Bearer <that value>`, and the server refuses to start
    /// if the named variable is unset, empty, or shorter than the floor.
    /// Omit this flag and `--token-file` both to run without auth, trusting a
    /// reverse proxy to guard it. Never the token itself, matching how agent
    /// files name key variables.
    #[arg(long, value_name = "ENV_VAR")]
    pub auth_token: Option<String>,
    /// A TOML file of NAMED bearer tokens, each stored as the SHA-256 hash of
    /// the token rather than the token itself, so a copy of the file hands
    /// nobody a working credential.
    ///
    /// One `[tokens.<name>]` table per token with a `hash` key of 64
    /// lowercase hex characters. The file must be mode 0600 or tighter and
    /// owned by the user serving, and both are checked on every read, so a
    /// file loosened after the server started is refused on its next read.
    /// The server re-reads the file when it changes, so adding a token and
    /// revoking one both take effect on the next request with no restart, and
    /// a reload that changes the set logs the names it added, removed, and
    /// rotated.
    ///
    /// Unions with `--auth-token`: with both set, a request matching either
    /// one is let through, and a request matching a named token is attributed
    /// to that name.
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
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
    /// How often, in seconds, to look for runs whose durable timer has come
    /// due and re-drive them. Default 60.
    ///
    /// The sweeper is ON by default: a sleeping run wakes only when something
    /// re-drives it, so a server that held the store and let its timers pass
    /// would be silently wrong. `0` turns it off, for an operator who wakes
    /// runs from cron with `salvor wake` instead and does not want two things
    /// reaching for the same run. Each sweep folds every run's log (status is
    /// not a stored column), so the shorter this is, the more that costs.
    #[arg(long, value_name = "SECS", default_value_t = 60)]
    pub wake_interval: u64,
    /// Register a small set of deterministic demo tools (`lookup_invoice`
    /// read, `issue_refund` write, `send_email` idempotent) instead of the
    /// stock empty tool registry.
    ///
    /// Off by default: a plain `salvor serve` ships NO tools of its own (see
    /// `salvor_server::ToolRegistry`'s own docs), so a `tool` node or a
    /// client-driven tool step is a clean `unknown_tool` until a host
    /// registers something, the honest default for a library other hosts
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
    /// A client-performed tool DECLARATION (TOML) to load. Repeatable, the
    /// same way `--agent` is repeatable on `graph run`.
    ///
    /// The tool named in the file is one the CLIENT runs, in its own process,
    /// with its own secrets; this server holds no code for it. The file says
    /// what the operator is willing to accept about such a call: its name, its
    /// effect class, the schema its input must satisfy, the schema its reported
    /// completion must satisfy, and whether the client may close the call
    /// itself (`trust_completion`, false unless the file opts in, so a
    /// declaration silent about trust settles every call by hand), plus any
    /// fields whose reported value must equal what the intent recorded
    /// (`require_equal`).
    ///
    /// It is a file the operator passes here, and there is no endpoint that
    /// accepts one, on purpose. The effect class fixes whether an unsettled
    /// call surfaces for a human; a client that could declare its own tool
    /// would be deciding that about its own writes. See
    /// `salvor_server::client_tools` for the argument in full.
    #[arg(long = "client-tool", value_name = "FILE")]
    pub client_tools: Vec<PathBuf>,
}

/// The global options `token new` parses and cannot act on.
const HIDDEN_UNDER_TOKEN_NEW: [&str; 2] = ["store", "caller"];

/// The parse tree with the global options hidden from the help of the verbs
/// they mean nothing to.
///
/// `--store` and `--caller` are global, so clap lists them under every
/// subcommand, `token new` included. That verb opens no store and writes no
/// event, so both lines describe something it cannot do. clap has no per-verb
/// hiding of a global, and defining the two per subcommand instead would
/// repeat them across every other verb, so the tree is built first and the
/// copies clap propagated into `token new` are hidden after the fact: the
/// flags still parse there, exactly as they parse anywhere else, and only the
/// help text loses two lines that could not be acted on.
///
/// The rewrite goes through `mut_args`, which maps every argument of that one
/// subcommand in place. `mut_arg`, which names a single argument, removes it
/// and pushes it back on the end, and the lookup table a built command holds
/// indexes arguments by position, so the one name it moves takes every name
/// after it out of that table with it.
#[must_use]
pub fn command_hiding_unusable_globals() -> clap::Command {
    use clap::CommandFactory;

    let mut command = Cli::command();
    // Builds the whole tree, which is what copies a global option down into
    // each subcommand; before this, `token new` carries neither name.
    command.build();
    command.mut_subcommand("token", |token| {
        token.mut_subcommand("new", |new| {
            new.mut_args(|arg| {
                if HIDDEN_UNDER_TOKEN_NEW.contains(&arg.get_id().as_str()) {
                    arg.hide(true)
                } else {
                    arg
                }
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `token new --help` lists what that verb can act on, and the two global
    /// options are not among them.
    #[test]
    fn token_new_help_hides_the_globals_it_cannot_act_on() {
        let mut command = command_hiding_unusable_globals();
        let help = command
            .find_subcommand_mut("token")
            .expect("the token verb")
            .find_subcommand_mut("new")
            .expect("the new verb")
            .render_help()
            .to_string();
        assert!(!help.contains("--store"), "{help}");
        assert!(!help.contains("--caller"), "{help}");
        assert!(help.contains("--file"), "the verb's own flags stay: {help}");
        assert!(help.contains("--stdin"), "{help}");
    }

    /// Hiding is a help-text change only: both options still parse there, and
    /// every other verb still lists them.
    #[test]
    fn the_hidden_globals_still_parse_and_still_show_elsewhere() {
        use clap::FromArgMatches;

        let matches = command_hiding_unusable_globals()
            .try_get_matches_from(["salvor", "token", "new", "ci", "--file", "tokens.toml"])
            .expect("the verb parses with no global at all");
        assert!(matches.subcommand().is_some());
        let matches = command_hiding_unusable_globals()
            .try_get_matches_from([
                "salvor",
                "token",
                "new",
                "ci",
                "--file",
                "tokens.toml",
                "--store",
                "other.db",
            ])
            .expect("--store still parses under token new");
        let cli = Cli::from_arg_matches(&matches).expect("the matches build a Cli");
        assert_eq!(cli.store, PathBuf::from("other.db"));
        assert!(matches!(cli.command, Command::Token { .. }));

        let mut command = command_hiding_unusable_globals();
        let help = command
            .find_subcommand_mut("resume")
            .expect("the resume verb")
            .render_help()
            .to_string();
        assert!(help.contains("--store"), "{help}");
        assert!(help.contains("--caller"), "{help}");
    }
}
