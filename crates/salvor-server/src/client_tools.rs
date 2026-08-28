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
//! idempotency_key = ["order_id", "amount_cents"]
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
use salvor_core::{Effect, Event, EventEnvelope, Performer};
use salvor_runtime::validate_against_schema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
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
    /// Top-level input field names that, together, say what one call for this
    /// tool IS. Empty by default, and empty means the key stays positional.
    ///
    /// A client-performed call always gets a server-derived idempotency key
    /// (see `client_tool_idempotency_key`), because the client must not be the
    /// one choosing it. What the operator chooses here is what the derivation
    /// is over. With no fields named, the key is derived from the call's
    /// POSITION in the run, which is an attempt identifier: the same position
    /// retried presents the same key, and that is all it promises. Naming
    /// fields makes it a content identity instead: `["order_id",
    /// "amount_cents"]` says that a refund of that amount against that order is
    /// one refund, wherever in the run it is asked for, so a loop that asks for
    /// it twice gets the first call's answer back rather than a second refund.
    ///
    /// Each name is a top-level field of the intent's input. A field the input
    /// does not carry is refused at the intent boundary, naming it, rather than
    /// silently deriving a key over a missing value: two different calls would
    /// otherwise collapse onto one identity.
    pub idempotency_key: Vec<String>,
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
    /// Silence keeps the positional derivation, which is what every declaration
    /// written before this field meant.
    #[serde(default)]
    idempotency_key: Vec<String>,
}

impl TryFrom<RawClientToolDecl> for ClientToolDecl {
    type Error = String;

