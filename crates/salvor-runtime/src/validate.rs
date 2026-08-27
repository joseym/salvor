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
//! - `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` (bounds on
//!   a JSON number, in the draft 2019-09 form where each keyword's value is
//!   itself the bound; draft 4's boolean `exclusiveMinimum` is not a number,
//!   so it reads as no bound at all and rejects nothing)
//! - `minLength`, `maxLength` (a string's length in Unicode code points)
//! - `minItems`, `maxItems` (an array's element count)
//!
//! Everything else (`format`, `pattern`, `multipleOf`, `uniqueItems`,
//! `oneOf`, `$ref`, and so on) is ignored: outside the keywords listed
//! above, an input is never *rejected* because of a keyword this subset
//! does not implement. That bias is what makes the list above worth
//! reading rather than assuming. The shape keywords came first, for the
//! failure mode that matters when a human fills an approval form (a string
//! where an object was asked for, a missing required field). The bounds
//! came second, for the failure mode that matters when a *program* fills
//! the form: an operator who writes `maximum = 99999` on a client tool's
//! `input_schema` is stating the desk's limit, and a limit that is only
//! documentation is not a limit. A full validator is still a dependency
//! decision deferred until the approval-inbox work (v0.3); `pattern` and
//! `format` wait for it.
//!
//! A schema that is not a JSON object (for example `true`) accepts
//! everything, matching JSON Schema's own boolean-schema semantics.

use std::cmp::Ordering;

use serde_json::{Map, Number, Value};

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

    match input {
        Value::Number(value) => check_numeric_bounds(value, schema, path)?,
        Value::String(text) => check_length_bounds(text, schema, path)?,
        Value::Array(items) => check_item_bounds(items.len(), schema, path)?,
        _ => {}
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

/// Checks the numeric bound keywords against a JSON number.
///
/// `exclusiveMinimum` and `exclusiveMaximum` are read in their draft 2019-09
/// form, where the keyword's value *is* the bound. Draft 4 wrote them as a
/// boolean modifying `minimum`/`maximum`; a boolean is not a number, so such
/// a schema simply carries no exclusive bound here and nothing is rejected
/// for it, which keeps the module's promise that an unimplemented spelling
/// never refuses an input.
fn check_numeric_bounds(
    value: &Number,
    schema: &Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    let bound_of = |keyword: &str| match schema.get(keyword) {
        Some(Value::Number(bound)) => Some(bound),
        _ => None,
    };

    if let Some(bound) = bound_of("minimum")
        && compare_numbers(value, bound) == Some(Ordering::Less)
    {
        return Err(out_of_range(path, "minimum", bound, value));
    }
    if let Some(bound) = bound_of("exclusiveMinimum")
        && matches!(
            compare_numbers(value, bound),
            Some(Ordering::Less | Ordering::Equal)
        )
    {
        return Err(out_of_range(path, "exclusiveMinimum", bound, value));
    }
    if let Some(bound) = bound_of("maximum")
        && compare_numbers(value, bound) == Some(Ordering::Greater)
    {
        return Err(out_of_range(path, "maximum", bound, value));
    }
    if let Some(bound) = bound_of("exclusiveMaximum")
        && matches!(
            compare_numbers(value, bound),
            Some(Ordering::Greater | Ordering::Equal)
        )
    {
        return Err(out_of_range(path, "exclusiveMaximum", bound, value));
    }

    Ok(())
}

/// Checks `minLength`/`maxLength` against a JSON string.
///
/// The length is counted in Unicode code points, which is what JSON Schema
/// specifies, so an accented character or an emoji counts once rather than
/// once per UTF-8 byte.
fn check_length_bounds(text: &str, schema: &Map<String, Value>, path: &str) -> Result<(), String> {
    let length = text.chars().count() as u64;

    if let Some(bound) = schema.get("minLength").and_then(Value::as_u64)
        && length < bound
    {
        return Err(format!(
            "{path}: minLength is {bound}, got a string of length {length}"
        ));
    }
    if let Some(bound) = schema.get("maxLength").and_then(Value::as_u64)
        && length > bound
    {
        return Err(format!(
            "{path}: maxLength is {bound}, got a string of length {length}"
        ));
    }

    Ok(())
}

/// Checks `minItems`/`maxItems` against an array's element count.
fn check_item_bounds(count: usize, schema: &Map<String, Value>, path: &str) -> Result<(), String> {
    let count = count as u64;

    if let Some(bound) = schema.get("minItems").and_then(Value::as_u64)
        && count < bound
    {
        return Err(format!(
            "{path}: minItems is {bound}, got {}",
            item_count(count)
        ));
    }
    if let Some(bound) = schema.get("maxItems").and_then(Value::as_u64)
        && count > bound
    {
        return Err(format!(
            "{path}: maxItems is {bound}, got {}",
            item_count(count)
        ));
    }

    Ok(())
}

/// The one message shape every numeric bound uses: where, which keyword,
/// what the bound was, and what arrived.
fn out_of_range(path: &str, keyword: &str, bound: &Number, value: &Number) -> String {
    format!("{path}: {keyword} is {bound}, got {value}")
}

/// An element count with its noun, so a message reads "got 1 item" rather
/// than "got 1 items".
fn item_count(count: u64) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    }
}

