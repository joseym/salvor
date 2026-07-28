//! The same-render proof, native side, and the generator for its fixtures.
//!
//! One test file with two jobs, so the reference corpus is defined once:
//!
//! - `regenerate` (only when `REGEN_FIXTURES=1`) writes `fixtures/rows/*.json`
//!   and `fixtures/argv/*.json` (the inputs a caller hands across the wasm
//!   boundary) and `fixtures/expected/**` (the text that comes back). Run it to
//!   (re)commit the fixtures:
//!   `REGEN_FIXTURES=1 cargo test -p salvor-cli-wasm --test same_render -- --ignored regenerate`.
//!
//! - The three real tests (always on) each check the same two things for their
//!   half of the surface:
//!
//!   1. The wasm-facing function's output is byte-identical to what calling
//!      `salvor-cli-core` directly produces. This is the divergence guard: a
//!      browser terminal cannot show a table, a help page, or a refusal that
//!      the real CLI would not.
//!   2. That output still equals the committed fixture. This is the drift
//!      guard, and it is the half that catches a change to `salvor-cli-core`
//!      itself: widen a column or rename a status and both sides move
//!      together, so only a pinned expectation notices.
//!
//! The corpus is deliberately wide rather than illustrative. The list tables
//! cover every status label the STATUS column can print (each one takes a
//! different colour branch), an unrecognised label, an empty table, and a wide
//! row. The help pages cover the root, a flat verb, a verb with a positional,
//! and both nested `graph` verbs. The parse cases cover every refusal shape the
//! CLI has, including the two custom `did you mean` tips that a plain
//! `value_parser` would have replaced with clap's string-similarity guess.

use std::fs;
use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use salvor_cli_core::cli::Cli;
use salvor_cli_core::render;
use salvor_cli_wasm::{
    parse_argv_to_json, render_help_to_ansi_string, render_help_to_string,
    render_list_to_plain_string, render_list_to_string,
};
use salvor_replay::{RunId, RunSummary};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

/// `--store`'s help line prints the value of `SALVOR_STORE`, and its default
/// feeds every parsed command, so a set variable would change both the help
/// pages and the parse envelopes out from under the committed fixtures.
///
/// Checked rather than cleared: clearing a process-wide variable from a test
/// running alongside other threads is unsound in this edition, and a loud
/// refusal is more useful than a mysterious diff.
fn assert_clean_env() {
    assert!(
        std::env::var_os("SALVOR_STORE").is_none(),
        "SALVOR_STORE is set; the `--store` help line and the parsed default both echo it, so \
         the committed fixtures cannot match. Unset it and re-run."
    );
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// A fixed run id per row, so the fixtures are byte-stable across
/// regenerations. The last byte varies.
fn run_id(tag: u8) -> RunId {
    RunId::from_uuid(
        Uuid::parse_str(&format!("00000000-0000-4000-8000-0000000000{tag:02x}")).unwrap(),
    )
}

/// A fixed instant, offset by `minutes`, for the same reason.
fn ts(minutes: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_752_566_400 + minutes * 60).unwrap()
}

fn summary(tag: u8, event_count: u64, first: i64, last: i64) -> RunSummary {
    RunSummary {
        run_id: run_id(tag),
        event_count,
        first_recorded_at: ts(first),
        last_recorded_at: ts(last),
    }
}

