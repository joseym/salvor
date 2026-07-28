//! A machine-readable description of the `salvor` CLI surface, walked
//! straight off clap's own [`clap::Command`] tree rather than hand-maintained.
//!
//! This exists for a consumer outside this repo (a landing page's demo
//! terminal) that needs to know salvor's verbs and flags without
//! reimplementing them by hand. The checked-in copy at
//! `docs/cli-manifest.json` is asserted byte-for-byte against [`build`]'s
//! output in `tests/cli_manifest.rs`, so a verb or flag added to
//! [`crate::cli`] without regenerating the manifest fails that test.
//!
//! The introspection mirrors [`crate::commands::completions`]: both start from
//! `<Cli as clap::CommandFactory>::command()`, the same built tree the parser
//! itself runs on, so the manifest can never describe a surface other than the
//! real one.

use clap::{ArgAction, CommandFactory};
use serde::Serialize;

/// The whole manifest: the salvor version it was generated from, so a
/// consumer can detect a stale copy, and the root command's full tree.
#[derive(Debug, Serialize)]
pub struct Manifest {
    /// `env!("CARGO_PKG_VERSION")` of the `salvor-cli` crate this was
    /// generated from.
    pub salvor_version: String,
    /// The `salvor` root command and everything under it.
    pub root: CommandManifest,
}

/// One command or subcommand: `salvor` itself, a top-level verb, or a nested
/// one (`salvor graph validate`).
#[derive(Debug, Serialize)]
pub struct CommandManifest {
    /// The verb's name, as typed on the command line (e.g. `"list"`).
    pub name: String,
    /// The help text clap shows for this command.
    pub about: String,
    /// Whether the command is hidden from `--help`. A hidden command still
    /// parses; the manifest records the same fact `--help` would hide.
    pub hidden: bool,
    /// This command's own arguments. Excludes arguments only present because
    /// a parent marked them `global`: those appear once, on the command that
    /// declares them, with `global: true` (see [`ArgManifest::global`]),
    /// rather than copied onto every command that inherits them.
    pub args: Vec<ArgManifest>,
    /// Nested subcommands, in declaration order.
    pub subcommands: Vec<CommandManifest>,
}

/// One argument (flag or positional) of a command.
#[derive(Debug, Serialize)]
pub struct ArgManifest {
    /// The long flag, written as typed (`"--store"`), or `None` for a
    /// positional argument that has no flag at all.
    pub long: Option<String>,
    /// The short flag, written as typed (`"-h"`), or `None` when there is
    /// none.
    pub short: Option<String>,
    /// The placeholder clap shows for the value in `--help` (e.g. `"PATH"`),
    /// or `None` for a boolean flag that takes no value.
    pub value_name: Option<String>,
    /// Whether clap enforces this argument as unconditionally required.
    /// `false` for an argument that is only conditionally required (clap's
    /// `required_unless_present` and similar do not set this flag), which
    /// faithfully reports what clap itself would refuse to parse without,
    /// not what a human would call "required" by reading the help text.
    pub required: bool,
    /// Whether the flag can be passed more than once and accumulate (a
    /// `Vec<_>` field), as opposed to a single value that a repeat overwrites.
    pub repeatable: bool,
    /// Whether this argument is declared `global = true`: available on every
    /// subcommand, not just the one that declares it.
    pub global: bool,
    /// The help text clap shows next to this argument.
    pub help: Option<String>,
    /// The legal values for this argument, when clap can enumerate them (an
    /// enum, or a custom value parser that implements `possible_values`, as
    /// `salvor list --group` and `--status` do). `None` when the value is
    /// free-form text clap cannot enumerate.
    pub possible_values: Option<Vec<String>>,
}

/// Builds the manifest fresh from clap's own parse tree.
///
/// # Regenerating the checked-in copy
///
/// `docs/cli-manifest.json` is checked in and asserted against this
/// function's output by `tests/cli_manifest.rs`. Regenerate it with:
///
/// ```text
/// UPDATE_CLI_MANIFEST=1 cargo test -p salvor-cli --test cli_manifest
/// ```
pub fn build() -> Manifest {
    let mut command = <crate::cli::Cli as CommandFactory>::command();
    // `build()` resolves each argument's real `num_args`/action from its
    // declared `ArgAction` (a plain `bool` field is `SetTrue`, taking no
    // value) and propagates `global` arguments onto every subcommand. Skipping
    // it would leave `num_args` unresolved, and every boolean flag would
    // misreport itself as taking a `true`/`false` value: `is_takes_value_set`
    // falls back to "takes one value" for an argument whose real num_args
    // (zero, for a flag) is only computed here.
    command.build();
    Manifest {
        salvor_version: env!("CARGO_PKG_VERSION").to_owned(),
        root: command_manifest(&command, true),
    }
}

