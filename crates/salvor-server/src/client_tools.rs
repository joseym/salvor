//! Client-performed tool declarations: what the operator says about a tool the
//! CLIENT runs in its own process, and the registry a host loads them into.
//!
//! # Declared by the operator, implemented by the client
//!
//! A [`ClientToolDecl`] is a tool with no code behind it on this server. The
//! operator declares its name, its [`Effect`], the shape of its input, the
//! shape of its completion, and whether the client's word is good enough to
//! close the call. The client is the one that actually performs the work, in
//! its own process, with its own secrets. That is the whole point: a tool whose
//! credential must never reach salvor can still be recorded in a salvor run.
//!
//! # Why declarations are never registered over HTTP
//!
//! They are loaded by `salvor serve --client-tool <FILE>` and by an embedding
//! host through [`AppState::with_client_tools`](crate::AppState::with_client_tools),
//! and there is deliberately no endpoint that accepts one.
//!
//! The reason is the effect class. The server-performed
//! [`tool_step`](crate::client_runs::tool_step) already refuses to take the
//! effect from the request body, so a caller cannot up- or down-grade a `Write`
//! into a freely retried `Read`. A declaration carries an effect too. If a
//! client could POST its own declaration it would be choosing its own effect
//! class by the back door: declare the charge as a `Read`, and the write-ahead
//! rule that makes an unsettled write surface for a human stops applying to it.
//! Keeping declarations operator-side keeps the effect an operator's word in
//! both surfaces, which is the invariant, not an implementation detail.
//!
//! # The format
//!
//! One TOML file per declaration, mirroring how `--agent` takes one agent file:
//!
//! ```toml
//! name = "charge_card"
//! effect = "write"
//! trust_completion = false
//!
//! [input_schema]
//! type = "object"
//! required = ["amount_cents"]
//!
//! [input_schema.properties.amount_cents]
//! type = "integer"
//!
//! [output_schema]
//! type = "object"
//! required = ["charge_id"]
//!
//! [output_schema.properties.charge_id]
//! type = "string"
//! ```
//!
//! The struct carries the `Deserialize` derive, so it defines the format; the
//! CLI reads the bytes off disk and hands them to `toml`, exactly as it owns
//! file reading for agent definitions. Nothing here touches the filesystem.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use salvor_core::Effect;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

/// One operator-written declaration of a tool the CLIENT performs.
///
/// There is no handler behind it. It exists so this server can do the four
/// things it CAN honestly do about a call it never witnessed: fix the effect
/// class, check the input before an intent is recorded, check the reported
/// output against a shape the operator declared, and decide whether the
/// client's report is allowed to close the call at all.
///
/// Unknown keys are rejected rather than ignored. A typo in `trust_completion`
/// would otherwise fall back to the permissive default silently, which is
/// exactly the mistake an operator would not catch until a client had already
/// self-completed a write.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientToolDecl {
    /// The tool's name, the one a client names when it opens an intent.
    pub name: String,
    /// The operator-declared effect class, recorded on every intent for this
    /// tool. Never taken from the client, for the reason in the module docs.
    pub effect: Effect,
    /// The schema an intent's input must satisfy, checked with
    /// [`salvor_runtime::validate_against_schema`] before anything is written.
    pub input_schema: Value,
    /// The schema a client-reported completion must satisfy. Optional in the
    /// format, because a declaration is still useful without one (the effect
    /// and the input check both still apply), but a tool declared without it
    /// cannot be self-completed by a client: an unfalsifiable completion is
    /// precisely what the schema exists to prevent.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// Whether the client may record its own completion for this tool. `true`
    /// by default, because the ordinary case is a client salvor is willing to
    /// take at its word. `false` means every call for this tool must be settled
    /// by hand through the resolve endpoint after someone has verified it
    /// externally.
    #[serde(default = "trusted_by_default")]
    pub trust_completion: bool,
}

/// The default for [`ClientToolDecl::trust_completion`]: a declaration that
/// says nothing about trust is trusting.
fn trusted_by_default() -> bool {
    true
}