/// The named reference tables. Each becomes one `rows/<name>.json` input and a
/// plain and an ANSI `expected/list/<name>.*.txt` pair.
fn reference_tables() -> Vec<(&'static str, Vec<(RunSummary, String)>)> {
    let mut tables: Vec<(&'static str, Vec<(RunSummary, String)>)> = Vec::new();

    // A table with no rows is still a header, and the header carries its own
    // styling, so the empty case is not a trivial one.
    tables.push(("empty", Vec::new()));

    // Every label the STATUS column can print, in the order the flag offers
    // them. Each group takes a different colour branch and the two terminal
    // outcomes take one each, so this single table walks every arm of the
    // renderer's style match.
    let every_status: Vec<(RunSummary, String)> = render::STATUS_LABELS
        .iter()
        .enumerate()
        .map(|(i, status)| {
            let tag = u8::try_from(i).unwrap();
            (
                summary(tag, (i as u64 + 1) * 3, i as i64, i as i64 + 5),
                (*status).to_owned(),
            )
        })
        .collect();
    tables.push(("every_status", every_status));

    // A label this build does not recognise renders unstyled rather than
    // miscoloured, and still has to sit in its column.
    tables.push((
        "unknown_status",
        vec![(summary(0xa0, 1, 0, 0), "something-added-later".to_owned())],
    ));

    // A count wide enough to test the right-aligned EVENTS column and a status
    // long enough to fill its own, so the padding is exercised at its edges.
    tables.push((
        "wide",
        vec![
            (
                summary(0xb0, 1_234_567, 0, 60 * 24 * 365),
                "needs-reconciliation".to_owned(),
            ),
            (summary(0xb1, 0, 1, 1), "not-started".to_owned()),
        ],
    ));

    // What an operator actually sees: a handful of runs in different states,
    // oldest first, the order the real `list` handler sorts them into.
    tables.push((
        "mixed",
        vec![
            (summary(0xc1, 12, 0, 4), "completed".to_owned()),
            (summary(0xc2, 3, 2, 2), "awaiting-model".to_owned()),
            (summary(0xc3, 41, 3, 90), "suspended".to_owned()),
            (summary(0xc4, 7, 5, 6), "failed".to_owned()),
        ],
    ));

    tables
}

/// A reference table as the JSON a caller hands across the boundary: the
/// `RunSummary` the store already serializes, with the folded status added.
fn rows_json(rows: &[(RunSummary, String)]) -> String {
    let values: Vec<Value> = rows
        .iter()
        .map(|(summary, status)| {
            let mut value = serde_json::to_value(summary).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("status".to_owned(), json!(status));
            value
        })
        .collect();
    serde_json::to_string_pretty(&values).unwrap() + "\n"
}

/// The named help paths. `""` is the root; a space separates the segments of a
/// nested path.
fn reference_help_paths() -> Vec<(&'static str, &'static str)> {
    vec![
        ("root", ""),
        ("list", "list"),
        ("run", "run"),
        ("fork", "fork"),
        ("serve", "serve"),
        ("graph", "graph"),
        ("graph-validate", "graph validate"),
        ("graph-run", "graph run"),
    ]
}

/// The help page for a path, built by driving `salvor-cli-core`'s own clap tree
/// directly. This is the reference the wasm-facing function is measured
/// against, so it repeats the incantation rather than calling the crate under
/// test: `build()` first, so the global `--store` and the generated
/// `--help`/`--version` have been propagated, then the long form, which is what
/// `--help` prints (`-h` prints the short one).
fn core_help(path: &str, ansi: bool) -> String {
    let mut command = <Cli as CommandFactory>::command();
    command.build();
    for segment in path.split_whitespace() {
        command = command
            .find_subcommand(segment)
            .unwrap_or_else(|| panic!("no subcommand `{segment}` in the path `{path}`"))
            .clone();
    }
    if ansi {
        command.render_long_help().ansi().to_string()
    } else {
        command.render_long_help().to_string()
    }
}

/// The named argv corpus: what a browser terminal's reader would hand over.
/// Every entry carries the program name at index 0, exactly as a shell does.
fn reference_argvs() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("list", vec!["salvor", "list"]),
        (
            "list_filters",
            vec![
                "salvor",
                "--store",
                "/tmp/runs.db",
                "list",
                "--status",
                "completed",
                "--status",
                "failed",
                "--group",
                "terminal",
                "--agent",
                "graph run",
                "--limit",
                "20",
            ],
        ),
        (
            "run",
            vec![
                "salvor",
                "run",
                "--agent",
                "agents/writer.toml",
                "--input",
                "@input.json",
            ],
        ),
        ("run_fixture", vec!["salvor", "run", "--fixture", "demo/"]),
        (
            "resume",
            vec![
                "salvor",
                "resume",
                "00000000-0000-4000-8000-0000000000aa",
                "--agent",
                "a.toml",
                "--agent",
                "b.toml",
                "--graph",
                "flow.json",
                "--input",
                "{\"approved\":true}",
            ],
        ),
        (
            "fork",
            vec![
                "salvor",
                "fork",
                "00000000-0000-4000-8000-0000000000aa",
                "--from-node",
                "settle",
                "--graph",
                "flow.json",
                "--acknowledge-writes",
                "4,7",
                "--dry-run",
            ],
        ),
        (
            "resolve",
            vec![
                "salvor",
                "resolve",
                "00000000-0000-4000-8000-0000000000aa",
                "--output",
                "{\"id\":\"TICKET-1\"}",
            ],
        ),
        (
            "abandon",
            vec![
                "salvor",
                "abandon",
                "00000000-0000-4000-8000-0000000000aa",
                "--reason",
                "husk is dead forever",
            ],
        ),
        ("completions", vec!["salvor", "completions", "zsh"]),
        (
            "history_json",
            vec![
                "salvor",
                "history",
                "00000000-0000-4000-8000-0000000000aa",
                "--json",
            ],
        ),
        (
            "replay",
            vec![
                "salvor",
                "replay",
                "00000000-0000-4000-8000-0000000000aa",
                "--dry-run",
            ],
        ),
        (
            "serve",
            vec![
                "salvor",
                "serve",
                "--bind",
                "0.0.0.0:9000",
                "--auth-token",
                "SALVOR_TOKEN",
                "--dev",
                "--demo-tools",
            ],
        ),
        ("build_install", vec!["salvor", "build", "--install"]),
        (
            "graph_validate",
            vec!["salvor", "graph", "validate", "flow.json"],
        ),
        ("graph_schema", vec!["salvor", "graph", "schema"]),
        (
            "graph_run",
            vec![
                "salvor",
                "graph",
                "run",
                "flow.json",
                "--input",
                "{}",
                "--agent",
                "a.toml",
                "--label",
                "tenant=acme",
            ],
        ),
        // The refusals. The first two are the reason `GroupParser` and
        // `StatusParser` exist at all: a status typed at `--group` (and the
        // reverse) must come back naming the flag that takes it, and for
        // `awaiting-model` clap's own similarity guess would name the WRONG
        // group.
        (
            "group_given_a_status",
            vec!["salvor", "list", "--group", "awaiting-model"],
        ),
        (
            "status_given_a_group",
            vec!["salvor", "list", "--status", "waiting"],
        ),
        (
            "group_given_nonsense",
            vec!["salvor", "list", "--group", "sideways"],
        ),
        ("unknown_flag", vec!["salvor", "list", "--nope"]),
        ("unknown_verb", vec!["salvor", "lst"]),
        ("missing_required", vec!["salvor", "run"]),
        (
            "conflicting_flags",
            vec![
                "salvor",
                "run",
                "--fixture",
                "demo/",
                "--agent",
                "writer.toml",
            ],
        ),
        ("no_verb", vec!["salvor"]),
        ("root_help", vec!["salvor", "--help"]),
        ("subcommand_help", vec!["salvor", "list", "--help"]),
        ("short_help", vec!["salvor", "list", "-h"]),
    ]
}