/// Orders two JSON numbers whose forms may differ.
///
/// A schema written in TOML or JSON says `maximum = 99999` (an integer) and
/// the input may arrive as `99999.5` (a float), or the other way round, so
/// the comparison cannot assume one representation. Two integers of the same
/// signedness compare exactly, which keeps a bound near `i64::MAX` from being
/// decided by a rounded `f64`; anything else falls back to `f64`. A number
/// with no `f64` at all (which serde_json's default representation never
/// produces) is left uncompared, and an uncompared number is not rejected.
fn compare_numbers(value: &Number, bound: &Number) -> Option<Ordering> {
    if let (Some(value), Some(bound)) = (value.as_i64(), bound.as_i64()) {
        return Some(value.cmp(&bound));
    }
    if let (Some(value), Some(bound)) = (value.as_u64(), bound.as_u64()) {
        return Some(value.cmp(&bound));
    }
    value.as_f64()?.partial_cmp(&bound.as_f64()?)
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
        let schema = json!({
            "type": "string",
            "pattern": "^[0-9]+$",
            "format": "email",
            "minLength": 3
        });
        // `pattern` and `format` are ignored; `minLength` is not, and this
        // string satisfies it, so the input passes on the honored keyword.
        assert!(validate_against_schema(&json!("not digits"), &schema).is_ok());
        assert!(validate_against_schema(&json!({"anything": 1}), &json!(true)).is_ok());

        let ignored = json!({"type": "array", "uniqueItems": true, "multipleOf": 7});
        assert!(validate_against_schema(&json!([1, 1, 1]), &ignored).is_ok());
    }

    /// `minimum` and `maximum` are inclusive: the bound itself passes, one
    /// step past it does not, and the message names path, keyword, bound
    /// and value.
    #[test]
    fn inclusive_numeric_bounds_hold_both_sides() {
        let schema = json!({
            "type": "object",
            "required": ["amount_cents"],
            "properties": {
                "amount_cents": {"type": "integer", "minimum": 1, "maximum": 99999}
            }
        });

        assert!(validate_against_schema(&json!({"amount_cents": 1}), &schema).is_ok());
        assert!(validate_against_schema(&json!({"amount_cents": 99999}), &schema).is_ok());

        let under = validate_against_schema(&json!({"amount_cents": 0}), &schema).unwrap_err();
        assert_eq!(under, "$.amount_cents: minimum is 1, got 0");

        let over = validate_against_schema(&json!({"amount_cents": 240_000}), &schema).unwrap_err();
        assert_eq!(over, "$.amount_cents: maximum is 99999, got 240000");
    }

    /// The draft 2019-09 exclusive bounds reject the bound itself, and
    /// draft 4's boolean spelling is not a bound at all.
    #[test]
    fn exclusive_numeric_bounds_reject_the_bound_itself() {
        let schema = json!({"exclusiveMinimum": 0, "exclusiveMaximum": 10});

        assert!(validate_against_schema(&json!(0.5), &schema).is_ok());
        assert!(validate_against_schema(&json!(9.999), &schema).is_ok());

        assert_eq!(
            validate_against_schema(&json!(0), &schema).unwrap_err(),
            "$: exclusiveMinimum is 0, got 0"
        );
        assert_eq!(
            validate_against_schema(&json!(10), &schema).unwrap_err(),
            "$: exclusiveMaximum is 10, got 10"
        );
        assert_eq!(
            validate_against_schema(&json!(-1), &schema).unwrap_err(),
            "$: exclusiveMinimum is 0, got -1"
        );

        // Draft 4 wrote these as booleans modifying `minimum`. A boolean is
        // not a number, so no exclusive bound is read and only `minimum`
        // applies, inclusively.
        let draft4 = json!({"minimum": 0, "exclusiveMinimum": true});
        assert!(validate_against_schema(&json!(0), &draft4).is_ok());
    }

    /// A bound and a value may disagree about integer versus float, in
    /// either direction, and still compare correctly.
    #[test]
    fn numeric_bounds_mix_integers_and_floats() {
        let integral_bound = json!({"maximum": 10});
        assert!(validate_against_schema(&json!(9.75), &integral_bound).is_ok());
        assert!(validate_against_schema(&json!(10.0), &integral_bound).is_ok());
        assert_eq!(
            validate_against_schema(&json!(10.25), &integral_bound).unwrap_err(),
            "$: maximum is 10, got 10.25"
        );

        let float_bound = json!({"minimum": 2.5});
        assert!(validate_against_schema(&json!(3), &float_bound).is_ok());
        assert_eq!(
            validate_against_schema(&json!(2), &float_bound).unwrap_err(),
            "$: minimum is 2.5, got 2"
        );

        // A bound past the f64 integer range still compares exactly.
        let huge = json!({"maximum": 9_007_199_254_740_993i64});
        assert!(validate_against_schema(&json!(9_007_199_254_740_993i64), &huge).is_ok());

        // Bounds apply only to numbers; a string is untouched by them.
        assert!(validate_against_schema(&json!("10.25"), &integral_bound).is_ok());
    }

    /// `minLength` and `maxLength` bound a string's length in code points.
    #[test]
    fn string_length_bounds_hold_both_sides() {
        let schema = json!({
            "type": "object",
            "properties": {"order_id": {"type": "string", "minLength": 3, "maxLength": 8}}
        });

        assert!(validate_against_schema(&json!({"order_id": "ORD"}), &schema).is_ok());
        assert!(validate_against_schema(&json!({"order_id": "ORD-7781"}), &schema).is_ok());

        assert_eq!(
            validate_against_schema(&json!({"order_id": "OR"}), &schema).unwrap_err(),
            "$.order_id: minLength is 3, got a string of length 2"
        );
        assert_eq!(
            validate_against_schema(&json!({"order_id": "ORD-77812"}), &schema).unwrap_err(),
            "$.order_id: maxLength is 8, got a string of length 9"
        );

        let four = json!({"maxLength": 4});
        // Code points, not UTF-8 bytes: `héllo` is five characters (six
        // bytes) and fails, `hého` is four (five bytes) and passes.
        assert!(validate_against_schema(&json!("hého"), &four).is_ok());
        assert_eq!(
            validate_against_schema(&json!("héllo"), &four).unwrap_err(),
            "$: maxLength is 4, got a string of length 5"
        );
    }

    /// `minItems` and `maxItems` bound an array's element count, and the
    /// message counts one item singularly.
    #[test]
    fn array_item_bounds_hold_both_sides() {
        let schema = json!({"type": "array", "minItems": 2, "maxItems": 3});

        assert!(validate_against_schema(&json!([1, 2]), &schema).is_ok());
        assert!(validate_against_schema(&json!([1, 2, 3]), &schema).is_ok());

        assert_eq!(
            validate_against_schema(&json!([1]), &schema).unwrap_err(),
            "$: minItems is 2, got 1 item"
        );
        assert_eq!(
            validate_against_schema(&json!([1, 2, 3, 4]), &schema).unwrap_err(),
            "$: maxItems is 3, got 4 items"
        );

        // The count is checked at the path where the array sits.
        let nested = json!({
            "type": "object",
            "properties": {"lines": {"type": "array", "minItems": 1}}
        });
        assert_eq!(
            validate_against_schema(&json!({"lines": []}), &nested).unwrap_err(),
            "$.lines: minItems is 1, got 0 items"
        );
    }

    /// Bounds inside `items` are checked per element, at the element's own
    /// path.
    #[test]
    fn bounds_inside_items_name_the_element() {
        let schema = json!({
            "type": "array",
            "items": {"type": "integer", "maximum": 5}
        });

        assert!(validate_against_schema(&json!([1, 5]), &schema).is_ok());
        assert_eq!(
            validate_against_schema(&json!([1, 6]), &schema).unwrap_err(),
            "$[1]: maximum is 5, got 6"
        );
    }
}
