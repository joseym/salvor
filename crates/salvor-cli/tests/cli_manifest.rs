//! Guards `docs/cli-manifest.json` against drift from the real clap parse
//! tree in `src/cli.rs`. A landing-page repo drives its demo terminal from
//! that checked-in file instead of a hand-maintained table, so a verb or flag
//! added here without regenerating it must break this build.
//!
//! Regenerate the checked-in copy with:
//!
//!     UPDATE_CLI_MANIFEST=1 cargo test -p salvor-cli --test cli_manifest
//!
//! That writes the file from the exact same in-process generator
//! (`salvor_cli::manifest::build`) this test asserts against, so the
//! checked-in copy and the assertion can never disagree about the format.

use std::path::PathBuf;

/// `docs/cli-manifest.json` at the repo root, from this crate's manifest dir.
fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/cli-manifest.json")
}

/// Pretty-printed JSON with a trailing newline, matching what a checked-in
/// text file holds (and what most editors leave behind on save).
fn generate() -> String {
    let manifest = salvor_cli::manifest::build();
    let mut json = serde_json::to_string_pretty(&manifest).expect("manifest serializes");
    json.push('\n');
    json
}

/// The manifest generated in-process from clap right now must match the
/// checked-in file byte for byte. A failure here means either the CLI
/// changed and the manifest was not regenerated, or the checked-in file was
/// hand-edited; both are fixed the same way.
#[test]
fn cli_manifest_matches_the_checked_in_copy() {
    let generated = generate();

    if std::env::var_os("UPDATE_CLI_MANIFEST").is_some() {
        std::fs::write(manifest_path(), &generated).expect("writing docs/cli-manifest.json");
        return;
    }

    let checked_in = std::fs::read_to_string(manifest_path()).expect(
        "docs/cli-manifest.json should exist; create it with `UPDATE_CLI_MANIFEST=1 cargo test \
         -p salvor-cli --test cli_manifest`",
    );
    assert_eq!(
        generated, checked_in,
        "docs/cli-manifest.json is stale: it no longer matches the clap parse tree in \
         src/cli.rs. Regenerate it with `UPDATE_CLI_MANIFEST=1 cargo test -p salvor-cli --test \
         cli_manifest` and commit the result."
    );
}

/// The generator must not depend on anything ordered by memory address or
/// hashing: two runs in the same process must agree exactly, or a diff in
/// the checked-in file could mean nothing more than which run produced it.
#[test]
fn cli_manifest_generation_is_deterministic() {
    assert_eq!(
        generate(),
        generate(),
        "regenerating twice must produce byte-identical output"
    );
}
