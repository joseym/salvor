//! Compile-fail fixture: the derive on a data shape that is not a struct. A
//! tool's identity is one name, so an enum or a union has nothing to derive
//! from. Both errors are recorded in `fail_data_shape.stderr`.

use salvor_tools::Tool;

#[derive(Tool)]
#[tool(effect = "read", description = "not a struct")]
enum NotAStruct {
    A,
    B,
}

#[derive(Tool)]
#[tool(effect = "read", description = "also not a struct")]
union AlsoNotAStruct {
    a: u32,
    b: u32,
}

fn main() {}