    /// Enforces the load-time [`require_equal`](ClientToolDecl::require_equal)
    /// rule: every named field must be present in both `input_schema.required`
    /// and `output_schema.required`, so the value to compare always exists on
    /// each side. A violation is refused here, naming the field and the side it
    /// is missing from, exactly as an unknown key is refused: early and precise.
    fn try_from(raw: RawClientToolDecl) -> Result<Self, Self::Error> {
        // A key field the input is not obliged to carry would be a key that
        // sometimes cannot be derived, and the failure would land on a client
        // mid-run rather than on the operator at load. Same rule, same moment,
        // same precision as the require_equal check below.
        for field in &raw.idempotency_key {
            if !schema_requires(&raw.input_schema, field) {
                return Err(missing_idempotency_key_field(&raw.name, field));
            }
        }
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
            idempotency_key: raw.idempotency_key,
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

/// The load-time refusal for an `idempotency_key` field that is not required by
/// the input schema, naming the tool and the field.
fn missing_idempotency_key_field(tool: &str, field: &str) -> String {
    format!(
        "tool `{tool}` names `{field}` in idempotency_key, but `{field}` is not in \
         input_schema.required; a field the key is derived from must be required, so every call \
         for this tool has one to derive from"
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
            // Published for the same reason `input_schema` is: a client that
            // wants to derive the key itself, to check this server's work, has
            // to know what the derivation is over.
            if !decl.idempotency_key.is_empty() {
                entry
                    .as_object_mut()
                    .expect("entry is a JSON object")
                    .insert("idempotency_key".to_owned(), json!(decl.idempotency_key));
            }
            entry
        })
        .collect();
    Json(json!({ "client_tools": client_tools }))
}

/// Checks a hand-recorded resolution against the operator's declaration, when
/// the call being resolved is one the CLIENT performed.
///
/// Both resolve endpoints (`POST /v1/runs/{id}/resolve` and its drive-token
/// twin `POST /v1/client-runs/{id}/resolve`) go through here before a
/// completion is written, and they share this one function so an operator meets
/// the same rules on either path.
///
/// # Why resolve is checked at all
///
/// Resolve records an output nothing in this process witnessed, which is
/// exactly the situation the declaration exists for. Every guard the completion
/// boundary applies to a client's own report applies here for the same reason:
/// the `output_schema` says what evidence a finished call has to carry, and a
/// `require_equal` field says a report may not change what was authorized. A
/// resolution that skipped both could settle a refund intent for 5000 with a
/// completion saying 50000, and the log would carry it as fact.
///
/// What is NOT checked here is `trust_completion`. That flag answers "may the
/// CLIENT close this call", and the whole point of a `false` is that a person
/// closes it instead, which is what this path is. Refusing here would leave a
/// tool nobody could ever settle.
///
/// A dangling intent that salvor performed itself is left alone: this server
/// witnessed that call, holds no declaration for it, and its output is checked
/// by nothing today. Returns `Ok(())` for that case, and for a log that does
/// not end at a tool intent at all (the resolve itself then refuses on state).
///
/// # Errors
///
/// [`ApiError::BadRequest`] naming the tool when no declaration for it is
/// loaded here, when the output fails the declared `output_schema`, or when a
/// `require_equal` field's value differs from the one the intent recorded.
pub(crate) fn check_client_resolution(
    registry: &ClientToolRegistry,
    log: &[EventEnvelope],
    output: &Value,
) -> Result<(), ApiError> {
    let Some(EventEnvelope {
        event:
            Event::ToolCallRequested {
                tool,
                input,
                performed_by: Some(Performer::Client),
                ..
            },
        ..
    }) = log.last()
    else {
        return Ok(());
    };

    // A declaration this server no longer holds is a stale registry, not a bad
    // request from the caller, so the message says what the operator has to fix
    // rather than what the caller should have sent. Recording the resolution
    // unchecked is the one thing this must not do: the shape and the pinned
    // fields would go unexamined precisely where nobody witnessed the call.
    let decl = registry.get(tool).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "no client-performed tool named `{tool}` is declared on this server, so the output \
             offered for it cannot be checked; start the server with `--client-tool <FILE>` for \
             `{tool}` and resolve again"
        ))
    })?;

    // A declaration with no output_schema has nothing to check the shape
    // against, and that is a legitimate declaration: it is exactly the tool the
    // client may not self-complete, whose calls are meant to arrive here. The
    // load-time rule guarantees no require_equal field can be named without an
    // output schema, so nothing below is skipped along with it.
    if let Some(output_schema) = &decl.output_schema {
        validate_against_schema(output, output_schema).map_err(|error| {
            ApiError::BadRequest(format!(
                "the output offered for `{tool}` does not match its declared output_schema: {error}"
            ))
        })?;
    }

    for field in &decl.require_equal {
        let authorized = input.get(field).unwrap_or(&Value::Null);
        let offered = output.get(field).unwrap_or(&Value::Null);
        if authorized != offered {
            return Err(ApiError::BadRequest(format!(
                "the output offered for `{tool}` reports `{field}` as {offered}, but the intent \
                 recorded {authorized}; a resolution may not alter a require_equal field. Record \
                 what was authorized, or abandon the run if the provider did something else"
            )));
        }
    }
    Ok(())
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

    /// A declaration silent about `idempotency_key` keeps the positional
    /// derivation, which is what every declaration written before the field
    /// meant.
    #[test]
    fn an_unset_idempotency_key_is_empty() {
        let decl: ClientToolDecl = toml::from_str(
            r#"
            name = "charge_card"
            effect = "write"

            [input_schema]
            type = "object"
            "#,
        )
        .expect("the declaration parses");
        assert!(
            decl.idempotency_key.is_empty(),
            "silence keeps the key positional"
        );
    }

    /// Named key fields load in the order the operator wrote them, which is the
    /// order the derivation reads them in.
    #[test]
    fn declared_key_fields_load_in_order() {
        let decl: ClientToolDecl = toml::from_str(
            r#"
            name = "refund_card"
            effect = "write"
            idempotency_key = ["order_id", "amount_cents"]

            [input_schema]
            type = "object"
            required = ["order_id", "amount_cents"]
            "#,
        )
        .expect("the declaration parses");
        assert_eq!(
            decl.idempotency_key,
            vec!["order_id".to_owned(), "amount_cents".to_owned()]
        );
    }

    /// A key field the input schema does not require is refused at load, naming
    /// the field: a key that sometimes cannot be derived is the operator's
    /// mistake, and it should surface here rather than on a client mid-run.
    #[test]
    fn an_idempotency_key_field_not_required_by_the_input_is_refused() {
        let error = toml::from_str::<ClientToolDecl>(
            r#"
            name = "refund_card"
            effect = "write"
            idempotency_key = ["order_id"]

            [input_schema]
            type = "object"
            required = ["amount_cents"]
            "#,
        )
        .expect_err("the declaration is refused");
        let message = error.to_string();
        assert!(
            message.contains("order_id") && message.contains("input_schema.required"),
            "the error names the field and what it is missing from: {message}"
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
