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
//! both nested groups, and the nested verbs under them. The parse cases cover
//! every refusal shape the CLI has, including the two custom `did you mean`
//! tips that a plain `value_parser` would have replaced with clap's
//! string-similarity guess.

use std::fs;
use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use salvor_cli_core::agent_config::AgentConfig;
use salvor_cli_core::cli::Cli;
use salvor_cli_core::render;
use salvor_cli_wasm::{
    parse_agent_toml_to_json, parse_argv_to_json, render_help_to_ansi_string,
    render_help_to_string, render_history_to_plain_string, render_history_to_string,
    render_list_to_plain_string, render_list_to_string,
};
use salvor_replay::{Effect, Event, EventEnvelope, RunId, RunSummary, SequenceNumber, TokenUsage};
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

/// One row of a reference list table: the summary, its folded status, and,
/// for a sleeping row, its wake instant. A named alias rather than repeating
/// the triple at every site `reference_tables` and `rows_json` pass it
/// through, which is also what keeps clippy's `type_complexity` lint quiet.
type ReferenceRow = (RunSummary, String, Option<OffsetDateTime>);

/// One named reference table: its fixture name and its rows.
type ReferenceTable = (&'static str, Vec<ReferenceRow>);

/// The named reference tables. Each becomes one `rows/<name>.json` input and a
/// plain and an ANSI `expected/list/<name>.*.txt` pair. The third element of
/// each row is the wake instant a sleeping row carries, or `None` for every
/// other status.
fn reference_tables() -> Vec<ReferenceTable> {
    let mut tables: Vec<ReferenceTable> = Vec::new();

    // A table with no rows is still a header, and the header carries its own
    // styling, so the empty case is not a trivial one.
    tables.push(("empty", Vec::new()));

    // Every label the STATUS column can print, in the order the flag offers
    // them. Each group takes a different colour branch and the two terminal
    // outcomes take one each, so this single table walks every arm of the
    // renderer's style match. The sleeping row alone carries a wake instant,
    // which is what exercises the WAKES AT column's one non-blank cell.
    let every_status: Vec<ReferenceRow> = render::STATUS_LABELS
        .iter()
        .enumerate()
        .map(|(i, status)| {
            let tag = u8::try_from(i).unwrap();
            let wake_at = (*status == "sleeping").then(|| ts(60 * 24));
            (
                summary(tag, (i as u64 + 1) * 3, i as i64, i as i64 + 5),
                (*status).to_owned(),
                wake_at,
            )
        })
        .collect();
    tables.push(("every_status", every_status));

    // A label this build does not recognise renders unstyled rather than
    // miscoloured, and still has to sit in its column.
    tables.push((
        "unknown_status",
        vec![(
            summary(0xa0, 1, 0, 0),
            "something-added-later".to_owned(),
            None,
        )],
    ));

    // A count wide enough to test the right-aligned EVENTS column and a status
    // long enough to fill its own, so the padding is exercised at its edges.
    tables.push((
        "wide",
        vec![
            (
                summary(0xb0, 1_234_567, 0, 60 * 24 * 365),
                "needs-reconciliation".to_owned(),
                None,
            ),
            (summary(0xb1, 0, 1, 1), "not-started".to_owned(), None),
        ],
    ));

    // What an operator actually sees: a handful of runs in different states,
    // oldest first, the order the real `list` handler sorts them into.
    tables.push((
        "mixed",
        vec![
            (summary(0xc1, 12, 0, 4), "completed".to_owned(), None),
            (summary(0xc2, 3, 2, 2), "awaiting-model".to_owned(), None),
            (summary(0xc3, 41, 3, 90), "suspended".to_owned(), None),
            (summary(0xc4, 7, 5, 6), "failed".to_owned(), None),
        ],
    ));

    tables
}

/// A reference table as the JSON a caller hands across the boundary: the
/// `RunSummary` the store already serializes, with the folded status added,
/// and, for a sleeping row, its wake instant.
fn rows_json(rows: &[ReferenceRow]) -> String {
    let values: Vec<Value> = rows
        .iter()
        .map(|(summary, status, wake_at)| {
            let mut value = serde_json::to_value(summary).unwrap();
            let object = value.as_object_mut().unwrap();
            object.insert("status".to_owned(), json!(status));
            if let Some(wake_at) = wake_at {
                // The same `time::serde::rfc3339` codec `RunSummary`'s own
                // timestamps go through, so a wake instant is spelled exactly
                // the way STARTED and LAST ACTIVITY already are.
                let formatted =
                    time::serde::rfc3339::serialize(wake_at, serde_json::value::Serializer)
                        .unwrap();
                object.insert("wake_at".to_owned(), formatted);
            }
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
        ("agent", "agent"),
        ("agent-hash", "agent hash"),
        ("agent-validate", "agent validate"),
        ("graph", "graph"),
        ("graph-edit", "graph edit"),
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
            "agent_hash",
            vec!["salvor", "agent", "hash", "agents/writer.toml"],
        ),
        // The repeatable positional, which parses into a vector rather than
        // into one value.
        (
            "agent_hash_many",
            vec![
                "salvor",
                "agent",
                "hash",
                "agents/writer.toml",
                "agents/reviewer.toml",
            ],
        ),
        (
            "agent_validate",
            vec!["salvor", "agent", "validate", "agents/writer.toml"],
        ),
        // The verb with no required argument at all, and the same verb with
        // both of its optional ones: the two ends of an argument list that is
        // entirely optional, which no other verb here has.
        ("graph_edit", vec!["salvor", "graph", "edit"]),
        (
            "graph_edit_opened",
            vec![
                "salvor",
                "graph",
                "edit",
                "flow.json",
                "--script",
                "session.salvor",
            ],
        ),
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
        // A required POSITIONAL, missing: a different clap arm from the missing
        // required flag above, and the reason `agent hash` cannot be asked to
        // hash nothing.
        (
            "missing_required_positional",
            vec!["salvor", "agent", "hash"],
        ),
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
    let logs_dir = fixtures_dir().join("logs");
    let agents_dir = fixtures_dir().join("agents");
    let list_dir = fixtures_dir().join("expected/list");
    let help_dir = fixtures_dir().join("expected/help");
    let parse_dir = fixtures_dir().join("expected/parse");
    let history_dir = fixtures_dir().join("expected/history");
    let agent_dir = fixtures_dir().join("expected/agent");
    for dir in [
        &rows_dir,
        &argv_dir,
        &logs_dir,
        &agents_dir,
        &list_dir,
        &help_dir,
        &parse_dir,
        &history_dir,
        &agent_dir,
    ] {
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

    for (name, events) in reference_logs() {
        let input = log_json(&events);
        fs::write(logs_dir.join(format!("{name}.json")), &input).unwrap();
        fs::write(
            history_dir.join(format!("{name}.plain.txt")),
            render_history_to_plain_string(&input).unwrap(),
        )
        .unwrap();
        fs::write(
            history_dir.join(format!("{name}.ansi.txt")),
            render_history_to_string(&input).unwrap(),
        )
        .unwrap();
    }

    // The real agent files are COPIED in rather than read through at check
    // time, so the committed corpus holds its own inputs the way the rows and
    // argv corpora do. The copy is then checked against the repository file it
    // came from, which is what keeps a fixture from quietly outliving the file
    // it was supposed to be about.
    for (name, relative) in PINNED_AGENT_FILES {
        let text = fs::read_to_string(repo_root().join(relative)).unwrap();
        fs::write(agents_dir.join(format!("{name}.toml")), &text).unwrap();
    }
    for (name, text) in reference_agent_texts() {
        fs::write(agents_dir.join(format!("{name}.toml")), text).unwrap();
    }
    for name in agent_fixture_names() {
        let text = fs::read_to_string(agents_dir.join(format!("{name}.toml"))).unwrap();
        let envelope: Value =
            serde_json::from_str(&parse_agent_toml_to_json(&text).unwrap()).unwrap();
        fs::write(
            agent_dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(&envelope).unwrap() + "\n",
        )
        .unwrap();
    }
}

/// Every name in the agent corpus: the pinned real files first, then the
/// synthetic ones.
fn agent_fixture_names() -> Vec<String> {
    PINNED_AGENT_FILES
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .chain(
            reference_agent_texts()
                .into_iter()
                .map(|(name, _)| name.to_owned()),
        )
        .collect()
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

// ---------------------------------------------------------------------------
// The history listing
// ---------------------------------------------------------------------------

/// One envelope of a reference log. Fixed run id and a timestamp that advances
/// with the sequence, so the fixtures are byte-stable and the recorded-time
/// column still varies between lines.
fn envelope(seq: u64, event: Event) -> EventEnvelope {
    EventEnvelope::new(
        run_id(0xaa),
        SequenceNumber::new(seq),
        ts(seq as i64),
        event,
    )
}

fn log(events: Vec<Event>) -> Vec<EventEnvelope> {
    events
        .into_iter()
        .enumerate()
        .map(|(i, event)| envelope(i as u64, event))
        .collect()
}

/// The named reference logs. Each becomes one `logs/<name>.json` input and a
/// plain and an ANSI `expected/history/<name>.*.txt` pair.
fn reference_logs() -> Vec<(&'static str, Vec<EventEnvelope>)> {
    let mut logs: Vec<(&'static str, Vec<EventEnvelope>)> = Vec::new();

    // No events is no lines, which the real command reaches only for a run id
    // it refuses first. It is here because an empty listing must be an empty
    // string rather than a stray newline.
    logs.push(("empty", Vec::new()));

    // The hero fixture's own shape, event for event: the ten-event run behind
    // the terminal on salvor.run (`salvor run --fixture examples/hero`). One
    // model call decides to record a claim, one tool call records it, one more
    // model call closes the run out, with a clock observation per loop
    // iteration. This is the log the landing page prints, so it is the log the
    // proof is anchored on.
    logs.push((
        "hero",
        log(vec![
            Event::RunStarted {
                agent_def_hash:
                    "sha256:1f0c6d2a9b3e5477c8d1e0a2b4f60918d3c5e7a9b1d3f50729c4e6a8b0d2f416"
                        .to_owned(),
                input: json!({"item": "ss-waratah"}),
                labels: None,
                driven_by: None,
                caller: None,
            },
            Event::NowObserved { now: ts(0) },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(1),
                request_hash:
                    "sha256:2b4d6f8a0c2e40628a4c6e80a2c4e6081b3d5f79a1c3e50729b4d6f8a0c2e406"
                        .to_owned(),
                request_body: None,
                performed_by: None,
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(2),
                response: json!({"text": "recording the claim"}),
                usage: TokenUsage {
                    input_tokens: 24,
                    output_tokens: 41,
                },
            },
            Event::ToolCallRequested {
                seq: SequenceNumber::new(4),
                tool: "save_claim".to_owned(),
                input: json!({"item": "ss-waratah"}),
                effect: Effect::Write,
                idempotency_key: Some("save_claim:ss-waratah".to_owned()),
                performed_by: None,
            },
            Event::ToolCallCompleted {
                seq: SequenceNumber::new(4),
                output: json!({"content": [{"text": "claim recorded: ss-waratah"}]}),
                deduplicated_from: None,
                settled_by: None,
                settled_caller: None,
            },
            Event::NowObserved { now: ts(1) },
            Event::ModelCallRequested {
                seq: SequenceNumber::new(7),
                request_hash:
                    "sha256:3c5e7a9b1d3f50729c4e6a8b0d2f4160a2c4e6081b3d5f79a1c3e50729b4d6f8"
                        .to_owned(),
                request_body: None,
                performed_by: None,
            },
            Event::ModelCallCompleted {
                seq: SequenceNumber::new(8),
                response: json!({"text": "Recorded the salvage claim for ss-waratah."}),
                usage: TokenUsage {
                    input_tokens: 118,
                    output_tokens: 17,
                },
            },
            Event::RunCompleted {
                output: json!("Recorded the salvage claim for ss-waratah."),
            },
        ]),
    ));

    // The ways a run can stop short, which the hero run never reaches: a
    // budget crossing (the f64 detail path), a suspension and its resume, an
    // abandonment with a reason, and a failure. Each takes a different arm of
    // `event_detail`, so this walks the branches the hero log leaves cold.
    logs.push((
        "parked_and_terminal",
        log(vec![
            Event::RunStarted {
                agent_def_hash: "sha256:aabbcc".to_owned(),
                input: json!({"claim": "wreck-9931"}),
                labels: None,
                driven_by: None,
                caller: None,
            },
            Event::BudgetExceeded {
                budget: salvor_replay::Budget {
                    kind: salvor_replay::BudgetKind::CostUsd,
                    limit: 2.5,
                },
                observed: 2.500_001,
            },
            Event::Resumed {
                input: json!({"extend": {"cost_usd": 1.0}}),
                caller: None,
            },
            Event::Suspended {
                reason: "a human must approve the payout".to_owned(),
                input_schema: json!({"type": "object"}),
                kind: None,
            },
            Event::Resumed {
                input: json!({"approved": true}),
                caller: None,
            },
            Event::RandomObserved {
                value: 17_014_118_346_046_923_173,
            },
            Event::RunFailed {
                error: "the ledger refused the write".to_owned(),
            },
        ]),
    ));

    logs
}

/// A reference log as the JSON a caller hands across the boundary: the exact
/// wire form the store writes, which is also what `salvor-replay-wasm`'s fold
/// takes.
fn log_json(events: &[EventEnvelope]) -> String {
    serde_json::to_string_pretty(events).unwrap() + "\n"
}

/// The history listing built by calling `salvor-cli-core` directly. This is the
/// reference the wasm-facing function is measured against, so it repeats the
/// command handler's own loop (one `render::history_line` per envelope, each on
/// its own line) rather than calling the crate under test.
fn core_history(events: &[EventEnvelope]) -> String {
    let mut out = String::new();
    for envelope in events {
        out.push_str(&render::history_line(envelope));
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// The agent-definition parse
// ---------------------------------------------------------------------------

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every agent file this repository ships, by path from the root.
///
/// These are the real thing: the files `salvor run --agent` is pointed at in
/// the READMEs, the examples, and the SDK walkthroughs. A parse that cannot
/// read them is not the CLI's parse, whatever else it can read.
const REPO_AGENT_FILES: &[&str] = &[
    "demo/agent.toml",
    "examples/hero/agent.toml",
    "examples/local-model/agent.toml",
    "examples/payroll/agents/notify-summary.toml",
    "examples/polyglot-service/agent.toml",
    "examples/python-tools/agent.toml",
    "examples/reconciliation/agent.toml",
    "examples/refine/agents/tailor.toml",
    "examples/support-ops/agent.toml",
    "examples/typescript-tools/agent.toml",
    "examples/wasm-tools/agent.toml",
    "examples/web-research/agent.toml",
    "examples/graph-clients/agents/settle-and-notify.toml",
    "examples/graph-clients/agents/small-claims.toml",
    "examples/graph-service/agents/customer-notice.toml",
    "examples/graph-service/agents/small-claims.toml",
    "sdks/python/example/agent.toml",
    "sdks/typescript/example/agent.toml",
];

/// The real agent files the committed corpus pins, and the fixture name each
/// is copied to. A subset of [`REPO_AGENT_FILES`], chosen for breadth rather
/// than count: the hero's MCP server and budgets, a wasm tool with limits and
/// grants, a graph node's agent, and a file with an `[llm]` section.
const PINNED_AGENT_FILES: &[(&str, &str)] = &[
    ("hero", "examples/hero/agent.toml"),
    ("wasm_tools", "examples/wasm-tools/agent.toml"),
    ("local_model", "examples/local-model/agent.toml"),
    (
        "graph_node",
        "examples/graph-clients/agents/small-claims.toml",
    ),
];

/// The definitions the parse must REFUSE, and the rule each one breaks. The
/// committed expectation for these is the CLI's own message, so a reworded
/// refusal is a visible diff rather than a silent change to what a page shows
/// somebody who mistyped their file.
///
/// One synthetic acceptance rides along (`keys`): no agent file in this
/// repository declares an idempotency key, so without it the path parse the
/// schema reaches into `salvor-tools` for would be uncovered.
fn reference_agent_texts() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "keys",
            "model = \"m\"\n\n\
             [[mcp_servers]]\n\
             command = \"payouts\"\n\
             idempotency_keys = { pay_claim = \"claim_id\", refund = \"payment.charge_id\" }\n",
        ),
        (
            "output_schema",
            "model = \"m\"\n\n\
             [output_schema]\n\
             type = \"object\"\n\
             required = [\"score\"]\n\n\
             [output_schema.properties.score]\n\
             type = \"number\"\n",
        ),
        (
            "bad_both_output_schemas",
            "model = \"m\"\n\
             output_schema_path = \"answer.json\"\n\n\
             [output_schema]\n\
             type = \"object\"\n",
        ),
        (
            "bad_unknown_field",
            "model = \"claude-opus-4-8\"\n\
             [budgets]\n\
             step = 3\n",
        ),
        (
            "bad_both_prompts",
            "model = \"m\"\n\
             system_prompt = \"inline\"\n\
             system_prompt_path = \"prompt.txt\"\n",
        ),
        (
            "bad_no_transport",
            "model = \"m\"\n\n[[mcp_servers]]\nargs = [\"x\"]\n",
        ),
        (
            "bad_both_transports",
            "model = \"m\"\n\n\
             [[mcp_servers]]\n\
             command = \"x\"\n\
             url = \"https://example.com/mcp\"\n",
        ),
        (
            "bad_wasm_without_effect",
            "model = \"m\"\n\n\
             [[wasm_tools]]\n\
             path = \"t.wasm\"\n\
             name = \"t\"\n\
             description = \"d\"\n\
             input_schema = \"{}\"\n",
        ),
        (
            "bad_idempotency_path",
            "model = \"m\"\n\n\
             [[mcp_servers]]\n\
             command = \"x\"\n\
             idempotency_keys = { pay_claim = \"a..b\" }\n",
        ),
        ("bad_not_toml", "model = \"m\"\nthis is not toml at all\n"),
    ]
}

/// The agent envelope built by calling `salvor-cli-core` directly, so the
/// wasm-facing function is measured against the parse rather than against
/// itself.
fn core_agent_envelope(text: &str) -> Value {
    match AgentConfig::from_toml_str(text) {
        Ok(config) => json!({"ok": true, "config": serde_json::to_value(&config).unwrap()}),
        Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
    }
}

/// The history listing, through the boundary, is the listing
/// `salvor-cli-core` writes, and it is still the listing that was committed.
///
/// The hero log is the one the landing page prints, so this is the assertion
/// that a page showing `salvor history` is showing the CLI's own lines: the
/// sequence column's width, the timestamp's spelling, the kind label, and the
/// per-event detail all come from `render::history_line` and the two renderers
/// in `salvor-replay` it delegates to.
#[test]
fn the_history_listing_matches_the_core_renderer() {
    assert_clean_env();
    let logs_dir = fixtures_dir().join("logs");
    let history_dir = fixtures_dir().join("expected/history");

    let mut checked = 0usize;
    for (name, events) in reference_logs() {
        // The committed input equals the reference corpus: an input that
        // drifted would let the expected side agree with the wrong log.
        let input = log_json(&events);
        let committed_input = fs::read_to_string(logs_dir.join(format!("{name}.json")))
            .unwrap_or_else(|_| panic!("missing committed log fixture {name}; run the generator"));
        assert_eq!(
            committed_input, input,
            "committed log fixture {name} drifted from the reference corpus; regenerate"
        );

        // The divergence guard: the wasm-facing function against a direct call
        // into salvor-cli-core, byte for byte.
        let core = core_history(&events);
        let wasm = render_history_to_string(&input).unwrap();
        assert_eq!(
            wasm, core,
            "the history listing for {name} crossed the boundary changed"
        );

        // The history renderer emits no styling today, so the two forms are
        // equal. Asserted rather than assumed: this is what would notice if it
        // ever grew a styled column, and the plain form stopped being plain.
        let plain = render_history_to_plain_string(&input).unwrap();
        assert_eq!(
            plain, wasm,
            "the history renderer is unstyled, so its two forms agree"
        );
        assert!(
            !wasm.contains('\u{1b}'),
            "the history listing for {name} carries no escape codes"
        );

        // The drift guard: the renderer itself still writes what was committed.
        assert_eq!(
            fs::read_to_string(history_dir.join(format!("{name}.ansi.txt"))).unwrap(),
            wasm,
            "the history listing for {name} no longer matches the committed fixture; the \
             renderer changed. Regenerate only if the change was intended."
        );
        assert_eq!(
            fs::read_to_string(history_dir.join(format!("{name}.plain.txt"))).unwrap(),
            plain,
            "the plain history listing for {name} no longer matches the committed fixture; the \
             renderer changed. Regenerate only if the change was intended."
        );
        checked += 1;
    }

    assert!(checked >= 3, "expected the full reference-log set");
}

/// Each line of the hero listing is a whole event, not a truncation of one.
///
/// The listing test above compares two implementations, which would agree even
/// if both were wrong. This says what the right answer looks like: ten lines
/// for ten events, in log order, each opening with its own sequence number in
/// the four-column gutter, and the write's effect class visible on the line
/// that records it.
#[test]
fn the_hero_listing_reads_like_the_command_prints_it() {
    assert_clean_env();
    let (_, events) = reference_logs()
        .into_iter()
        .find(|(name, _)| *name == "hero")
        .expect("the hero log is in the corpus");
    let listing = render_history_to_string(&log_json(&events)).unwrap();

    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(lines.len(), 10, "ten events, ten lines: {listing}");
    for (seq, line) in lines.iter().enumerate() {
        assert!(
            line.starts_with(&format!("{seq:>4}  ")),
            "line {seq} opens with its sequence number: {line}"
        );
    }
    assert!(lines[0].contains("RunStarted"), "{}", lines[0]);
    assert!(
        lines[4].contains("save_claim") && lines[4].contains("Write"),
        "the write names its tool and its effect class: {}",
        lines[4]
    );
    assert!(lines[9].contains("RunCompleted"), "{}", lines[9]);
}

/// Parsing an agent definition, through the boundary, accepts and refuses
/// exactly what `salvor-cli-core` does, gives back the CLI's own message for a
/// refusal, and still produces the committed envelope.
#[test]
fn the_agent_parse_matches_the_core_parser() {
    assert_clean_env();
    let agents_dir = fixtures_dir().join("agents");
    let agent_dir = fixtures_dir().join("expected/agent");

    let mut accepted = 0usize;
    let mut refused = 0usize;
    for name in agent_fixture_names() {
        let text =
            fs::read_to_string(agents_dir.join(format!("{name}.toml"))).unwrap_or_else(|_| {
                panic!("missing committed agent fixture {name}; run the generator")
            });

        // The divergence guard: the wasm-facing function against a direct call
        // into salvor-cli-core.
        let envelope: Value =
            serde_json::from_str(&parse_agent_toml_to_json(&text).unwrap()).unwrap();
        assert_eq!(
            envelope,
            core_agent_envelope(&text),
            "the agent envelope for {name} crossed the boundary changed"
        );

        if envelope["ok"] == Value::Bool(true) {
            accepted += 1;
        } else {
            refused += 1;
            // The refusal text is the CLI's own, not a paraphrase: the same
            // string `salvor agent validate` prints, context chain included.
            let error = AgentConfig::from_toml_str(&text).unwrap_err();
            assert_eq!(
                envelope["error"],
                Value::String(format!("{error:#}")),
                "the refusal for {name} crossed the boundary changed"
            );
        }

        // The drift guard: the parse still produces what was committed.
        let committed: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            committed, envelope,
            "the agent envelope for {name} no longer matches the committed fixture; the schema \
             changed. Regenerate only if the change was intended."
        );
    }

    assert!(
        accepted >= 5,
        "expected the pinned real files plus the keys case"
    );
    assert!(refused >= 6, "expected the full reference-refusal set");
}

/// The pinned fixtures are still copies of the repository's own agent files.
///
/// Without this the corpus could pass forever against a file that no longer
/// exists in the form anyone ships, which would make the accept side of the
/// proof about nothing.
#[test]
fn the_pinned_agent_fixtures_are_the_repository_files() {
    let agents_dir = fixtures_dir().join("agents");
    for (name, relative) in PINNED_AGENT_FILES {
        let committed =
            fs::read_to_string(agents_dir.join(format!("{name}.toml"))).unwrap_or_else(|_| {
                panic!("missing committed agent fixture {name}; run the generator")
            });
        let real = fs::read_to_string(repo_root().join(relative))
            .unwrap_or_else(|_| panic!("{relative} is gone; the fixture points at nothing"));
        assert_eq!(
            committed, real,
            "the committed copy of {relative} drifted from the file itself; regenerate"
        );
    }
}

/// Every agent file this repository ships parses.
///
/// The pinned corpus proves the parse is byte-identical across the boundary;
/// this proves it is the CLI's parse in the sense that matters to somebody
/// reading the docs, which is that the files those docs tell them to run are
/// files it accepts. Checked live against the repository rather than against a
/// fixture, so a new example that the parse would refuse fails here.
#[test]
fn every_agent_file_in_the_repository_parses() {
    let root = repo_root();
    for relative in REPO_AGENT_FILES {
        let path = root.join(relative);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{relative} is listed here but not in the repository"));
        let parsed = parse_agent_toml_to_json(&text).unwrap();
        let envelope: Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(
            envelope["ok"],
            Value::Bool(true),
            "{relative} must parse: {}",
            envelope["error"]
        );
    }
}
