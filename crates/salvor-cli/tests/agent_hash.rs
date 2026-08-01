//! End to end through the real `salvor` binary for `salvor agent hash`.
//!
//! The headline is the one property that makes the verb worth having: the hash
//! it prints is the SAME string `graph run` resolves an `agent` node against.
//! That is proven here without a model and without a network, by reading the
//! resolver's own refusal, which lists the hashes the `--agent` files supplied.
//!
//! The rest pins the output contract. One file prints the bare hash and nothing
//! else, because an author drops it straight into a shell substitution; several
//! print `<path>: <hash>` so the mapping is legible. The definitions here
//! declare no MCP server and no tools, so every test is offline and needs no
//! API key: a definition builds without one, since nothing is sent.

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

/// Whether a line is a well-formed definition hash and nothing more: the same
/// shape a graph document's `agent_hash` has to carry.
fn is_def_hash(text: &str) -> bool {
    match text.strip_prefix("sha256:") {
        Some(hex) => hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

#[test]
fn one_file_prints_the_bare_hash_and_nothing_else() {
    let dir = tempdir().expect("tempdir");
    let agent = write(dir.path(), "research.toml", MINIMAL);

    let output = salvor()
        .arg("agent")
        .arg("hash")
        .arg(&agent)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "hashing a valid file exits 0: {output:?}"
    );
    // Exactly one line, and that line is the hash: no path, no label, no
    // trailing report. A shell substitution takes whatever is here verbatim.
    let line = stdout.strip_suffix('\n').unwrap_or(&stdout);
    assert!(is_def_hash(line), "stdout is one bare hash: {stdout:?}");
}

/// The property the verb exists for. `graph run` refuses an `agent` node whose
/// hash matches none of the provided `--agent` files, and names the hashes that
/// WERE provided; that list is the resolver's own key for this very file, so
/// what `agent hash` printed has to appear in it verbatim.
#[test]
fn the_printed_hash_is_what_the_graph_resolver_keys_the_file_on() {
    let dir = tempdir().expect("tempdir");
    let store = dir.path().join("salvor.db");
    let agent = write(dir.path(), "research.toml", MINIMAL);

    let printed = salvor()
        .arg("agent")
        .arg("hash")
        .arg(&agent)
        .output()
        .expect("runs");
    let printed = String::from_utf8_lossy(&printed.stdout).trim().to_owned();

    // A node referencing a hash nothing supplies, so the run refuses before it
    // writes a head and reports what the --agent file did supply.
    let graph = write(
        dir.path(),
        "flow.json",
        &format!(
            r#"{{
  "schema_version": 1,
  "nodes": [ {{ "kind": "agent", "payload": {{
    "id": "research",
    "agent_hash": "sha256:{}"
  }} }} ],
  "edges": []
}}"#,
            "1".repeat(64)
        ),
    );

    let output = salvor()
        .arg("--store")
        .arg(&store)
        .args(["graph", "run"])
        .arg(&graph)
        .args(["--input", "{}"])
        .arg("--agent")
        .arg(&agent)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an unresolvable node refuses: {output:?}"
    );
    assert!(
        stderr.contains(&format!("(provided: {printed})")),
        "the resolver keys this file on the hash `agent hash` printed: {stderr}"
    );
}

/// Several files need to say which hash is which, so each line carries its
/// path, in the order the paths were given rather than any order the builder
/// happens to produce.
#[test]
fn several_files_are_labelled_by_path_in_the_order_given() {
    let dir = tempdir().expect("tempdir");
    let first = write(dir.path(), "writer.toml", MINIMAL);
    let second = write(
        dir.path(),
        "reviewer.toml",
        "model = \"claude-test-model\"\nsystem_prompt = \"Review it.\"\n",
    );

    let output = salvor()
        .arg("agent")
        .arg("hash")
        .arg(&second)
        .arg(&first)
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "one line per file: {stdout}");
    for (line, path) in lines.iter().zip([&second, &first]) {
        let prefix = format!("{}: ", path.display());
        let hash = line
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("line `{line}` is labelled with {}", path.display()));
        assert!(
            is_def_hash(hash),
            "the label is followed by the hash: {line}"
        );
    }
    // Two different system prompts are two different definitions, so the two
    // hashes must differ; otherwise the labels would be decoration over one
    // value.
    assert_ne!(lines[0], lines[1], "{stdout}");
}

/// The hash is a fact about the definition's content, not about where the file
/// sits: a copy under another name in another directory hashes the same, and
/// changing one declared field changes it.
#[test]
fn the_hash_follows_the_content_not_the_path() {
    let dir = tempdir().expect("tempdir");
    let nested = dir.path().join("agents");
    fs::create_dir(&nested).expect("mkdir");
    let original = write(dir.path(), "research.toml", MINIMAL);
    let copy = write(&nested, "renamed.toml", MINIMAL);
    let changed = write(dir.path(), "other-model.toml", "model = \"claude-other\"\n");

    let hash_of = |path: &Path| {
        let output = salvor()
            .arg("agent")
            .arg("hash")
            .arg(path)
            .output()
            .expect("runs");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    assert_eq!(
        hash_of(&original),
        hash_of(&copy),
        "same content, same hash"
    );
    assert_ne!(
        hash_of(&original),
        hash_of(&changed),
        "a different model is a different agent"
    );
}

/// The refusals: nothing to hash is a parse error, and a file that is not an
/// agent definition names itself in the failure.
#[test]
fn nothing_to_hash_and_an_unreadable_file_are_both_refused() {
    let output = salvor().args(["agent", "hash"]).output().expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "a clap refusal: {output:?}");
    assert!(
        stderr.contains("<FILE>..."),
        "names what is missing: {stderr}"
    );

    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("nowhere.toml");
    let output = salvor()
        .arg("agent")
        .arg("hash")
        .arg(&missing)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "names the file it could not read: {stderr}"
    );
}
