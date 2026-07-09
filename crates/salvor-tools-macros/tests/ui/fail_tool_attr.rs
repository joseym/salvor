//! Compile-fail fixture: every way a `#[tool(...)]` attribute can be malformed.
//! Each struct triggers exactly one error, and the errors are independent, so
//! trybuild records all of them in `fail_tool_attr.stderr`.

use salvor_tools::Tool;

// The attribute is absent entirely.
#[derive(Tool)]
struct NoAttr;

// `description` is required and missing.
#[derive(Tool)]
#[tool(effect = "read")]
struct NoDescription;

// `effect` is required and missing.
#[derive(Tool)]
#[tool(description = "no effect given")]
struct NoEffect;

// `effect` is present but not one of the three valid values.
#[derive(Tool)]
#[tool(effect = "delete", description = "an invalid effect")]
struct BadEffect;

// An unrecognized key.
#[derive(Tool)]
#[tool(effect = "read", description = "has a stray key", color = "blue")]
struct UnknownKey;

// The same key given twice.
#[derive(Tool)]
#[tool(effect = "read", effect = "write", description = "keyed twice")]
struct DuplicateKey;

// A value that is not a string literal.
#[derive(Tool)]
#[tool(effect = "read", description = 42)]
struct NonStringValue;

fn main() {}
