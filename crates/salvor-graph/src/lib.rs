//! The Salvor graph document format, strict versioned validation, and JSON
//! Schema emission.
//!
//! A graph is a declarative CONTROL document: authored once, submitted, hashed
//! into a run, and then frozen. It coordinates nodes the runtime already knows
//! how to execute (a full `agent` loop, a single `tool` call, a human `gate`, a
//! `branch`, a `map` fan-out, and a `fold` bounded-iteration loop) and the typed
//! edges between them. This crate owns four things and no more:
//!
//! - the [`document`] model: [`Graph`], [`Node`], the payloads, and [`Edge`],
//!   parsed strictly (unknown fields rejected) and versioned additively;
//! - the [`validate`] pass: a set of independent checks that collect every
//!   error and name the offending node or edge;
//! - the [`expr`] language: the total, non-Turing-complete condition language a
//!   `branch` case's expression string is written in, parsed at the submit
//!   boundary so a malformed condition is a node-precise error, never a runtime
//!   failure;
//! - [`graph_schema`]: the graph document's JSON Schema, the single source of
//!   truth for editors and the future per-language builders.
//!
//! # What this crate is NOT
//!
//! There is no run-time execution here. No engine drives a graph, no scheduler
//! fans out a `map`, and no server endpoint submits one. Validation PARSES a
//! branch condition (so a bad one fails at submit) but never EVALUATES one
//! against a routed value; the evaluator [`expr::Expr::eval`] exists and is
//! total, but it is the future engine that calls it, not this crate. Keeping
//! this crate to format-plus-validation is what keeps it a pure, IO-free leaf:
//! it depends only on `serde`, `serde_json`, `schemars`, and `thiserror`, drags
//! in no runtime, and so stays usable from a future wasm dashboard projection.
//!
//! # Strict in, additive out
//!
//! Parsing rejects a stray field loudly, because a silently dropped field could
//! drop a gate or an unenforced budget. Validation is likewise strict and fails
//! at the submit boundary, not at run time. The one forward-compatibility
//! concession is the additive `schema_version` discipline (see
//! [`document::SCHEMA_VERSION`]): a graph recorded under an older build still
//! parses and validates under a newer one.

#![warn(missing_docs)]

pub mod builder;
pub mod document;
pub mod expr;
pub mod validate;

pub use builder::{AgentSpec, BranchSpec, FoldSpec, GateSpec, GraphBuilder, MapSpec, ToolSpec};
pub use document::{
    AgentNode, BranchCase, BranchCondition, BranchNode, Edge, FoldBody, FoldJoin, FoldNode,
    GateNode, Graph, MapBody, MapNode, Node, SCHEMA_VERSION, ToolNode,
};
pub use expr::{Expr, ExprError, MAX_EXPRESSION_LEN, parse as parse_expression};
pub use validate::{GraphError, GraphSummary, MAX_NODE_NAME_LEN, validate};

/// Returns the graph document's JSON Schema as a [`serde_json::Value`].
///
/// This is the single source of truth for the document format: editors read it
/// for autocomplete and inline validation, and the future per-language builders
/// generate from it so a Rust, Python, or TypeScript author reduces to the same
/// canonical JSON. It is derived from the [`Graph`] types by `schemars`, so it
/// can never drift from what this crate actually parses.
#[must_use]
pub fn graph_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Graph))
        .expect("a schemars-generated schema is always valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitted schema is a JSON object that describes the `Graph` type: it
    /// declares the document's own fields (`schema_version`, `nodes`, `edges`)
    /// and, through `$defs`, the node payloads.
    #[test]
    fn graph_schema_describes_the_document() {
        let schema = graph_schema();
        assert!(schema.is_object(), "schema is a JSON object");
        let text = serde_json::to_string(&schema).expect("serialize");
        for expected in [
            "schema_version",
            "nodes",
            "edges",
            "agent_hash",
            "approval_schema",
        ] {
            assert!(
                text.contains(expected),
                "schema mentions {expected}: {text}"
            );
        }
    }
}
