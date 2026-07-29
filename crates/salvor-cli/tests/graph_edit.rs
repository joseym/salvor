//! End to end through the real `salvor` binary for `salvor graph edit`.
//!
//! Every test here drives the verb the way a script does: lines on stdin, which
//! is not a terminal, so there is no prompt and no banner and the whole session
//! is the transcript on stdout with the refusals on stderr.
//!
//! The headline is the property the verb exists for: a document typed a line at
//! a time is a document `salvor graph validate` accepts and `salvor graph run`
//! can drive. The rest pins the seam and the session contract. An `--file` line
//! resolves to the SAME hash `salvor agent hash` prints, and dumps as a
//! `--hash` line that needs no file to replay. A refused line ends a script
//! before the lines that assumed it. End of input is the only way a session
//! ends.
//!
//! The definitions here declare no MCP server and no tools, so every test is
//! offline and needs no API key: a definition builds without one, since nothing
//! is sent.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::tempdir;

/// The smallest definition that builds: a model and nothing else.
const MINIMAL_AGENT: &str = r#"model = "claude-test-model"
"#;

/// A placeholder hash of the shape `validate` requires, for the lines that are
/// not about resolving a file.
fn some_hash() -> String {
    format!("sha256:{}", "1".repeat(64))
}

/// A fresh handle to the `salvor` binary with tracing quieted.
fn salvor() -> Command {
    let mut command = Command::cargo_bin("salvor").expect("salvor binary builds");
    command.env("RUST_LOG", "warn");
    command
}

/// Writes `content` to `dir/name` and returns the path.
fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write");
    path
}

/// One editing session: `lines` on stdin, plus whatever arguments the session
/// needs. Returns the exit code, stdout, and stderr.
fn session(args: &[&str], lines: &str) -> (Option<i32>, String, String) {
    let output = salvor()
        .args(["graph", "edit"])
        .args(args)
        .write_stdin(lines.to_owned())
        .output()
        .expect("runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The headline. A document built one line at a time is a real document: it
/// validates through the sibling verb, with the same node and edge counts the
/// session reported.
#[test]
fn a_session_on_stdin_writes_a_document_the_validate_verb_accepts() {
    let dir = tempdir().expect("tempdir");
    let flow = dir.path().join("flow.json");
    let script = format!(
        "add agent research --hash {hash}\n\
         add gate approve --approval-schema {{\"type\":\"object\"}}\n\
         edge research approve\n\
         validate\n\
         write {}\n",
        flow.display(),
        hash = some_hash(),
    );

    let (code, stdout, stderr) = session(&[], &script);
    assert_eq!(code, Some(0), "every line took: {stderr}");
    assert!(stdout.contains("added agent node `research`"), "{stdout}");
    assert!(stdout.contains("added gate node `approve`"), "{stdout}");
    assert!(
        stdout.contains("added edge `research` -> `approve`"),
        "{stdout}"
    );
    assert!(
        stdout.contains("graph ok: 2 node(s), 1 edge(s)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("wrote {}", flow.display())),
        "the write names the file it wrote: {stdout}"
    );

    // The document the session wrote, through the verb that judges documents.
    let output = salvor()
        .args(["graph", "validate"])
        .arg(&flow)
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "the written document validates: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("graph ok: 2 node(s), 1 edge(s)"),
        "{output:?}"
    );
}

/// The seam. `--file` names a path the editor cannot resolve itself, so the
/// host resolves it, and the number it resolves to has to be the one `salvor
/// agent hash` prints or the node would reference an agent no run could find.
#[test]
fn an_agent_file_resolves_to_the_hash_the_agent_hash_verb_prints() {
    let dir = tempdir().expect("tempdir");
    let agent = write(dir.path(), "research.toml", MINIMAL_AGENT);
    let flow = dir.path().join("flow.json");

    let printed = salvor()
        .args(["agent", "hash"])
        .arg(&agent)
        .output()
        .expect("runs");
    assert!(printed.status.success(), "{printed:?}");
    let printed = String::from_utf8_lossy(&printed.stdout).trim().to_owned();

    let (code, stdout, stderr) = session(
        &[],
        &format!(
            "add agent research --file {}\nhistory\nwrite {}\n",
            agent.display(),
            flow.display(),
        ),
    );
    assert_eq!(code, Some(0), "the file resolves: {stderr}");
    assert!(
        stdout.contains(&format!("{} is {printed}", agent.display())),
        "the session names the hash it resolved to: {stdout}"
    );

    // The document records the hash, never the path.
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&flow).expect("read")).expect("a document");
    assert_eq!(
        document["nodes"][0]["payload"]["agent_hash"],
        serde_json::Value::String(printed.clone()),
        "the node references the resolved hash: {document}"
    );

    // And the dumped history is in the resolved form, so the script replays
    // with no agent definition present.
    assert!(
        stdout.contains(&format!("add agent research --hash {printed}")),
        "history dumps the resolved line: {stdout}"
    );
    assert!(
        !stdout.contains("--file"),
        "no dumped line names a file: {stdout}"
    );
}

