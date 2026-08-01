//! Enforcing a gate's `approval_schema` against the input a human resumes with.
//!
//! A `gate` node carries an `approval_schema`: the JSON Schema the human
//! approval must satisfy. Until now that schema was recorded (it becomes the
//! `Suspended` event's `input_schema`) and advertised (the approval inbox and
//! the CLI both print it), but nothing ever checked an approval against it, so
//! a gate declaring `{"required": ["approved"]}` accepted `null`, `42`,
//! `"nope"`, and `{}` alike. This module is the check.
//!
//! # The accept edge
//!
//! The one place validation may happen is the **accept edge**: the moment a
//! resume input is about to become a `Resumed` event, and before that event is
//! appended. In the engine that edge is in `run_graph`'s `Node::Gate` arm,
//! between `RunCtx::suspend` and `RunCtx::await_resume` (see the comment
//! there). The server and the CLI each run the same check one layer earlier, so
//! the operator gets a synchronous refusal instead of a driver task that dies
//! quietly; the engine's check is the backstop that a direct library caller
//! also gets.
//!
//! Validation must NEVER move to the other side of that edge. A recorded
//! `Resumed` event is history: replay trusts it and re-feeds it to the gate
//! verbatim. If replay re-validated it, tightening this validator (or bumping
//! the `jsonschema` crate) could turn a log that replayed yesterday into a
//! refusal today, which is exactly the property durable execution sells. So the
//! rule is: check what has not been written yet, trust what has.
//!
//! # What "conforms" means
//!
//! The schema is handed to the [`jsonschema`] crate, which implements drafts 4,
//! 6, 7, 2019-09 and 2020-12 and picks 2020-12 when the schema declares no
//! `$schema`. That is a real validator, not the structural subset
//! `salvor_runtime::validate_against_schema` applies to tool suspensions.
//!
//! On top of it sits ONE rule the JSON Schema specification does not give for
//! free, and it is the rule that closes most of the hole. Under the spec,
//! `required` and `properties` are *object* keywords: applied to `null`, `42`,
//! or `"nope"`, they are vacuously satisfied, because those instances have no
//! properties to require. A spec-perfect validator therefore approves all
//! three against `{"required": ["approved"], "properties": {...}}`. That is
//! correct JSON Schema and useless as an approval gate. So: a top-level
//! approval schema that names object shape (`required`, `properties`, and their
//! kin) and does not otherwise say what type it wants is read as asking for an
//! object, and a non-object approval is a violation. See `implies_object`
//! below. The rule is deliberately top-level only: it is about
//! what the approval AS A WHOLE is, and a nested `anyOf` branch may legitimately
//! not be an object. An author who really wants to accept a bare `42` says so
//! with `"type"`, `enum`, `const`, or a combinator, all of which switch the rule
//! off.
//!
//! One deliberate soft edge: a schema the validator cannot COMPILE (a gate
//! whose `approval_schema` is a legal JSON object but not a legal JSON Schema)
//! constrains nothing, and every input passes. The alternative is worse. The
//! graph document validator already accepted that gate and runs are already
//! parked at it; refusing to compile mid-flight would strand those runs with no
//! way forward but abandonment. Fail-open here matches how the older structural
//! validator treated keywords it did not implement: an input is never rejected
//! for a reason the schema author cannot see.

use salvor_core::{Event, EventEnvelope};
use salvor_graph::{GateNode, Graph, Node};
use serde_json::Value;
use std::fmt;

/// The most violations one refusal reports. An approval schema is a small,
/// hand-written document, so this is never reached in practice; it exists so a
/// pathological input cannot turn one 400 response into an unbounded body.
const MAX_VIOLATIONS: usize = 20;

/// One way an approval input failed its gate's `approval_schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalViolation {
    /// Where in the input the violation is, in the `$.field[0]` form the rest
    /// of the codebase's schema messages use. The whole input is `$`.
    pub path: String,
    /// What was wrong there, in the validator's own words.
    pub message: String,
}

impl fmt::Display for ApprovalViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Every way `input` fails `schema`, in a stable order. An empty list means the
/// input conforms.
///
/// The order is sorted by path then message rather than left as the validator's
/// iteration order, so the list a refusal prints (and the list a test asserts
/// on) does not move when the validator's internals do.
///
/// A schema that will not compile as JSON Schema yields no violations; see the
/// module docs for why that soft edge is deliberate.
#[must_use]
pub fn approval_violations(input: &Value, schema: &Value) -> Vec<ApprovalViolation> {
    let mut violations: Vec<ApprovalViolation> = Vec::new();
    if implies_object(schema) && !input.is_object() {
        violations.push(ApprovalViolation {
            path: "$".to_owned(),
            message: format!(
                "expected an object, got {}: this gate's approval_schema names the properties an \
                 approval must carry, so only an object can answer it",
                type_name(input)
            ),
        });
    }
    let Ok(validator) = jsonschema::validator_for(schema) else {
        return violations;
    };
    violations.extend(
        validator
            .iter_errors(input)
            .take(MAX_VIOLATIONS)
            .map(|error| ApprovalViolation {
                path: pointer_to_path(&error.instance_path().to_string()),
                message: error.to_string(),
            }),
    );
    violations.sort();
    violations.dedup();
    violations.truncate(MAX_VIOLATIONS);
    violations
}

