//! End to end through the real `salvor` binary for `salvor agent validate`.
//!
//! The verb builds the same definition `salvor agent hash` builds; these tests
//! pin what it reports instead of a bare hash, and that the exit-code and
//! multi-file contract matches `graph validate`: `0` only when every file
//! given builds, `1` when any one of them does not, and every file is
//! reported rather than the run stopping at the first failure. The
//! definitions here declare no MCP server and no tools, so every test is
//! offline and needs no API key.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use tempfile::tempdir;

/// The smallest definition that builds: a model and nothing else.
const MINIMAL: &str = r#"model = "claude-test-model"
"#;

/// A fresh handle to the `salvor` binary with tracing quieted.
fn salvor() -> Command {
    let mut command = Command::cargo_bin("salvor").expect("salvor binary builds");
    command.env("RUST_LOG", "warn");
    command
}

/// Writes `content` to `dir/name` and returns the path.
fn write(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write");
    path
}

#[test]
fn a_valid_file_reports_the_model_and_a_hash_and_exits_zero() {
    let dir = tempdir().expect("tempdir");
    let agent = write(dir.path(), "research.toml", MINIMAL);

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "a valid file exits 0: {output:?}");
    assert!(
        stdout.contains("agent ok: model claude-test-model, 0 tool(s) from 0 mcp server(s)"),
        "{stdout}"
    );
    assert!(stdout.contains("hash:    sha256:"), "{stdout}");
    // One file: no path prefix, since there is nothing to disambiguate.
    assert!(!stdout.contains("research.toml:"), "{stdout}");
}

#[test]
fn a_declared_prompt_and_budget_are_reported() {
    let dir = tempdir().expect("tempdir");
    let agent = write(
        dir.path(),
        "reviewer.toml",
        "model = \"claude-test-model\"\nsystem_prompt = \"Review the diff.\"\n\n[budgets]\nsteps = 10\ntokens = 5000\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains("prompt:  set ("), "{stdout}");
    assert!(
        stdout.contains("budgets: steps 10, tokens 5000"),
        "{stdout}"
    );
}

/// An unknown top-level field is refused with the field named and every real
/// field listed, the same message `agent hash` would fail on.
#[test]
fn an_unknown_top_level_field_is_refused_precisely() {
    let dir = tempdir().expect("tempdir");
    let agent = write(
        dir.path(),
        "typo.toml",
        "model = \"claude-test-model\"\nsystem_promt = \"typo\"\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains(
            "unknown field `system_promt`, expected one of `model`, `name`, `system_prompt`, \
             `system_prompt_path`, `llm`, `budgets`, `pricing`, `max_response_tokens`, \
             `mcp_servers`, `wasm_tools`, `record_prompts`"
        ),
        "{stderr}"
    );
}

/// An unknown field inside `[budgets]` is refused naming the real budget
/// fields, not the top-level ones.
#[test]
fn an_unknown_nested_field_is_refused_precisely() {
    let dir = tempdir().expect("tempdir");
    let agent = write(
        dir.path(),
        "typo-budget.toml",
        "model = \"claude-test-model\"\n\n[budgets]\nstep = 5\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains("unknown field `step`, expected one of `steps`, `tokens`, `cost_usd`, `wall_time_seconds`"),
        "{stderr}"
    );
}

/// A missing `model` is refused as a missing field, not a default guess.
#[test]
fn a_missing_model_is_refused_precisely() {
    let dir = tempdir().expect("tempdir");
    let agent = write(
        dir.path(),
        "no-model.toml",
        "system_prompt = \"no model here\"\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("missing field `model`"), "{stderr}");
}

/// A wrong type in a declared field is refused naming the type it got and the
/// type it wanted.
#[test]
fn a_wrong_type_is_refused_precisely() {
    let dir = tempdir().expect("tempdir");
    let agent = write(
        dir.path(),
        "wrong-type.toml",
        "model = \"claude-test-model\"\n\n[budgets]\nsteps = \"forty\"\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&agent)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains("invalid type: string \"forty\", expected u64"),
        "{stderr}"
    );
}

/// Several files, some valid and some not: every file is reported (the run
/// does not stop at the first failure), each report is labelled by its path,
/// and the command exits non-zero overall, exactly as `graph validate` exits
/// non-zero for the one invalid document it is given.
#[test]
fn several_files_are_all_reported_and_exit_reflects_any_failure() {
    let dir = tempdir().expect("tempdir");
    let good_one = write(dir.path(), "writer.toml", MINIMAL);
    let bad = write(
        dir.path(),
        "typo.toml",
        "model = \"claude-test-model\"\nsystem_promt = \"typo\"\n",
    );
    let good_two = write(
        dir.path(),
        "reviewer.toml",
        "model = \"claude-test-model\"\nsystem_prompt = \"Review it.\"\n",
    );

    let output = salvor()
        .args(["agent", "validate"])
        .arg(&good_one)
        .arg(&bad)
        .arg(&good_two)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "one bad file among several fails the whole command: {output:?}"
    );
    // Both good files still ran and are labelled by path, in the order given.
    assert!(
        stdout.contains(&format!("{}: agent ok:", good_one.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("{}: agent ok:", good_two.display())),
        "{stdout}"
    );
    // The bad file is reported too, labelled, with its precise error.
    assert!(
        stderr.contains(&format!("{}: parsing agent file", bad.display())),
        "{stderr}"
    );
    assert!(stderr.contains("unknown field `system_promt`"), "{stderr}");
}

/// Nothing to validate is a clap refusal, and a missing file names itself.
#[test]
fn nothing_to_validate_and_an_unreadable_file_are_both_refused() {
    let output = salvor().args(["agent", "validate"]).output().expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "a clap refusal: {output:?}");
    assert!(
        stderr.contains("<FILE>..."),
        "names what is missing: {stderr}"
    );

    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("nowhere.toml");
    let output = salvor()
        .args(["agent", "validate"])
        .arg(&missing)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "names the file it could not read: {stderr}"
    );
}