/// A dumped script is applied exactly as the lines that produced it were, which
/// is what makes a session something you can check in and pick up again.
#[test]
fn a_script_replays_into_the_document_the_same_lines_typed_produce() {
    let dir = tempdir().expect("tempdir");
    // A branch, the case it routes, and the edge that realizes it: enough
    // shapes that a replay agreeing by accident is not plausible.
    let lines = "# a branch and the case it routes\n\
                 add branch route --on output\n\
                 case route refund --when kind==\"refund\"\n\
                 add tool settle refund_payment\n\
                 edge route settle --label refund\n"
        .to_owned();
    let script = write(dir.path(), "session.salvor", &lines);
    let replayed = dir.path().join("replayed.json");
    let typed = dir.path().join("typed.json");

    let (code, _, stderr) = session(
        &["--script", &script.display().to_string()],
        &format!("write {}\n", replayed.display()),
    );
    assert_eq!(code, Some(0), "the script replays: {stderr}");

    let (code, _, stderr) = session(&[], &format!("{lines}write {}\n", typed.display()));
    assert_eq!(code, Some(0), "the same lines typed: {stderr}");

    assert_eq!(
        fs::read_to_string(&replayed).expect("read"),
        fs::read_to_string(&typed).expect("read"),
        "a replayed script and the lines it was dumped from are one document"
    );
}

/// The positional FILE is the `read <FILE>` the author would type first, so it
/// lands in the history and `undo` steps back across it like any other command.
#[test]
fn an_opened_document_is_the_starting_point_and_undo_steps_back_across_it() {
    let dir = tempdir().expect("tempdir");
    let flow = dir.path().join("flow.json");
    let (code, _, stderr) = session(
        &[],
        &format!(
            "add gate approve --approval-schema {{\"type\":\"object\"}}\nwrite {}\n",
            flow.display()
        ),
    );
    assert_eq!(code, Some(0), "{stderr}");

    let (code, stdout, stderr) = session(&[&flow.display().to_string()], "show\nundo\nshow\n");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.contains("read a document of 1 node(s) and 0 edge(s)"),
        "the file is read before the first line: {stdout}"
    );
    assert!(
        stdout.contains("gate    approve"),
        "the opened document is what the session holds: {stdout}"
    );
    assert!(
        stdout.contains("0 command(s) still recorded"),
        "the read was recorded, so undo has something to take: {stdout}"
    );
    assert!(
        stdout.contains("graph: empty"),
        "undoing the read leaves the empty document it replaced: {stdout}"
    );
}

/// A file that is not a document, and a file that is not there, both end the
/// session before it starts. An editor that opened nothing holds an empty
/// document, and an empty document is what a later `write` would put over the
/// file that was named.
#[test]
fn a_document_that_cannot_be_opened_ends_the_session() {
    let dir = tempdir().expect("tempdir");
    let garbage = write(dir.path(), "notes.txt", "these are not a graph\n");

    let (code, stdout, stderr) = session(&[&garbage.display().to_string()], "show\n");
    assert_eq!(code, Some(1), "opening a non-document refuses: {stdout}");
    assert!(
        stderr.contains("that is not a graph document"),
        "the editor's own message says why: {stderr}"
    );
    assert!(
        !stdout.contains("graph:"),
        "no line after it ran: {stdout:?}"
    );

    let missing = dir.path().join("nowhere.json");
    let (code, _, stderr) = session(&[&missing.display().to_string()], "show\n");
    assert_eq!(code, Some(1), "a missing file refuses: {stderr}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "names the file it could not read: {stderr}"
    );
}