/// The client-performed tool declarations a server was started with.
///
/// The counterpart of [`ToolRegistry`](crate::ToolRegistry), and deliberately a
/// separate type: that one holds executable tools this server dispatches, this
/// one holds declarations of tools it never runs. Merging them would put a
/// `DynTool` with no implementation into the registry a graph `tool` node
/// resolves through, and a graph node would then resolve a tool that cannot be
/// called.
///
/// Empty is the default and is a complete, honest state: every client-tool
/// intent is a clean `unknown_tool` until an operator declares one. There is no
/// "no registry wired" case to distinguish, unlike the executable registry,
/// because nothing is ever dispatched here.
#[derive(Debug, Default, Clone)]
pub struct ClientToolRegistry {
    decls: HashMap<String, ClientToolDecl>,
}

impl ClientToolRegistry {
    /// An empty set of declarations: the `salvor serve` default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            decls: HashMap::new(),
        }
    }

    /// Records `decl` under its own [`ClientToolDecl::name`], replacing any
    /// declaration already held under that name, so a host composing a set
    /// keeps the last word (the same rule [`ToolRegistry`](crate::ToolRegistry)
    /// uses).
    pub fn declare(&mut self, decl: ClientToolDecl) {
        self.decls.insert(decl.name.clone(), decl);
    }

    /// Records `decl` and returns the registry, for the builder style a host
    /// composes with.
    #[must_use]
    pub fn with_decl(mut self, decl: ClientToolDecl) -> Self {
        self.declare(decl);
        self
    }

    /// The declaration held under `name`, if any. `None` is the `unknown_tool`
    /// case the client-tool intent endpoint reports without writing anything.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ClientToolDecl> {
        self.decls.get(name)
    }

    /// Whether no declarations are held (the `salvor serve` default).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decls.is_empty()
    }

    /// How many declarations are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.decls.len()
    }

    /// Every declared name, sorted, for a stable listing in a log line or an
    /// operator-facing report.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.decls.keys().cloned().collect();
        names.sort();
        names
    }
}

/// `GET /v1/client-tools`: every client-performed tool declaration this server
/// was started with.
///
/// This is how a client-driven loop gets the function definitions to hand the
/// model: a declaration's `input_schema` IS the model tool's parameter schema,
/// the same schema the server checks a client-tool intent's input against, so
/// publishing it here is what keeps the client from keeping a second copy that
/// can drift from the one the server validates against.
///
/// No drive token: this is server configuration, not run state, so it sits
/// behind only the bearer-auth layer every other `/v1` route sits behind.
/// Empty (never an error) on a server started with no `--client-tool` files,
/// the same honest-empty posture [`ClientToolRegistry`] itself takes.
pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let registry = state.client_tools();
    let client_tools: Vec<Value> = registry
        .names()
        .into_iter()
        .filter_map(|name| registry.get(&name).cloned())
        .map(|decl| {
            let mut entry = json!({
                "name": decl.name,
                "effect": decl.effect,
                "input_schema": decl.input_schema,
                "trust_completion": decl.trust_completion,
            });
            if let Some(output_schema) = decl.output_schema {
                entry
                    .as_object_mut()
                    .expect("entry is a JSON object")
                    .insert("output_schema".to_owned(), output_schema);
            }
            entry
        })
        .collect();
    Json(json!({ "client_tools": client_tools }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TOML format the operator writes: the required fields, the optional
    /// output schema, and the trusting default.
    #[test]
    fn a_declaration_parses_from_toml_with_its_defaults() {
        let decl: ClientToolDecl = toml::from_str(
            r#"
            name = "charge_card"
            effect = "write"

            [input_schema]
            type = "object"
            "#,
        )
        .expect("the declaration parses");
        assert_eq!(decl.name, "charge_card");
        assert_eq!(decl.effect, Effect::Write);
        assert!(decl.output_schema.is_none());
        assert!(
            decl.trust_completion,
            "a declaration silent about trust is trusting"
        );
    }

    /// A misspelled key is an error, not a silent fall back to the permissive
    /// default: that mistake would only be discovered after a client had
    /// already self-completed a write.
    #[test]
    fn an_unknown_key_is_refused() {
        let error = toml::from_str::<ClientToolDecl>(
            r#"
            name = "charge_card"
            effect = "write"
            trust_completions = false

            [input_schema]
            type = "object"
            "#,
        )
        .expect_err("an unknown key is refused");
        assert!(
            error.to_string().contains("trust_completions"),
            "the error names the offending key: {error}"
        );
    }
}
