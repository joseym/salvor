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
/// There is no handler behind it. It exists so this server can do the things it
/// CAN honestly do about a call it never witnessed: fix the effect class, check
/// the input before an intent is recorded, check the reported output against a
/// shape the operator declared, pin named fields so a report cannot alter what
/// was authorized, and decide whether the client's report is allowed to close
/// the call at all.
///
/// Unknown keys are rejected rather than ignored. A misspelled key like
/// `require_equal` would otherwise be dropped silently, leaving a guard the
/// operator meant to set quietly absent, and the mistake would not surface until
/// a client had already altered a field the operator meant to pin. Refusing
/// early, precisely, is the rule.
///
/// The declaration deserializes through [`RawClientToolDecl`] so a
/// cross-field rule the field-by-field format cannot express is enforced at
/// load: every [`require_equal`](Self::require_equal) name must be required on
/// both sides. A file that breaks it fails to parse, naming the field and the
/// missing side.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawClientToolDecl")]
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
    pub output_schema: Option<Value>,
    /// Whether the client may record its own completion for this tool. `false`
    /// by default: silence gets the safe direction, and self-completing a write
    /// on the client's word alone is the convenient direction, so it is an
    /// explicit opt-in. `false` means every call for this tool is settled by
    /// hand through the resolve endpoint after someone has verified it
    /// externally.
    pub trust_completion: bool,
    /// Top-level field names whose client-reported value must equal the intent's
    /// recorded value. Empty by default. Every named field must appear in both
    /// `input_schema.required` and `output_schema.required`, checked at load, so
    /// the two values always exist to compare; at the completion boundary a
    /// reported value that differs from the authorized one refuses the
    /// completion. The output schema is a shape check and cannot know what was
    /// authorized; this is the field-level equality the shape check cannot do.
    pub require_equal: Vec<String>,
}

/// The on-disk shape of a [`ClientToolDecl`], before its cross-field rule is
/// checked. Deserializing lands here first; [`TryFrom`] enforces the
/// [`require_equal`](ClientToolDecl::require_equal) invariant and produces the
/// public type, so a violating file fails to parse rather than loading a
/// declaration whose completion boundary could not do the comparison it names.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientToolDecl {
    name: String,
    effect: Effect,
    input_schema: Value,
    #[serde(default)]
    output_schema: Option<Value>,
    /// Silence gets the safe direction: a declaration that says nothing about
    /// trust may not self-complete.
    #[serde(default)]
    trust_completion: bool,
    #[serde(default)]
    require_equal: Vec<String>,
}

impl TryFrom<RawClientToolDecl> for ClientToolDecl {
    type Error = String;

    /// Enforces the load-time [`require_equal`](ClientToolDecl::require_equal)
    /// rule: every named field must be present in both `input_schema.required`
    /// and `output_schema.required`, so the value to compare always exists on
    /// each side. A violation is refused here, naming the field and the side it
    /// is missing from, exactly as an unknown key is refused: early and precise.
    fn try_from(raw: RawClientToolDecl) -> Result<Self, Self::Error> {
        for field in &raw.require_equal {
            if !schema_requires(&raw.input_schema, field) {
                return Err(missing_require_equal(&raw.name, field, "input_schema"));
            }
            let present_in_output = raw
                .output_schema
                .as_ref()
                .is_some_and(|schema| schema_requires(schema, field));
            if !present_in_output {
                return Err(missing_require_equal(&raw.name, field, "output_schema"));
            }
        }
        Ok(ClientToolDecl {
            name: raw.name,
            effect: raw.effect,
            input_schema: raw.input_schema,
            output_schema: raw.output_schema,
            trust_completion: raw.trust_completion,
            require_equal: raw.require_equal,
        })
    }
}

/// Whether `schema`'s `required` array lists `field`. A JSON Schema object with
/// no `required`, or one whose `required` is not an array, requires nothing.
fn schema_requires(schema: &Value, field: &str) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| required.iter().any(|name| name.as_str() == Some(field)))
}

/// The load-time refusal for a `require_equal` field absent from one side's
/// `required` list, naming the tool, the field, and the side it is missing from.
fn missing_require_equal(tool: &str, field: &str, side: &str) -> String {
    format!(
        "tool `{tool}` names `{field}` in require_equal, but `{field}` is not in {side}.required; a \
         require_equal field must be required on both the input and the output side, so the two \
         values always exist to compare"
    )
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
            if !decl.require_equal.is_empty() {
                entry
                    .as_object_mut()
                    .expect("entry is a JSON object")
                    .insert("require_equal".to_owned(), json!(decl.require_equal));
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
    /// output schema, and the safe defaults. Silence about trust does not
    /// self-complete, and no field is pinned unless one is named.
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
            !decl.trust_completion,
            "a declaration silent about trust does not self-complete"
        );
        assert!(
            decl.require_equal.is_empty(),
            "no field is pinned unless one is named"
        );
    }

    /// A misspelled key is an error, not a silent drop: a mistyped `require_equal`
    /// would otherwise leave a guard the operator meant to set quietly absent.
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

    /// An explicit `trust_completion = true` opts into self-completion, the
    /// direction silence no longer takes.
    #[test]
    fn trust_completion_is_an_explicit_opt_in() {
        let decl: ClientToolDecl = toml::from_str(
            r#"
            name = "charge_card"
            effect = "write"
            trust_completion = true

            [input_schema]
            type = "object"
            "#,
        )
        .expect("the declaration parses");
        assert!(decl.trust_completion, "the explicit opt-in is honored");
    }

    /// A `require_equal` field present in both `required` lists loads and is
    /// carried on the declaration.
    #[test]
    fn a_require_equal_field_required_on_both_sides_loads() {
        let decl: ClientToolDecl = toml::from_str(
            r#"
            name = "charge_card"
            effect = "write"
            require_equal = ["amount_cents"]

            [input_schema]
            type = "object"
            required = ["amount_cents"]

            [output_schema]
            type = "object"
            required = ["amount_cents"]
            "#,
        )
        .expect("the declaration parses");
        assert_eq!(decl.require_equal, vec!["amount_cents".to_owned()]);
    }

    /// A `require_equal` field absent from `input_schema.required` is refused at
    /// load, naming the field and the side it is missing from.
    #[test]
    fn a_require_equal_field_missing_from_the_input_required_is_refused() {
        let error = toml::from_str::<ClientToolDecl>(
            r#"
            name = "charge_card"
            effect = "write"
            require_equal = ["amount_cents"]

            [input_schema]
            type = "object"

            [output_schema]
            type = "object"
            required = ["amount_cents"]
            "#,
        )
        .expect_err("the declaration is refused");
        let message = error.to_string();
        assert!(
            message.contains("amount_cents") && message.contains("input_schema.required"),
            "the error names the field and the missing side: {message}"
        );
    }

    /// A `require_equal` field absent from `output_schema.required` (here because
    /// there is no output schema at all) is refused at load, naming the output
    /// side.
    #[test]
    fn a_require_equal_field_missing_from_the_output_required_is_refused() {
        let error = toml::from_str::<ClientToolDecl>(
            r#"
            name = "charge_card"
            effect = "write"
            require_equal = ["amount_cents"]

            [input_schema]
            type = "object"
            required = ["amount_cents"]
            "#,
        )
        .expect_err("the declaration is refused");
        let message = error.to_string();
        assert!(
            message.contains("amount_cents") && message.contains("output_schema.required"),
            "the error names the field and the missing side: {message}"
        );
    }
}
