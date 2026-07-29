//! Dynamic shell completion: the values in *this* store, computed at the
//! moment Tab is pressed.
//!
//! [`crate::commands::completions`] generates a STATIC script: it knows every
//! verb, flag, and fixed value set, because all of those are facts about the
//! parse tree. What it cannot know is the one thing an operator types most —
//! a run id. This module answers that half, and the two live side by side: the
//! static script keeps working exactly as before, and a user who also enables
//! this one gets run ids and agent identities on top.
//!
//! # The protocol
//!
//! Activation is by environment variable, matching `clap_complete`'s own
//! `COMPLETE=<shell> <bin>` convention (see "Why not `clap_complete`" below),
//! so the incantation a user learns here is the one the wider Rust CLI
//! ecosystem already documents:
//!
//! - `COMPLETE=zsh salvor` — no arguments after a `--`, so this prints the
//!   shell function that registers `salvor` for completion. That is what a
//!   user evaluates from their rc file.
//! - `COMPLETE=zsh salvor -- salvor history <partial>` — the registration
//!   function's own call back into the binary on each Tab. Everything after
//!   `--` is the command line as the shell currently sees it, and
//!   `_SALVOR_COMPLETE_INDEX` says which of those words the cursor is in. The
//!   answer is candidate values, one per line, on stdout.
//!
//! Because the protocol is an environment variable plus argv, it adds nothing
//! to the parse tree: there is no hidden subcommand, and `docs/cli-manifest.json`
//! describes the same surface it did before.
//!
//! # Why not `clap_complete`'s own dynamic engine
//!
//! `clap_complete` 4.6 does ship one ([`clap_complete::env::CompleteEnv`] with
//! [`clap_complete::engine`]), but only behind its `unstable-dynamic` feature,
//! which in turn enables `clap/unstable-ext`. Two unstable feature gates on a
//! floating `4` requirement means any minor release of either crate may change
//! the API under a `cargo update`, and the thing that would break is the shell
//! integration of a shipped binary. This module is the conservative trade: a
//! few hundred lines of our own walking of the very same [`clap::Command`]
//! tree, no new dependency, no unstable gate, and total control over the
//! degradation rules below — which are the part that actually matters. If the
//! engine stabilises, the shell-facing protocol is deliberately the same one,
//! so the swap is an implementation change and not a user-visible one.
//!
//! # The two rules that outrank being useful
//!
//! **Never hang.** Every store read happens on a worker thread under a
//! [`STORE_BUDGET`] wall-clock deadline. A database locked by a running
//! `salvor serve` would otherwise block for the store's own five-second busy
//! timeout, with the operator's shell frozen and no way to tell why.
//!
//! **Never speak.** A missing store, a corrupt store, a store the user cannot
//! read, a poisoned lock, an outright panic — each degrades to zero candidates
//! and exit 0. Nothing is ever written to stderr from the completion path, and
//! the generated shell functions redirect it away besides. A shell that offers
//! nothing is a small disappointment; a shell that prints a Rust backtrace over
//! a half-typed command is a bug report.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::CommandFactory;
use salvor_store::{EventStore, SqliteStore};

/// The environment variable that activates completion, and names the shell.
/// `clap_complete`'s default, deliberately: see the module docs.
const ACTIVATION_VAR: &str = "COMPLETE";

/// The environment variable carrying the index (into the words after `--`) of
/// the word the cursor sits in. Set by the generated shell functions.
const INDEX_VAR: &str = "_SALVOR_COMPLETE_INDEX";

/// The most candidates ever printed for one Tab.
///
/// A menu longer than this is not a menu, it is a wall: nobody reads the 51st
/// run id, they type another character and press Tab again. Capping the
/// *output* also caps the cost of everything downstream of it in the shell.
const MAX_CANDIDATES: usize = 50;

