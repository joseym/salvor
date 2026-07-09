//! Compile-pass fixture: the derive produces a `ToolMeta` impl through the
//! real `salvor_tools::Tool` surface, with a defaulted name, an overridden
//! name, and each of the three effect values. It compiles, so trybuild passes
//! it; the runtime assertions on the derived constants live in
//! `salvor-tools/tests/derive_tool.rs`.

use salvor_tools::{Effect, Tool, ToolMeta};

#[derive(Tool)]
#[tool(effect = "write", description = "Create a Jira ticket")]
struct CreateTicket;

#[derive(Tool)]
#[tool(name = "make_ticket", effect = "read", description = "Read a ticket")]
struct FetchTicket;

#[derive(Tool)]
#[tool(effect = "idempotent", description = "Charge a card once")]
struct ChargeCard;

fn main() {
    assert_eq!(CreateTicket::NAME, "create_ticket");
    assert_eq!(CreateTicket::EFFECT, Effect::Write);
    assert_eq!(FetchTicket::NAME, "make_ticket");
    assert_eq!(FetchTicket::EFFECT, Effect::Read);
    assert_eq!(ChargeCard::EFFECT, Effect::Idempotent);
}