/// Writes the fixtures. Ignored by default; run explicitly with
/// `REGEN_FIXTURES=1 ... -- --ignored regenerate` to (re)commit them.
#[test]
#[ignore = "generator: run with REGEN_FIXTURES=1 to rewrite committed fixtures"]
fn regenerate() {
    assert_eq!(
        std::env::var("REGEN_FIXTURES").ok().as_deref(),
        Some("1"),
        "refusing to write fixtures without REGEN_FIXTURES=1"
    );
    assert_clean_env();

    let rows_dir = fixtures_dir().join("rows");
    let argv_dir = fixtures_dir().join("argv");
    let list_dir = fixtures_dir().join("expected/list");
    let help_dir = fixtures_dir().join("expected/help");
    let parse_dir = fixtures_dir().join("expected/parse");
    for dir in [&rows_dir, &argv_dir, &list_dir, &help_dir, &parse_dir] {
        fs::create_dir_all(dir).unwrap();
    }

    for (name, rows) in reference_tables() {
        let input = rows_json(&rows);
        fs::write(rows_dir.join(format!("{name}.json")), &input).unwrap();
        fs::write(
            list_dir.join(format!("{name}.plain.txt")),
            render_list_to_plain_string(&input).unwrap(),
        )
        .unwrap();
        fs::write(
            list_dir.join(format!("{name}.ansi.txt")),
            render_list_to_string(&input).unwrap(),
        )
        .unwrap();
    }

    for (name, path) in reference_help_paths() {
        fs::write(
            help_dir.join(format!("{name}.plain.txt")),
            render_help_to_string(path).unwrap(),
        )
        .unwrap();
        fs::write(
            help_dir.join(format!("{name}.ansi.txt")),
            render_help_to_ansi_string(path).unwrap(),
        )
        .unwrap();
    }

    for (name, argv) in reference_argvs() {
        let input = serde_json::to_string_pretty(&argv).unwrap() + "\n";
        fs::write(argv_dir.join(format!("{name}.json")), &input).unwrap();
        let envelope: Value = serde_json::from_str(&parse_argv_to_json(&input).unwrap()).unwrap();
        fs::write(
            parse_dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(&envelope).unwrap() + "\n",
        )
        .unwrap();
    }
}