/// The most run logs read to derive agent identities for one Tab.
///
/// Run ids come out of a single grouped query ([`EventStore::list_runs`]), so
/// they are cheap at any store size and are filtered across every run. An
/// agent identity is not stored: it lives in a run's first event, so each
/// distinct identity costs one full log read. That is the one genuinely
/// unbounded cost on this path, and this is the bound: the 50 most recently
/// active runs, newest first. Fifty is chosen to match [`MAX_CANDIDATES`] —
/// there is no point reading a log that could not produce a candidate anyone
/// would see — and because the set of agents an operator is switching between
/// right now is small; an identity that appears nowhere in the last fifty runs
/// is one they can finish typing. The deadline below is the backstop for the
/// pathological case (fifty enormous logs) that this count alone does not
/// bound.
const MAX_LOGS_READ: usize = 50;

/// The wall-clock budget for all store work behind one Tab.
///
/// Not a nice-to-have: [`SqliteStore::open`] sets `busy_timeout=5000`, so a
/// store locked by another writer blocks for five seconds. Under this deadline
/// the worker is simply abandoned and the shell gets no candidates, which is
/// the correct answer for "the store is busy" and arrives fast enough that a
/// human reads it as "there was nothing to offer" rather than as a freeze.
const STORE_BUDGET: Duration = Duration::from_millis(150);

/// Answers a completion request if this process is one, and reports whether it
/// was.
///
/// Call this before anything else in `main`: it owns stdout when it returns
/// `true`, and the caller must exit 0 immediately without printing anything
/// else. When it returns `false` this is an ordinary `salvor` invocation and
/// nothing has been read, written, or installed.
#[must_use]
pub fn try_complete() -> bool {
    let Some(shell) = activated_shell() else {
        return false;
    };

    // From here on this process exists only to answer a shell, so a panic must
    // never reach the terminal: the default hook prints the panic message and
    // location to stderr, and a completion function that emits that instead of
    // candidates is worse than one that emits nothing. Installed only after
    // the activation check, so an ordinary `salvor` run keeps the normal hook
    // and its normal crash reporting.
    std::panic::set_hook(Box::new(|_| {}));
    let output = std::panic::catch_unwind(|| respond(&shell)).unwrap_or_default();
    print!("{output}");
    true
}

/// The shell named by the activation variable, or `None` when this is not a
/// completion request.
///
/// `COMPLETE=` and `COMPLETE=0` are the documented ways to switch completion
/// back off without unsetting the variable, so both read as "not a request".
/// The value may be a path (a user reaching for `COMPLETE=$SHELL`), so only
/// the file stem is matched.
fn activated_shell() -> Option<String> {
    let raw = std::env::var_os(ACTIVATION_VAR)?;
    if raw.is_empty() || raw == "0" {
        return None;
    }
    let path = PathBuf::from(&raw);
    let stem = path.file_stem().unwrap_or(raw.as_os_str());
    Some(stem.to_string_lossy().into_owned())
}

/// The whole answer to one invocation, as the text to print on stdout.
///
/// Two shapes, told apart exactly as `clap_complete` tells them apart: no
/// words after a `--` means "print the registration script", any words mean
/// "complete this command line".
fn respond(shell: &str) -> String {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let words = words_after_escape(&argv);

    match words {
        None => registration(shell),
        Some(words) => {
            let words: Vec<String> = words
                .iter()
                .map(|word| word.to_string_lossy().into_owned())
                .collect();
            // Default to the last word rather than to zero: it makes a hand
            // invocation (`salvor -- salvor history ''`) behave the way a
            // reader expects, and both generated shell functions set the
            // variable anyway.
            let index = std::env::var(INDEX_VAR)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_else(|| words.len().saturating_sub(1));
            let mut out = String::new();
            for candidate in candidates(&words, index) {
                out.push_str(&candidate);
                out.push('\n');
            }
            out
        }
    }
}

/// The arguments after the first `--`, or `None` when there is no `--` at all.
///
/// `None` and `Some(&[])` mean different things here: no `--` is the
/// registration call, while `--` with nothing after it is a completion of an
/// empty command line.
fn words_after_escape(argv: &[OsString]) -> Option<&[OsString]> {
    let escape = argv.iter().position(|arg| arg == "--")?;
    Some(&argv[escape + 1..])
}