/// Walks one [`clap::Command`] into a [`CommandManifest`], recursing into its
/// subcommands. Shared by the root command and every nested one, so `salvor
/// graph`'s nested `validate`/`schema`/`run` are described the same way the
/// top-level verbs are.
///
/// `is_root` decides which arguments are this command's own: at the root,
/// every argument (global or not) is one it declares; on a subcommand,
/// `build()` has copied every ancestor's `global` argument onto it too, so
/// those copies are dropped there and kept only where they are declared
/// (see [`ArgManifest::global`]), and clap's own injected `--help`/`--version`
/// are dropped everywhere, as boilerplate identical on every command and no
/// part of salvor's own surface.
fn command_manifest(command: &clap::Command, is_root: bool) -> CommandManifest {
    CommandManifest {
        name: command.get_name().to_owned(),
        about: command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
        hidden: command.is_hide_set(),
        args: command
            .get_arguments()
            .filter(|arg| is_root || !arg.is_global_set())
            .filter(|arg| !is_builtin_help_or_version(arg))
            .map(arg_manifest)
            .collect(),
        // `help` keeps its place as a verb (`salvor help` is a real thing to
        // type) but is deliberately a leaf here. `Command::build()` hangs a
        // clone of every sibling verb under it so `salvor help list` resolves,
        // which makes those children help TARGETS aliasing commands described
        // elsewhere in this tree, not commands in their own right. Recursing
        // would list every verb twice, so a consumer building a verb table or
        // a completion pool straight from this file would have to know to
        // filter them back out — and the point of the manifest is that it can
        // be trusted unfiltered.
        subcommands: if is_builtin_help_subcommand(command) {
            Vec::new()
        } else {
            command
                .get_subcommands()
                .map(|subcommand| command_manifest(subcommand, false))
                .collect()
        },
    }
}

/// Whether `command` is the `help` subcommand `Command::build()` injects into
/// every command that has subcommands of its own.
///
/// Matched by name because clap exposes no marker for it: the settings it
/// carries (`DisableHelpSubcommand` and friends) are set on ordinary leaf
/// commands too, so they cannot tell the two apart. That is exact as long as
/// [`crate::cli`] declares no verb of its own called `help`, which it does
/// not, and could not usefully: the name is already taken by the one clap
/// injects.
fn is_builtin_help_subcommand(command: &clap::Command) -> bool {
    command.get_name() == "help"
}

/// Whether `arg` is clap's own injected `-h`/`--help` or `-V`/`--version`,
/// added by `Command::build()` rather than declared in [`crate::cli`].
fn is_builtin_help_or_version(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

/// Walks one [`clap::Arg`] into an [`ArgManifest`].
fn arg_manifest(arg: &clap::Arg) -> ArgManifest {
    // A boolean flag (`ArgAction::SetTrue`, `--dry-run`) carries a
    // clap-derive-assigned value name internally (used only if the action is
    // later overridden), but takes no value on the command line, as `--help`
    // confirms by printing it with no `<PLACEHOLDER>`. Reporting that
    // internal name here would tell a consumer `--dry-run` wants an argument,
    // which is false, so it is gated on whether the argument's resolved
    // `num_args` actually takes one.
    let takes_value = arg.get_num_args().is_some_and(|range| range.takes_values());
    let possible_values = arg.get_possible_values();
    let possible_values = if possible_values.is_empty() {
        None
    } else {
        Some(
            possible_values
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect(),
        )
    };
    ArgManifest {
        long: arg.get_long().map(|long| format!("--{long}")),
        short: arg.get_short().map(|short| format!("-{short}")),
        value_name: takes_value
            .then(|| arg.get_value_names().and_then(|names| names.first()))
            .flatten()
            .map(ToString::to_string),
        required: arg.is_required_set(),
        repeatable: matches!(arg.get_action(), ArgAction::Append | ArgAction::Count),
        global: arg.is_global_set(),
        help: arg.get_help().map(ToString::to_string),
        possible_values,
    }
}