/// The list table, through the boundary, is the table `salvor-cli-core` draws,
/// and it is still the table that was committed.
#[test]
fn the_list_table_matches_the_core_renderer() {
    assert_clean_env();
    let rows_dir = fixtures_dir().join("rows");
    let list_dir = fixtures_dir().join("expected/list");

    let mut checked = 0usize;
    for (name, rows) in reference_tables() {
        // The committed input equals the reference corpus: an input that
        // drifted would let the expected side agree with the wrong table.
        let input = rows_json(&rows);
        let committed_input = fs::read_to_string(rows_dir.join(format!("{name}.json")))
            .unwrap_or_else(|_| panic!("missing committed rows fixture {name}; run the generator"));
        assert_eq!(
            committed_input, input,
            "committed rows fixture {name} drifted from the reference corpus; regenerate"
        );

        // The divergence guard: the wasm-facing function against a direct call
        // into salvor-cli-core, byte for byte.
        let core_styled = render::list_table(&rows);
        let wasm_styled = render_list_to_string(&input).unwrap();
        assert_eq!(
            wasm_styled, core_styled,
            "the styled table for {name} crossed the boundary changed"
        );

        let core_plain = anstream::adapter::strip_str(&core_styled).to_string();
        let wasm_plain = render_list_to_plain_string(&input).unwrap();
        assert_eq!(
            wasm_plain, core_plain,
            "the plain table for {name} crossed the boundary changed"
        );

        // The drift guard: the renderer itself still draws what was committed.
        assert_eq!(
            fs::read_to_string(list_dir.join(format!("{name}.ansi.txt"))).unwrap(),
            wasm_styled,
            "the styled table for {name} no longer matches the committed fixture; the renderer \
             changed. Regenerate only if the change was intended."
        );
        assert_eq!(
            fs::read_to_string(list_dir.join(format!("{name}.plain.txt"))).unwrap(),
            wasm_plain,
            "the plain table for {name} no longer matches the committed fixture; the renderer \
             changed. Regenerate only if the change was intended."
        );
        checked += 1;
    }

    assert!(checked >= 5, "expected the full reference-table set");
}

/// Help, through the boundary, is the help `salvor-cli-core`'s clap tree
/// renders, and it is still the help that was committed.
#[test]
fn the_help_text_matches_the_core_command_tree() {
    assert_clean_env();
    let help_dir = fixtures_dir().join("expected/help");

    let mut checked = 0usize;
    for (name, path) in reference_help_paths() {
        let wasm_plain = render_help_to_string(path).unwrap();
        let wasm_ansi = render_help_to_ansi_string(path).unwrap();

        assert_eq!(
            wasm_plain,
            core_help(path, false),
            "the plain help for `{path}` crossed the boundary changed"
        );
        assert_eq!(
            wasm_ansi,
            core_help(path, true),
            "the styled help for `{path}` crossed the boundary changed"
        );

        assert_eq!(
            fs::read_to_string(help_dir.join(format!("{name}.plain.txt"))).unwrap(),
            wasm_plain,
            "the plain help for `{path}` no longer matches the committed fixture; the command \
             tree changed. Regenerate only if the change was intended."
        );
        assert_eq!(
            fs::read_to_string(help_dir.join(format!("{name}.ansi.txt"))).unwrap(),
            wasm_ansi,
            "the styled help for `{path}` no longer matches the committed fixture; the command \
             tree changed. Regenerate only if the change was intended."
        );
        checked += 1;
    }

    assert!(checked >= 8, "expected the full reference-help set");
}