/// The shell function that registers `salvor` for dynamic completion, for the
/// shells this supports.
///
/// An unsupported shell gets an empty script and one line on stderr naming the
/// path that does cover it. This is the one place the completion machinery
/// speaks, and it is deliberate: a registration runs once, when a person is
/// setting completion up and can act on what they read. The Tab-time path
/// (everything reached through a `--`) never comes here and never says
/// anything at all.
fn registration(shell: &str) -> String {
    let completer = shell_quote(&completer_path());
    match shell {
        // `${(@f)...}` splits the binary's stdout on newlines into an array,
        // and `compadd` (rather than zsh's `_describe`) is what takes those
        // strings as literal candidates: it quotes a value containing a space
        // — `graph run` is one — when it inserts it on the command line, and
        // it never reads a `:` in a value as the start of a description.
        "zsh" => ZSH_REGISTRATION.replace("COMPLETER", &completer),
        // `read -r` into an array rather than word-splitting the output: a
        // candidate is one line, whatever whitespace or glob character it
        // contains. `bashdefault` and `default` mean bash's own filename
        // completion still runs when this offers nothing.
        "bash" => BASH_REGISTRATION.replace("COMPLETER", &completer),
        _ => {
            eprintln!(
                "salvor: dynamic completion covers zsh and bash; for {shell}, use `salvor \
                 completions {shell}`"
            );
            String::new()
        }
    }
}

/// The zsh registration script, with `COMPLETER` still to be substituted.
///
/// stderr is discarded on the call back into the binary: belt and braces
/// against anything on that path ever speaking, since the module's own
/// guarantee is that it does not.
const ZSH_REGISTRATION: &str = r#"#compdef salvor
function _salvor_dynamic_completion() {
    local -a candidates
    candidates=("${(@f)$( \
        _SALVOR_COMPLETE_INDEX="$((CURRENT - 1))" \
        COMPLETE="zsh" \
        COMPLETER -- "${words[@]}" 2>/dev/null \
    )}")
    # Empty output splits to a single empty element, which is "no candidates",
    # not "one candidate that is the empty string".
    if [[ ${#candidates[@]} -eq 1 && -z "${candidates[1]}" ]]; then
        return 1
    fi
    compadd -- "${candidates[@]}"
}

compdef _salvor_dynamic_completion salvor
"#;

/// The bash registration script, with `COMPLETER` still to be substituted.
const BASH_REGISTRATION: &str = r#"_salvor_dynamic_completion() {
    local candidates=()
    local line
    while IFS= read -r line; do
        candidates+=("$line")
    done < <( \
        _SALVOR_COMPLETE_INDEX="$COMP_CWORD" \
        COMPLETE="bash" \
        COMPLETER -- "${COMP_WORDS[@]}" 2>/dev/null \
    )
    COMPREPLY=("${candidates[@]}")
}

complete -o bashdefault -o default -F _salvor_dynamic_completion salvor
"#;

/// The path the registration script should call back into: this very binary.
///
/// Resolved against the current directory when it is a relative path with a
/// directory component (`./target/debug/salvor`), because the completion
/// function will run later, from wherever the user happens to be. A bare
/// `salvor` found on `PATH` is left bare, so the script keeps working after an
/// upgrade moves the binary.
fn completer_path() -> String {
    let raw = std::env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("salvor"));
    let mut path = PathBuf::from(raw);
    if path.components().count() > 1
        && path.is_relative()
        && let Ok(current) = std::env::current_dir()
    {
        path = current.join(path);
    }
    path.to_string_lossy().into_owned()
}

/// Wraps a string so a shell reads it as one literal word.
///
/// Single quotes disable every expansion in both bash and zsh; an embedded
/// single quote is closed, escaped, and reopened, which is the only case the
/// quoting has to think about.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// The candidates for `words` with the cursor in word `index`.
///
/// `words[0]` is the command name as typed, matching what both shells hand
/// over, so the walk starts at index 1.
fn candidates(words: &[String], index: usize) -> Vec<String> {
    // Completing the command name itself is the shell's job, not ours.
    if index == 0 {
        return Vec::new();
    }
    let current = words.get(index).map(String::as_str).unwrap_or("");
    let preceding = &words[1..index.min(words.len())];

    let mut command = <crate::cli::Cli as CommandFactory>::command();
    // Resolves `num_args` from each argument's action and copies `--store`
    // (declared `global`) onto every subcommand, so the walk below can ask any
    // command whether it knows a flag. See `manifest::build` for the same call
    // and the same reason.
    command.build();

    let walk = walk(&command, preceding);
    let store = store_path(walk.store.as_deref());

    let mut out = if let Some(arg) = walk.pending_value {
        // The previous word was a flag that takes a value, so this word is
        // that value and nothing else.
        value_candidates(walk.command, arg, current, &store)
    } else if let Some((flag, partial)) = inline_value(current) {
        // `--store=part`: complete the value, but hand back the whole word,
        // since that is what the shell will replace.
        match find_long(walk.command, flag) {
            Some(arg) => value_candidates(walk.command, arg, partial, &store)
                .into_iter()
                .map(|value| format!("--{flag}={value}"))
                .collect(),
            None => Vec::new(),
        }
    } else if current.starts_with('-') {
        flag_candidates(walk.command, current)
    } else {
        let mut out: Vec<String> = walk
            .command
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name().to_owned())
            .filter(|name| name.starts_with(current))
            .collect();
        if let Some(arg) = positional_at(walk.command, walk.positionals) {
            out.extend(value_candidates(walk.command, arg, current, &store));
        }
        out
    };

    out.truncate(MAX_CANDIDATES);
    out
}