/// Whether a top-level approval schema is asking for an object without saying
/// so: it names object shape, and it names no other way of deciding what it
/// accepts.
///
/// Any of `type`, `enum`, `const`, `$ref`, or a combinator (`anyOf`, `oneOf`,
/// `allOf`, `not`, `if`) means the author has expressed an intent about the
/// instance's form, and the specification's own semantics are then exactly
/// right; this rule stays out of the way.
fn implies_object(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    const SPEAKS_FOR_ITSELF: [&str; 9] = [
        "type", "enum", "const", "$ref", "anyOf", "oneOf", "allOf", "not", "if",
    ];
    if SPEAKS_FOR_ITSELF
        .iter()
        .any(|key| schema.contains_key(*key))
    {
        return false;
    }
    const OBJECT_SHAPE: [&str; 8] = [
        "required",
        "properties",
        "patternProperties",
        "additionalProperties",
        "propertyNames",
        "minProperties",
        "maxProperties",
        "dependentRequired",
    ];
    OBJECT_SHAPE.iter().any(|key| schema.contains_key(*key))
}

/// The JSON type name of a value, for the one message this module writes
/// itself. Matches `salvor_runtime::validate_against_schema`'s vocabulary.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl PartialOrd for ApprovalViolation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ApprovalViolation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.path, &self.message).cmp(&(&other.path, &other.message))
    }
}

/// The gate a parked run is waiting at, or `None` when it is parked somewhere
/// else (a tool suspension, a budget crossing) or at a node this document has
/// no gate for.
///
/// A parked log ends at its `Suspended` event, and the node that suspended is
/// the last `NodeEntered` before it. That is enough: the server and the CLI
/// both hold the log and the document at resume time, so neither needs a new
/// event field to find out which gate is being answered.
#[must_use]
pub fn parked_gate<'g>(log: &[EventEnvelope], graph: &'g Graph) -> Option<&'g GateNode> {
    let node = log
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            Event::NodeEntered { node } => Some(node.as_str()),
            _ => None,
        })?;
    graph.nodes.iter().find_map(|candidate| match candidate {
        Node::Gate(gate) if gate.id == node => Some(gate),
        _ => None,
    })
}

/// Renders a JSON Pointer (`/targets/0`) as the `$.targets[0]` form the rest of
/// the codebase's schema messages use. The empty pointer, meaning the whole
/// instance, is `$`.
fn pointer_to_path(pointer: &str) -> String {
    let mut path = String::from("$");
    for segment in pointer.split('/').skip(1) {
        if segment.parse::<usize>().is_ok() {
            path.push('[');
            path.push_str(segment);
            path.push(']');
        } else {
            path.push('.');
            // Undo the JSON Pointer escapes, so a property literally named
            // `a/b` reads back as `a/b` rather than `a~1b`.
            path.push_str(&segment.replace("~1", "/").replace("~0", "~"));
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The schema the task's reproduction used: `required` and `properties`
    /// with no top-level `type`. The four inputs that used to sail through it
    /// all have to be violations now.
    #[test]
    fn the_four_reproduced_inputs_are_violations() {
        let schema = json!({
            "required": ["approved"],
            "properties": {"approved": {"type": "boolean"}}
        });
        for bad in [json!(null), json!(42), json!("nope"), json!({})] {
            assert!(
                !approval_violations(&bad, &schema).is_empty(),
                "{bad} must not approve"
            );
        }
        assert_eq!(approval_violations(&json!({"approved": true}), &schema), []);
        assert_eq!(
            approval_violations(&json!({"approved": false}), &schema),
            []
        );
    }

    /// Every violation is reported, not just the first, and each names where it
    /// is.
    #[test]
    fn violations_name_their_paths_and_are_all_reported() {
        let schema = json!({
            "type": "object",
            "required": ["approved", "targets"],
            "properties": {
                "approved": {"type": "boolean"},
                "targets": {"type": "array", "items": {"type": "string"}}
            }
        });
        let violations = approval_violations(&json!({"approved": "yes", "targets": [1]}), &schema);
        let paths: Vec<&str> = violations.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, ["$.approved", "$.targets[0]"], "{violations:?}");
        assert!(violations[0].message.contains("boolean"), "{violations:?}");
    }

    /// A schema that says what form it wants, by any of the means an author
    /// has, switches the implied-object rule off and gets plain JSON Schema
    /// semantics.
    #[test]
    fn a_schema_that_states_its_own_form_is_left_alone() {
        let choice = json!({"enum": ["approve", "reject"]});
        assert_eq!(approval_violations(&json!("approve"), &choice), []);
        assert!(!approval_violations(&json!("maybe"), &choice).is_empty());

        let either = json!({
            "anyOf": [{"type": "boolean"}, {"type": "object", "required": ["approved"]}]
        });
        assert_eq!(approval_violations(&json!(true), &either), []);
        assert_eq!(
            approval_violations(&json!({"approved": false}), &either),
            []
        );
        assert!(!approval_violations(&json!("nope"), &either).is_empty());

        // An explicit `type` that admits a non-object is honored, even
        // alongside `properties`.
        let lenient = json!({"type": ["object", "null"], "properties": {"a": {}}});
        assert_eq!(approval_violations(&json!(null), &lenient), []);
    }

    /// The soft edge: an approval schema that is not a compilable JSON Schema
    /// constrains nothing rather than stranding the run.
    #[test]
    fn an_uncompilable_schema_constrains_nothing() {
        let schema = json!({"type": "not-a-json-type"});
        assert_eq!(approval_violations(&json!(null), &schema), []);
    }

    /// The pointer rendering, including an escaped property name.
    #[test]
    fn pointers_render_in_the_codebase_path_style() {
        assert_eq!(pointer_to_path(""), "$");
        assert_eq!(pointer_to_path("/approved"), "$.approved");
        assert_eq!(pointer_to_path("/targets/0/url"), "$.targets[0].url");
        assert_eq!(pointer_to_path("/a~1b"), "$.a/b");
    }
}