/// Parsing, through the boundary, accepts and refuses exactly what
/// `salvor-cli-core`'s clap tree does, gives back clap's own text for a
/// refusal, and still produces the committed envelope.
#[test]
fn the_parse_envelope_matches_the_core_parser() {
    assert_clean_env();
    let argv_dir = fixtures_dir().join("argv");
    let parse_dir = fixtures_dir().join("expected/parse");

    let mut refusals = 0usize;
    for (name, argv) in reference_argvs() {
        let input = serde_json::to_string_pretty(&argv).unwrap() + "\n";
        let committed_input = fs::read_to_string(argv_dir.join(format!("{name}.json")))
            .unwrap_or_else(|_| panic!("missing committed argv fixture {name}; run the generator"));
        assert_eq!(
            committed_input, input,
            "committed argv fixture {name} drifted from the reference corpus; regenerate"
        );

        let envelope: Value = serde_json::from_str(&parse_argv_to_json(&input).unwrap()).unwrap();
        let core = Cli::try_parse_from(&argv);

        match core {
            Ok(cli) => {
                assert_eq!(envelope["ok"], true, "{name} parses for the core parser");
                assert_eq!(
                    envelope["command"]["store"],
                    Value::String(cli.store.display().to_string()),
                    "{name} carries the store the core parser resolved"
                );
            }
            Err(err) => {
                refusals += 1;
                assert_eq!(
                    envelope["ok"], false,
                    "{name} is refused by the core parser"
                );
                let message = &envelope["message"];
                assert_eq!(message["kind"], format!("{:?}", err.kind()), "{name}");
                assert_eq!(message["is_error"], err.use_stderr(), "{name}");
                assert_eq!(message["exit_code"], err.exit_code(), "{name}");
                // The refusal text is clap's own, not a paraphrase of it.
                assert_eq!(
                    message["plain"],
                    Value::String(err.to_string()),
                    "the plain text for {name} crossed the boundary changed"
                );
                assert_eq!(
                    message["ansi"],
                    Value::String(err.render().ansi().to_string()),
                    "the styled text for {name} crossed the boundary changed"
                );
            }
        }

        let committed: Value = serde_json::from_str(
            &fs::read_to_string(parse_dir.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            committed, envelope,
            "the parse envelope for {name} no longer matches the committed fixture; the parse \
             tree changed. Regenerate only if the change was intended."
        );
    }

    assert!(refusals >= 8, "expected the full reference-refusal set");
}

/// The tip is the whole reason `GroupParser` exists, so it gets its own
/// assertion rather than living only inside a fixture nobody reads: the text
/// that reaches the browser must name the flag that takes the value AND the
/// group the status really belongs to, with clap's wrong similarity guess gone.
#[test]
fn the_group_tip_survives_the_boundary() {
    assert_clean_env();
    let argv = ["salvor", "list", "--group", "awaiting-model"];
    let envelope: Value =
        serde_json::from_str(&parse_argv_to_json(&serde_json::to_string(&argv).unwrap()).unwrap())
            .unwrap();
    let plain = envelope["message"]["plain"].as_str().unwrap();

    assert_eq!(
        plain,
        Cli::try_parse_from(argv).unwrap_err().to_string(),
        "the refusal is clap's own text"
    );
    assert!(
        plain.contains("--status awaiting-model"),
        "names the flag that takes it: {plain}"
    );
    assert!(
        plain.contains("--group progress"),
        "names the group the status really lives in: {plain}"
    );
    assert!(
        !plain.contains("similar value exists"),
        "clap's similarity guess must not survive alongside the real answer: {plain}"
    );
}

/// `--version` is deliberately outside the committed corpus, because its text
/// carries the workspace version and every release would rewrite the fixture.
/// It still has to cross the boundary unchanged, so it is checked live against
/// the core parser instead of against a file.
#[test]
fn the_version_line_matches_the_core_parser() {
    assert_clean_env();
    let argv = ["salvor", "--version"];
    let envelope: Value =
        serde_json::from_str(&parse_argv_to_json(&serde_json::to_string(&argv).unwrap()).unwrap())
            .unwrap();
    let err = Cli::try_parse_from(argv).unwrap_err();

    assert_eq!(envelope["message"]["kind"], "DisplayVersion");
    assert_eq!(envelope["message"]["is_error"], false);
    assert_eq!(
        envelope["message"]["plain"],
        Value::String(err.to_string()),
        "the version line crossed the boundary changed"
    );
}