/// What walking the already-typed words established.
struct Walk<'a> {
    /// The deepest subcommand named so far: `salvor graph run` walks to `run`.
    command: &'a clap::Command,
    /// How many positional values that command has already been given, which
    /// is the index of the positional the cursor is on.
    positionals: usize,
    /// The flag whose value the cursor is sitting on, when the last typed word
    /// was a flag that takes one.
    pending_value: Option<&'a clap::Arg>,
    /// A `--store` value typed on this very command line, which outranks the
    /// environment exactly as it does when the command actually runs.
    store: Option<String>,
}

/// Walks the words already typed, resolving the subcommand, the position
/// within it, and whether the cursor is on a flag's value.
///
/// This is a deliberately shallow reading of the command line: it does not
/// validate, and it never refuses. Completion runs on a half-typed, frequently
/// wrong command line, where clap's parser would (correctly, for its purpose)
/// return an error rather than a best guess.
fn walk<'a>(command: &'a clap::Command, preceding: &[String]) -> Walk<'a> {
    let mut walk = Walk {
        command,
        positionals: 0,
        pending_value: None,
        store: None,
    };
    let mut store_next = false;

    for word in preceding {
        if walk.pending_value.take().is_some() {
            if store_next {
                walk.store = Some(word.clone());
                store_next = false;
            }
            continue;
        }
        if let Some((flag, value)) = inline_value(word) {
            if flag == "store" {
                walk.store = Some(value.to_owned());
            }
            continue;
        }
        if let Some(long) = word.strip_prefix("--") {
            if long.is_empty() {
                continue;
            }
            if let Some(arg) = find_long(walk.command, long)
                && takes_value(arg)
            {
                walk.pending_value = Some(arg);
                store_next = long == "store";
            }
            continue;
        }
        if word.len() > 1 && word.starts_with('-') {
            // A short-flag cluster: only its last letter can take a value.
            if let Some(last) = word.chars().last()
                && let Some(arg) = find_short(walk.command, last)
                && takes_value(arg)
            {
                walk.pending_value = Some(arg);
            }
            continue;
        }
        if let Some(sub) = walk.command.find_subcommand(word) {
            walk.command = sub;
            walk.positionals = 0;
        } else {
            walk.positionals += 1;
        }
    }

    walk
}

