//! End-to-end coverage of `#[derive(Tool)]`: a `CreateTicket` example
//! compiled verbatim (modulo imports), the `name = "..."` override, the
//! default `snake_case` rule across three identifier shapes, and the hygiene
//! guarantee that a shadowed `Effect` in the caller's scope does not disturb
//! the generated impl. The derive writes only the `ToolMeta` half; every
//! `ToolHandler` here is hand-written, exactly as a tool author would.

use async_trait::async_trait;
use salvor_tools::{
    Effect, HandlerError, Tool, ToolCtx, ToolHandler, ToolMeta, ToolOutcome, ToolSet,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct TicketRequest {
    summary: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct TicketRef {
    id: String,
}

// --- The example, verbatim modulo imports --------------------------------

#[derive(Tool)]
#[tool(effect = "write", description = "Create a Jira ticket")]
struct CreateTicket;

#[async_trait]
impl ToolHandler for CreateTicket {
    type Input = TicketRequest;
    type Output = TicketRef;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: TicketRequest,
    ) -> Result<ToolOutcome<TicketRef>, HandlerError> {
        Ok(ToolOutcome::Output(TicketRef {
            id: format!("JIRA-{}", input.summary.len()),
        }))
    }
}

#[test]
fn derives_meta_from_attributes() {
    assert_eq!(CreateTicket::NAME, "create_ticket");
    assert_eq!(CreateTicket::DESCRIPTION, "Create a Jira ticket");
    assert_eq!(CreateTicket::EFFECT, Effect::Write);
}

#[tokio::test]
async fn derived_tool_registers_and_dispatches() {
    let mut tools = ToolSet::new();
    tools.register(CreateTicket).expect("first registration");

    // The name the derive produced is the key the registry stores it under.
    let tool = tools.get("create_ticket").expect("tool is registered");
    assert_eq!(tool.description(), "Create a Jira ticket");
    assert_eq!(tool.effect(), Effect::Write);

    let outcome = tool
        .call_json(&ToolCtx::new(None), json!({ "summary": "fix login" }))
        .await
        .expect("dispatch succeeds");

    match outcome {
        ToolOutcome::Output(value) => assert_eq!(value, json!({ "id": "JIRA-9" })),
        ToolOutcome::Suspend(_) | ToolOutcome::Sleep(_) => {
            panic!("tool returned an output, not a park")
        }
    }
}

// --- The name override --------------------------------------------------

#[derive(Tool)]
#[tool(
    name = "make_ticket",
    effect = "write",
    description = "Create a ticket"
)]
struct RenamedTool;

#[test]
fn name_attribute_overrides_the_default() {
    assert_eq!(RenamedTool::NAME, "make_ticket");
}

// --- The default snake_case rule across identifier shapes ---------------

#[derive(Tool)]
#[tool(effect = "read", description = "Fetch over HTTP")]
struct HTTPFetch;

#[derive(Tool)]
#[tool(effect = "read", description = "A one-word tool")]
struct Ticket;

#[test]
fn default_name_is_snake_case() {
    // Word boundary after a lowercase letter.
    assert_eq!(CreateTicket::NAME, "create_ticket");
    // Acronym run: the underscore lands before the last capital that begins a
    // new lowercase word, not between every capital.
    assert_eq!(HTTPFetch::NAME, "http_fetch");
    // A single word is simply lowercased.
    assert_eq!(Ticket::NAME, "ticket");
}

// --- Hygiene: a shadowing `Effect` in scope must not matter -------------

/// This module deliberately defines its own `Effect` type and does *not*
/// import `salvor_tools::Effect`. If the derive named `Effect` unqualified, the
/// generated constant would bind this local type and fail to compile. Because
/// the derive names `::salvor_tools::Effect` from the crate root, the impl
/// compiles here and still carries the real effect class.
mod hygiene {
    use salvor_tools::{Tool, ToolMeta};

    /// The tool author's own unrelated `Effect`, shadowing the runtime's.
    struct Effect;

    #[derive(Tool)]
    #[tool(effect = "idempotent", description = "Charge a card once")]
    struct ChargeCard;

    #[test]
    fn generated_paths_ignore_a_shadowed_effect() {
        // Prove the shadow is really in scope and really unrelated.
        let _shadow = Effect;
        assert_eq!(ChargeCard::EFFECT, salvor_tools::Effect::Idempotent);
    }
}
