//! Minimal structural JSON Schema validation for resume inputs.
//!
//! When a run parks on a tool suspension, the `Suspended` event records the
//! JSON Schema the resume input must satisfy. `Runtime::resume` checks the
//! supplied input against that recorded schema *before* recording a
//! `Resumed` event, so a wrong-shaped approval never becomes history.
//!
//! # What "validates" means in v0.1
//!
//! This is a structural subset of JSON Schema, not a full validator. The
//! keywords honored are:
//!
//! - `type` (a string or an array of strings, with the standard seven type
//!   names; `integer` accepts only integral JSON numbers)
//! - `required` (each named property must be present on an object)
//! - `properties` (present properties are validated recursively)
//! - `items` (array elements are validated recursively against a single
//!   schema object)
//! - `enum` (the value must equal one of the listed values)
//!
//! Everything else (`format`, `pattern`, numeric ranges, `oneOf`, `$ref`,
//! and so on) is ignored: an input is never *rejected* because of a keyword
//! this subset does not implement. That bias is deliberate for v0.1. The
//! recorded schema is written by tool authors for humans filling approval
//! forms; the failure mode that matters is an obviously wrong shape (a
//! string where an object was asked for, a missing required field), and
//! that is exactly what this subset rejects. A full validator is a
//! dependency decision deferred until the approval-inbox work (v0.3) makes
//! the richer keywords real.
//!
//! A schema that is not a JSON object (for example `true`) accepts
//! everything, matching JSON Schema's own boolean-schema semantics.

use serde_json::Value;

/// Validates `input` against the structural subset of `schema` documented
/// at module level.
///
/// # Errors
///
/// Returns a human-readable description of the first violation, naming the
/// JSON path where it occurred.
pub fn validate_against_schema(input: &Value, schema: &Value) -> Result<(), String> {
    validate_at(input, schema, "$")
}

/// The recursive worker behind [`validate_against_schema`]; `path` names
/// the location being validated, for error messages.
fn validate_at(input: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };

    if let Some(expected) = schema.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|name| matches_type(input, name)) {
            return Err(format!(
                "{path}: expected type {}, got {}",
                names.join(" or "),
                type_name(input)
            ));
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.iter().any(|candidate| candidate == input)
    {
        return Err(format!("{path}: value is not one of the allowed values"));
    }

    if let Some(object) = input.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}: missing required property `{name}`"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                if let Some(value) = object.get(name) {
                    validate_at(value, property_schema, &format!("{path}.{name}"))?;
                }
            }
        }
    }

    if let (Some(items), Some(item_schema)) = (input.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            validate_at(item, item_schema, &format!("{path}[{index}]"))?;
        }
    }

    Ok(())
}

/// Whether `value` is of the named JSON Schema type.
fn matches_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// The JSON type name of a value, for error messages.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shapes the subset must reject: wrong type, missing required
    /// property, wrong nested property type, value outside an enum.
    #[test]
    fn obviously_wrong_shapes_are_rejected() {
        let schema = json!({
            "type": "object",
            "required": ["approved"],
            "properties": {
                "approved": {"type": "boolean"},
                "note": {"type": "string"}
            }
        });
        assert!(validate_against_schema(&json!({"approved": true}), &schema).is_ok());
        assert!(
            validate_against_schema(&json!({"approved": false, "note": "no"}), &schema).is_ok()
        );

        assert!(validate_against_schema(&json!("yes"), &schema).is_err());
        assert!(validate_against_schema(&json!({}), &schema).is_err());
        assert!(validate_against_schema(&json!({"approved": "yes"}), &schema).is_err());

        let choice = json!({"enum": ["approve", "reject"]});
        assert!(validate_against_schema(&json!("approve"), &choice).is_ok());
        assert!(validate_against_schema(&json!("maybe"), &choice).is_err());
    }

    /// Arrays validate their items; type unions accept any listed type.
    #[test]
    fn arrays_and_type_unions_validate() {
        let schema = json!({"type": "array", "items": {"type": "integer"}});
        assert!(validate_against_schema(&json!([1, 2, 3]), &schema).is_ok());
        assert!(validate_against_schema(&json!([1, "two"]), &schema).is_err());

        let union = json!({"type": ["string", "null"]});
        assert!(validate_against_schema(&json!("text"), &union).is_ok());
        assert!(validate_against_schema(&json!(null), &union).is_ok());
        assert!(validate_against_schema(&json!(3), &union).is_err());
    }

    /// Keywords outside the subset never cause rejection, and non-object
    /// schemas accept everything.
    #[test]
    fn unimplemented_keywords_do_not_reject() {
        let schema = json!({"type": "string", "pattern": "^[0-9]+$", "minLength": 99});
        assert!(validate_against_schema(&json!("not digits"), &schema).is_ok());
        assert!(validate_against_schema(&json!({"anything": 1}), &json!(true)).is_ok());
    }
}