/// The positional argument the cursor sits on, given how many positional values
/// the command has already been handed.
///
/// Past the last declared positional the answer is normally nothing. The
/// exception is a REPEATABLE positional (`salvor agent hash a.toml b.toml`),
/// which keeps accepting values at its own slot: the cursor stays on it however
/// many have been typed, so the second file completes exactly like the first.
fn positional_at(command: &clap::Command, index: usize) -> Option<&clap::Arg> {
    let positionals: Vec<&clap::Arg> = command.get_positionals().collect();
    if let Some(arg) = positionals.get(index) {
        return Some(arg);
    }
    positionals.last().copied().filter(|arg| {
        arg.get_num_args()
            .is_some_and(|range| range.max_values() > 1)
    })
}

/// Splits `--flag=value` into its two halves, or `None` for anything else.
fn inline_value(word: &str) -> Option<(&str, &str)> {
    word.strip_prefix("--")?.split_once('=')
}

/// Finds a long flag on a command, including the globals `build()` copied onto
/// it.
fn find_long<'a>(command: &'a clap::Command, long: &str) -> Option<&'a clap::Arg> {
    command
        .get_arguments()
        .find(|arg| arg.get_long() == Some(long))
}

/// Finds a short flag on a command.
fn find_short(command: &clap::Command, short: char) -> Option<&clap::Arg> {
    command
        .get_arguments()
        .find(|arg| arg.get_short() == Some(short))
}

/// Whether an argument takes a value on the command line, as opposed to being
/// a boolean flag.
fn takes_value(arg: &clap::Arg) -> bool {
    arg.get_num_args().is_some_and(|range| range.takes_values())
}

/// The flags of a command whose long form starts with what is typed.
fn flag_candidates(command: &clap::Command, current: &str) -> Vec<String> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| arg.get_long())
        .map(|long| format!("--{long}"))
        .filter(|flag| flag.starts_with(current))
        .collect()
}