/// A script is a program: the lines after a refused one were written expecting
/// it to have taken, so the first refusal ends the session and the `write` that
/// would have saved a document nobody described never runs.
#[test]
fn a_refused_line_ends_a_script_before_the_lines_that_assumed_it() {
    let dir = tempdir().expect("tempdir");
    let flow = dir.path().join("flow.json");
    let (code, stdout, stderr) = session(
        &[],
        &format!(
            "add agent research --hash {hash}\n\
             rm node reviewer\n\
             add gate approve --approval-schema {{\"type\":\"object\"}}\n\
             write {}\n",
            flow.display(),
            hash = some_hash(),
        ),
    );

    assert_eq!(code, Some(1), "a refused line is a failed session");
    assert!(stdout.contains("added agent node `research`"), "{stdout}");
    assert!(
        stderr.contains("no node `reviewer` in the document"),
        "the refusal is on stderr: {stderr}"
    );
    assert!(
        !stdout.contains("reviewer"),
        "and not on stdout: {stdout:?}"
    );
    assert!(
        !stdout.contains("added gate node"),
        "the line after the refusal never ran: {stdout}"
    );
    assert!(!flow.exists(), "and nothing was written");
}

/// A line that is not a command at all is refused the same way, naming the word
/// that was not one.
#[test]
fn a_line_that_does_not_parse_names_what_broke_it() {
    let (code, stdout, stderr) = session(&[], "sketch a graph\n");
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stderr.contains("unknown command `sketch`"),
        "names the word: {stderr}"
    );
    assert!(
        stderr.contains("the commands are add,"),
        "and offers the ones that are: {stderr}"
    );
}

/// Asking a half-built document to validate is a question answered, not a
/// command refused: a partial document is a legal document while it is being
/// built, so the session carries on and its exit code is unaffected. `salvor
/// graph validate` is the verb that gates on a document's validity.
#[test]
fn validate_reporting_an_invalid_document_is_not_a_failed_session() {
    let (code, stdout, stderr) = session(&[], "edge research approve\nvalidate\nshow\n");
    assert_eq!(code, Some(0), "the session did not fail: {stderr}");
    assert!(
        stdout.contains("references unknown node id `research`"),
        "the validation report is output, on stdout: {stdout}"
    );
    assert!(
        stdout.contains("edges"),
        "and the session carried on past it: {stdout}"
    );
}

/// A bare `write` names no destination, so the document comes back on the
/// stream the session is already writing to.
#[test]
fn a_bare_write_hands_the_document_back_on_stdout() {
    let (code, stdout, stderr) = session(
        &[],
        &format!("add agent research --hash {}\nwrite\n", some_hash()),
    );
    assert_eq!(code, Some(0), "{stderr}");
    let json = stdout
        .find('{')
        .map(|at| stdout[at..].to_owned())
        .expect("the document is on stdout");
    let document: serde_json::Value = serde_json::from_str(&json).expect("a document");
    assert_eq!(document["nodes"][0]["payload"]["id"], "research");
    assert_eq!(document["schema_version"], 1);
}

/// End of input is how a session ends, and ending one that was never given a
/// line is not a failure: the editor opened an empty document, was asked for
/// nothing, and says nothing.
#[test]
fn end_of_input_with_nothing_typed_is_a_clean_exit() {
    let (code, stdout, stderr) = session(&[], "");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.is_empty(), "nothing was asked for: {stdout:?}");
    assert!(
        stderr.is_empty(),
        "and no prompt, since stdin is not a terminal: {stderr:?}"
    );
}

/// Blank lines and `#` comments are not commands and not mistakes, which is
/// what lets a dumped script be annotated and still replay.
#[test]
fn blank_lines_and_comments_pass_through() {
    let (code, stdout, stderr) = session(
        &[],
        "\n# the researcher\n\n   # indented, still a comment\nshow\n",
    );
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout.lines().next(),
        Some("graph: empty, schema_version 1"),
        "only the one real line produced anything: {stdout}"
    );
}
