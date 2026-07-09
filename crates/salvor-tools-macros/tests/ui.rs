//! The trybuild driver for the derive's compiler diagnostics.
//!
//! trybuild compiles each fixture under `tests/ui/` and compares the result to
//! expectations: `pass` fixtures must compile, `compile_fail` fixtures must
//! fail with the exact diagnostics recorded in the neighboring `.stderr` file.
//! This is how the derive's error messages become part of the tested contract,
//! not just prose. Regenerate the `.stderr` files after an intentional message
//! change with `TRYBUILD=overwrite cargo test -p salvor-tools-macros --test ui`.
//!
//! The fixtures live here, with the macro, because they exercise the
//! diagnostics this crate emits. The compile-pass fixture drives the derive
//! through the real `salvor_tools::Tool` surface, which is why this crate keeps
//! `salvor-tools` as a dev-dependency.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    // One compile-pass fixture: the derive produces a usable `ToolMeta` impl.
    t.pass("tests/ui/pass_derive.rs");
    // Malformed `#[tool(...)]` attributes, each error at its offending tokens.
    t.compile_fail("tests/ui/fail_tool_attr.rs");
    // The derive on the wrong data shape (an enum, a union).
    t.compile_fail("tests/ui/fail_data_shape.rs");
}