/// The candidate values for one argument.
///
/// The order of the arms is the order of certainty. An enumerable set (a
/// `ValueEnum`, or a parser that implements `possible_values`, as `salvor list
/// --status` and `--group` do) is the truth about what the flag accepts, so it
/// wins. Below that sit the two store-backed kinds, and below those the
/// filesystem, which is a guess about a path an operator is typing.
fn value_candidates(
    command: &clap::Command,
    arg: &clap::Arg,
    current: &str,
    store: &Path,
) -> Vec<String> {
    let possible = arg.get_possible_values();
    if !possible.is_empty() {
        return possible
            .iter()
            .filter(|value| !value.is_hide_set())
            .map(|value| value.get_name().to_owned())
            .filter(|value| value.starts_with(current))
            .collect();
    }

    // Every verb that takes a run id names the field `run_id`, so one test
    // covers `history`, `replay`, `resume`, `abandon`, `resolve`, and `fork`,
    // and a seventh verb gets completion by naming its field the same way.
    if arg.get_id() == "run_id" {
        return run_ids(store, current);
    }
    // `--agent` means two different things: an agent IDENTITY to match on
    // `salvor list`, and a definition FILE everywhere else. The command it
    // hangs off is what tells them apart.
    if arg.get_id() == "agent" && command.get_name() == "list" {
        return agent_identities(store, current);
    }

    match value_name(arg) {
        Some("DIR") => path_candidates(current, true),
        Some("FILE" | "PATH") => path_candidates(current, false),
        // `JSON|@FILE` is a JSON literal or an `@path`. Only the second is
        // completable, and the `@` is the operator saying which one they meant.
        Some("JSON|@FILE") => match current.strip_prefix('@') {
            Some(partial) => path_candidates(partial, false)
                .into_iter()
                .map(|path| format!("@{path}"))
                .collect(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The placeholder clap prints for an argument's value (`PATH`, `RUN_ID`).
fn value_name(arg: &clap::Arg) -> Option<&str> {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(clap::builder::Str::as_str)
}

/// Filesystem paths starting with `current`, directories marked with a
/// trailing slash.
///
/// Unreadable directories yield nothing, like every other failure here.
fn path_candidates(current: &str, directories_only: bool) -> Vec<String> {
    let (directory, partial) = match current.rsplit_once('/') {
        Some((directory, partial)) => (format!("{directory}/"), partial),
        None => (String::new(), current),
    };
    let read = if directory.is_empty() {
        std::fs::read_dir(".")
    } else {
        std::fs::read_dir(&directory)
    };
    let Ok(entries) = read else {
        return Vec::new();
    };

    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(partial) {
                return None;
            }
            // A dotfile is offered only once the operator has typed the dot,
            // matching what every shell's own completion does.
            if name.starts_with('.') && !partial.starts_with('.') {
                return None;
            }
            let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if directories_only && !is_directory {
                return None;
            }
            Some(if is_directory {
                format!("{directory}{name}/")
            } else {
                format!("{directory}{name}")
            })
        })
        .collect();
    out.sort();
    out.truncate(MAX_CANDIDATES);
    out
}

/// The store to complete against: `--store` on the line being typed, else
/// `SALVOR_STORE`, else `./salvor.db`.
///
/// The same precedence `cli::Cli` declares, re-stated here because the parser
/// never runs on this path: a half-typed command line is not something clap can
/// be asked to resolve.
fn store_path(typed: Option<&str>) -> PathBuf {
    if let Some(typed) = typed {
        return PathBuf::from(typed);
    }
    if let Some(from_env) = std::env::var_os("SALVOR_STORE") {
        return PathBuf::from(from_env);
    }
    PathBuf::from("./salvor.db")
}

/// Run ids starting with `current`, most recently active first.
///
/// Filtered across every run in the store, not just the ones read for
/// identities: `list_runs` is one grouped query, so the whole set is cheap.
fn run_ids(store: &Path, current: &str) -> Vec<String> {
    let current = current.to_owned();
    within_budget(store, move |store, runtime| {
        let mut summaries = runtime.block_on(store.list_runs()).ok()?;
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_recorded_at));
        Some(
            summaries
                .into_iter()
                .map(|summary| summary.run_id.as_uuid().to_string())
                .filter(|id| id.starts_with(&current))
                .take(MAX_CANDIDATES)
                .collect(),
        )
    })
    .flatten()
    .unwrap_or_default()
}

/// The distinct agent identities among the most recent [`MAX_LOGS_READ`] runs,
/// starting with `current`.
///
/// One full log read per run, because an identity is not a stored column: it
/// is the `agent_def_hash` on a run's first event (or the literal `graph run`
/// for a graph). See [`MAX_LOGS_READ`] for why that read is capped and why the
/// cap is where it is.
fn agent_identities(store: &Path, current: &str) -> Vec<String> {
    let current = current.to_owned();
    within_budget(store, move |store, runtime| {
        let mut summaries = runtime.block_on(store.list_runs()).ok()?;
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_recorded_at));
        summaries.truncate(MAX_LOGS_READ);
        // Sorted and de-duplicated: many runs share one agent, and an operator
        // filtering by agent wants the set, not the tally.
        let mut identities = BTreeSet::new();
        for summary in summaries {
            let Ok(log) = runtime.block_on(store.read_log(summary.run_id)) else {
                continue;
            };
            let identity = crate::commands::agent_identity(&log);
            if !identity.is_empty() && identity.starts_with(&current) {
                identities.insert(identity);
            }
        }
        Some(identities.into_iter().take(MAX_CANDIDATES).collect())
    })
    .flatten()
    .unwrap_or_default()
}

