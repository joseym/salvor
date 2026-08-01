//! Guards `docs/graph-schema.json` against drift from [`salvor_graph::graph_schema`].
//! An editor points at the checked-in file for autocomplete and inline
//! validation while authoring a graph document by hand, so a node field added
//! or redocumented here without regenerating it must break this build.
//!
//! Regenerate the checked-in copy with:
//!
//!     UPDATE_GRAPH_SCHEMA=1 cargo test -p salvor-graph --test graph_schema
//!
//! That writes the file from the exact same in-process generator
//! (`salvor_graph::graph_schema`) this test asserts against, so the
//! checked-in copy and the assertion can never disagree about the format.

use std::path::PathBuf;

/// `docs/graph-schema.json` at the repo root, from this crate's manifest dir.
fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/graph-schema.json")
}

/// Pretty-printed JSON with a trailing newline, matching what `salvor graph
/// schema` prints and what a checked-in text file holds.
fn generate() -> String {
    let schema = salvor_graph::graph_schema();
    let mut json = serde_json::to_string_pretty(&schema).expect("schema serializes");
    json.push('\n');
    json
}

/// The schema generated in-process from the `Graph` types right now must
/// match the checked-in file byte for byte. A failure here means either a
/// node field changed (a new field, a renamed one, a redocumented one) and
/// the schema was not regenerated, or the checked-in file was hand-edited;
/// both are fixed the same way.
#[test]
fn graph_schema_matches_the_checked_in_copy() {
    let generated = generate();

    if std::env::var_os("UPDATE_GRAPH_SCHEMA").is_some() {
        std::fs::write(schema_path(), &generated).expect("writing docs/graph-schema.json");
        return;
    }

    let checked_in = std::fs::read_to_string(schema_path()).expect(
        "docs/graph-schema.json should exist; create it with `UPDATE_GRAPH_SCHEMA=1 cargo test \
         -p salvor-graph --test graph_schema`",
    );
    assert_eq!(
        generated, checked_in,
        "docs/graph-schema.json is stale: it no longer matches the Graph types in src/document.rs. \
         Regenerate it with `UPDATE_GRAPH_SCHEMA=1 cargo test -p salvor-graph --test graph_schema` \
         and commit the result."
    );
}

/// The generator must not depend on anything ordered by memory address or
/// hashing: two runs in the same process must agree exactly, or a diff in
/// the checked-in file could mean nothing more than which run produced it.
#[test]
fn graph_schema_generation_is_deterministic() {
    assert_eq!(
        generate(),
        generate(),
        "regenerating twice must produce byte-identical output"
    );
}