/// Runs `work` against the store on a worker thread, giving up after
/// [`STORE_BUDGET`].
///
/// `None` means "no answer": no store file, a store that would not open, work
/// that failed, or a deadline that passed. Every one of those is the same
/// thing to a shell, and none of them is worth a word to the operator.
///
/// The existence check is not an optimisation. `SqliteStore::open` goes
/// through SQLite's `Connection::open`, which CREATES the database file when
/// it is absent — so without this, pressing Tab in a directory with no store
/// would leave a stray empty `salvor.db` behind in it.
///
/// The worker is abandoned rather than joined when the deadline passes. The
/// process exits immediately afterwards, and the abandoned thread only ever
/// reads, so there is nothing to leave half-written.
fn within_budget<T: Send + 'static>(
    store: &Path,
    work: impl FnOnce(&SqliteStore, &tokio::runtime::Runtime) -> T + Send + 'static,
) -> Option<T> {
    if !store.is_file() {
        return None;
    }
    let path = store.to_path_buf();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("salvor-completion".to_owned())
        .spawn(move || {
            // A private current-thread runtime: the store's API is async, and
            // this thread is not part of any runtime the CLI otherwise uses.
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
                return;
            };
            let Ok(store) = SqliteStore::open(&path) else {
                return;
            };
            let _ = sender.send(work(&store, &runtime));
        })
        .ok()?;
    receiver.recv_timeout(STORE_BUDGET).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine, with no store in reach: what a bare command line offers.
    fn complete(line: &[&str], index: usize) -> Vec<String> {
        let words: Vec<String> = line.iter().map(|word| (*word).to_owned()).collect();
        candidates(&words, index)
    }

    #[test]
    fn verbs_complete_at_the_top_level() {
        let offered = complete(&["salvor", "re"], 1);
        assert!(offered.contains(&"replay".to_owned()), "{offered:?}");
        assert!(offered.contains(&"resume".to_owned()), "{offered:?}");
        assert!(!offered.contains(&"list".to_owned()), "{offered:?}");
    }

    #[test]
    fn nested_verbs_complete_under_graph() {
        let offered = complete(&["salvor", "graph", ""], 2);
        assert!(offered.contains(&"edit".to_owned()), "{offered:?}");
        assert!(offered.contains(&"validate".to_owned()), "{offered:?}");
        assert!(offered.contains(&"run".to_owned()), "{offered:?}");
    }

    /// The built tree the parser itself runs on, for the tests that ask a
    /// question about an argument rather than about a candidate list.
    fn built_command() -> clap::Command {
        let mut command = <crate::cli::Cli as CommandFactory>::command();
        command.build();
        command
    }

    /// A file positional that repeats has to keep completing: `salvor agent
    /// hash` takes any number of definitions, so the cursor past the first is
    /// still on the same argument. A positional that takes exactly one value
    /// (every run id in the CLI) must NOT behave that way, or a stray extra
    /// word would be offered completions it can never accept.
    #[test]
    fn a_repeatable_positional_keeps_completing_past_the_first_value() {
        let command = built_command();

        let hash = command
            .find_subcommand("agent")
            .and_then(|agent| agent.find_subcommand("hash"))
            .expect("`agent hash` is in the tree");
        let first = positional_at(hash, 0).expect("the first file is a positional");
        assert_eq!(value_name(first), Some("FILE"));
        for already_typed in [1, 2, 7] {
            assert_eq!(
                positional_at(hash, already_typed).map(clap::Arg::get_id),
                Some(first.get_id()),
                "the file positional repeats, so {already_typed} values in the cursor is still on it"
            );
        }

        let history = command
            .find_subcommand("history")
            .expect("`history` is in the tree");
        assert!(positional_at(history, 0).is_some(), "the run id");
        assert!(
            positional_at(history, 1).is_none(),
            "a run id takes exactly one value, so there is no second positional to complete"
        );
    }

    /// A FILE positional gets path completion from its value name alone, which
    /// is the whole reason the new verb needed nothing added here.
    #[test]
    fn an_agent_definition_path_completes_from_the_filesystem() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("research.toml"), "model = \"m\"").expect("write");
        std::fs::create_dir(directory.path().join("agents")).expect("mkdir");
        let prefix = format!("{}/", directory.path().display());

        let command = built_command();
        let hash = command
            .find_subcommand("agent")
            .and_then(|agent| agent.find_subcommand("hash"))
            .expect("`agent hash` is in the tree");
        let arg = positional_at(hash, 0).expect("the file positional");
        let offered = value_candidates(hash, arg, &format!("{prefix}re"), Path::new("nothing.db"));

        assert_eq!(
            offered,
            vec![format!("{prefix}research.toml")],
            "{offered:?}"
        );
        // A directory is offered with a trailing slash so the next Tab
        // descends into it, and a FILE position offers directories too.
        let offered = value_candidates(hash, arg, &prefix, Path::new("nothing.db"));
        assert!(
            offered.contains(&format!("{prefix}agents/")),
            "a directory on the way to a file: {offered:?}"
        );
    }

    #[test]
    fn flags_complete_for_the_walked_subcommand() {
        let offered = complete(&["salvor", "list", "--"], 2);
        assert!(offered.contains(&"--status".to_owned()), "{offered:?}");
        assert!(offered.contains(&"--group".to_owned()), "{offered:?}");
        // The global flag reaches every subcommand, exactly as it does when
        // the command runs.
        assert!(offered.contains(&"--store".to_owned()), "{offered:?}");
        assert!(!offered.contains(&"--dry-run".to_owned()), "{offered:?}");
    }

    /// The fixed value sets the parse tree already knows must survive: dynamic
    /// completion is additive, and a flag that could always be completed still
    /// is.
    #[test]
    fn enumerable_values_still_complete() {
        let offered = complete(&["salvor", "list", "--group", ""], 3);
        assert_eq!(offered, vec!["waiting", "progress", "terminal"]);
        let offered = complete(&["salvor", "list", "--group=w"], 2);
        assert_eq!(offered, vec!["--group=waiting"]);
    }

    /// A run id comes from the store, so what it must NOT offer is the thing a
    /// naive walk would fall back to: the verbs and flags of the command it is
    /// already inside.
    #[test]
    fn a_run_id_position_offers_no_verbs_or_flags() {
        let offered = complete(&["salvor", "history", ""], 2);
        assert!(!offered.contains(&"list".to_owned()), "{offered:?}");
        assert!(!offered.contains(&"--json".to_owned()), "{offered:?}");
    }

    #[test]
    fn a_typed_store_flag_outranks_the_environment() {
        let command = {
            let mut command = <crate::cli::Cli as CommandFactory>::command();
            command.build();
            command
        };
        let words = ["--store".to_owned(), "/tmp/other.db".to_owned()];
        let walked = walk(&command, &words);
        assert_eq!(walked.store.as_deref(), Some("/tmp/other.db"));

        let words = ["--store=/tmp/inline.db".to_owned()];
        let walked = walk(&command, &words);
        assert_eq!(walked.store.as_deref(), Some("/tmp/inline.db"));
    }

    /// The degradation property, at the unit level: a path that is not a store
    /// yields no candidates and no panic. The integration suite proves the
    /// same thing through the real binary, where a panic would actually reach
    /// a shell.
    #[test]
    fn a_missing_or_corrupt_store_yields_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let missing = directory.path().join("nothing-here.db");
        assert!(run_ids(&missing, "").is_empty());
        assert!(agent_identities(&missing, "").is_empty());
        assert!(
            !missing.exists(),
            "completing must never create the store it was looking for"
        );

        let corrupt = directory.path().join("corrupt.db");
        std::fs::write(&corrupt, b"this is not a database").expect("write");
        assert!(run_ids(&corrupt, "").is_empty());
        assert!(agent_identities(&corrupt, "").is_empty());
    }

    #[test]
    fn registration_is_written_for_the_shells_it_supports() {
        assert!(registration("zsh").contains("compdef _salvor_dynamic_completion salvor"));
        assert!(registration("bash").contains("complete -o bashdefault"));
        // The note about the static path goes to stderr; the script itself is
        // empty, so nothing an rc file evaluates can misfire.
        assert!(registration("tcsh").is_empty());
    }

    #[test]
    fn a_single_quote_in_the_completer_path_stays_one_word() {
        assert_eq!(
            shell_quote("/opt/o'brien/salvor"),
            r"'/opt/o'\''brien/salvor'"
        );
    }
}
