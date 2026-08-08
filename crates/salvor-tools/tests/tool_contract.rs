//! Exercises the tool-contract layer end to end: a hand-written `CreateTicket`
//! tool driven through both the typed and the type-erased
//! paths, plus the metadata surface, input validation, suspension, the
//! registry, and the retry table.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use salvor_core::Effect;
use salvor_tools::{
    DynTool, HandlerError, RegistryError, RetryPolicy, Suspension, ToolCtx, ToolError, ToolHandler,
    ToolMeta, ToolOutcome, ToolSet, TypedTool,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

// --- A hand-written tool -------------------------------------------------

/// The typed request the model must supply. Two properties with distinct names
/// so the schema assertion is unambiguous.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct TicketRequest {
    summary: String,
    priority: u8,
}

/// The typed response the tool produces.
#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
struct TicketRef {
    id: String,
    /// Echoes the idempotency key the context carried, so tests can prove the
    /// context reached the handler.
    idempotency_key: Option<String>,
}

struct CreateTicket;

impl ToolMeta for CreateTicket {
    const NAME: &'static str = "create_ticket";
    const DESCRIPTION: &'static str = "Create a Jira ticket";
    const EFFECT: Effect = Effect::Write;
}

#[async_trait]
impl ToolHandler for CreateTicket {
    type Input = TicketRequest;
    type Output = TicketRef;

    async fn call(
        &self,
        ctx: &ToolCtx,
        input: TicketRequest,
    ) -> Result<ToolOutcome<TicketRef>, HandlerError> {
        if input.priority > 5 {
            return Err(HandlerError::message("priority must be between 0 and 5"));
        }
        Ok(ToolOutcome::Output(TicketRef {
            id: format!("JIRA-{}", input.summary.len()),
            idempotency_key: ctx.idempotency_key().map(str::to_owned),
        }))
    }
}

// --- A tool that opts into an output schema ------------------------------

struct CreateTicketWithReceipt;

impl ToolMeta for CreateTicketWithReceipt {
    const NAME: &'static str = "create_ticket_with_receipt";
    const DESCRIPTION: &'static str = "Create a Jira ticket, requiring a verifiable receipt";
    const EFFECT: Effect = Effect::Write;
}

#[async_trait]
impl ToolHandler for CreateTicketWithReceipt {
    type Input = TicketRequest;
    type Output = TicketRef;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: TicketRequest,
    ) -> Result<ToolOutcome<TicketRef>, HandlerError> {
        Ok(ToolOutcome::Output(TicketRef {
            id: format!("JIRA-{}", input.summary.len()),
            idempotency_key: None,
        }))
    }

    // The one-line opt-in documented on `ToolHandler::output_schema`: `Output`
    // (`TicketRef`) already derives `JsonSchema`, so the same `schema_for!`
    // machinery `input_schema` uses covers it.
    fn output_schema() -> Option<serde_json::Value> {
        Some(serde_json::to_value(schemars::schema_for!(Self::Output)).unwrap())
    }
}

// --- A human-in-the-loop tool that suspends -----------------------------

struct RequestApproval;

impl ToolMeta for RequestApproval {
    const NAME: &'static str = "request_approval";
    const DESCRIPTION: &'static str = "Ask a human to approve the plan";
    const EFFECT: Effect = Effect::Read;
}

#[async_trait]
impl ToolHandler for RequestApproval {
    type Input = TicketRequest;
    type Output = TicketRef;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _input: TicketRequest,
    ) -> Result<ToolOutcome<TicketRef>, HandlerError> {
        Ok(ToolOutcome::Suspend(Suspension {
            reason: "manager approval required".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "approved": { "type": "boolean" } } }),
        }))
    }
}

// --- A tool that records whether its handler body ran -------------------

struct SpyTool<'a>(&'a AtomicBool);

impl ToolMeta for SpyTool<'_> {
    const NAME: &'static str = "spy";
    const DESCRIPTION: &'static str = "Records whether its body executed";
    const EFFECT: Effect = Effect::Read;
}

#[async_trait]
impl ToolHandler for SpyTool<'_> {
    type Input = TicketRequest;
    type Output = TicketRef;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: TicketRequest,
    ) -> Result<ToolOutcome<TicketRef>, HandlerError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(ToolOutcome::Output(TicketRef {
            id: input.summary,
            idempotency_key: None,
        }))
    }
}

// --- Criterion 1: typed and erased paths both work ----------------------

#[tokio::test]
async fn typed_call_produces_typed_output() {
    let ctx = ToolCtx::new(Some("attempt-1".to_owned()));
    let outcome = CreateTicket
        .call(
            &ctx,
            TicketRequest {
                summary: "fix login".to_owned(),
                priority: 2,
            },
        )
        .await
        .expect("handler succeeds");

    match outcome {
        ToolOutcome::Output(ticket) => {
            assert_eq!(ticket.id, "JIRA-9");
            assert_eq!(ticket.idempotency_key.as_deref(), Some("attempt-1"));
        }
        ToolOutcome::Suspend(_) | ToolOutcome::Sleep(_) => panic!("expected an output, not a park"),
    }
}

#[tokio::test]
async fn erased_call_roundtrips_through_json() {
    let tool = TypedTool::new(CreateTicket);
    let ctx = ToolCtx::new(Some("attempt-7".to_owned()));

    let outcome = tool
        .call_json(&ctx, json!({ "summary": "fix login", "priority": 2 }))
        .await
        .expect("valid input dispatches");

    match outcome {
        ToolOutcome::Output(value) => {
            assert_eq!(
                value,
                json!({ "id": "JIRA-9", "idempotency_key": "attempt-7" })
            );
        }
        ToolOutcome::Suspend(_) | ToolOutcome::Sleep(_) => panic!("expected an output, not a park"),
    }
}

// --- Criterion 2: metadata surface + schema names properties ------------

#[test]
fn descriptor_exposes_name_description_effect_and_schema() {
    let tool = TypedTool::new(CreateTicket);
    let descriptor = tool.descriptor();

    assert_eq!(descriptor.name, "create_ticket");
    assert_eq!(descriptor.description, "Create a Jira ticket");
    assert_eq!(descriptor.effect, Effect::Write);

    let properties = descriptor
        .input_schema
        .get("properties")
        .expect("object schema has properties");
    assert!(
        properties.get("summary").is_some(),
        "schema names `summary`"
    );
    assert!(
        properties.get("priority").is_some(),
        "schema names `priority`"
    );

    // A tool that never overrides `ToolHandler::output_schema` descends the
    // default all the way through `TypedTool` to the descriptor: `None`, not
    // an empty schema.
    assert_eq!(
        descriptor.output_schema, None,
        "a tool that declares no output schema reports None, not a schema"
    );
}

#[test]
fn descriptor_surfaces_a_declared_output_schema() {
    let tool = TypedTool::new(CreateTicketWithReceipt);
    let descriptor = tool.descriptor();

    let schema = descriptor
        .output_schema
        .expect("the tool opted into an output schema");
    let properties = schema
        .get("properties")
        .expect("object schema has properties");
    assert!(properties.get("id").is_some(), "schema names `id`");
    assert!(
        properties.get("idempotency_key").is_some(),
        "schema names `idempotency_key`"
    );
}

// --- Criterion 3: bad input -> distinct error, handler never runs -------

#[tokio::test]
async fn malformed_input_yields_invalid_input_without_running_handler() {
    let ran = AtomicBool::new(false);
    let tool = TypedTool::new(SpyTool(&ran));
    let ctx = ToolCtx::new(None);

    // `priority` is a string where a number is required.
    let result = tool
        .call_json(&ctx, json!({ "summary": "x", "priority": "high" }))
        .await;

    match result {
        Err(ToolError::InvalidInput { tool, .. }) => assert_eq!(tool, "spy"),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    assert!(
        !ran.load(Ordering::SeqCst),
        "handler body must not have run"
    );
}

#[tokio::test]
async fn handler_failure_is_distinct_from_invalid_input() {
    let tool = TypedTool::new(CreateTicket);
    let ctx = ToolCtx::new(None);

    // Well-formed input, but the handler rejects the value.
    let result = tool
        .call_json(&ctx, json!({ "summary": "x", "priority": 9 }))
        .await;

    match result {
        Err(ToolError::Handler { tool, source }) => {
            assert_eq!(tool, "create_ticket");
            assert_eq!(source.to_string(), "priority must be between 0 and 5");
        }
        other => panic!("expected Handler error, got {other:?}"),
    }
}

// --- Criterion 4: suspension propagates through the erased layer --------

#[tokio::test]
async fn suspension_propagates_as_outcome_not_error() {
    let tool = TypedTool::new(RequestApproval);
    let ctx = ToolCtx::new(None);

    let outcome = tool
        .call_json(&ctx, json!({ "summary": "ship it", "priority": 1 }))
        .await
        .expect("suspension is a success outcome, not an error");

    match outcome {
        ToolOutcome::Suspend(Suspension {
            reason,
            input_schema,
        }) => {
            assert_eq!(reason, "manager approval required");
            assert_eq!(
                input_schema,
                json!({ "type": "object", "properties": { "approved": { "type": "boolean" } } })
            );
        }
        ToolOutcome::Output(_) | ToolOutcome::Sleep(_) => panic!("expected a suspension"),
    }
}

// --- Criterion 5: registry register/lookup/enumerate/duplicate ----------

#[tokio::test]
async fn registry_registers_looks_up_and_enumerates() {
    let mut set = ToolSet::new();
    set.register(CreateTicket).expect("first registration");
    set.register(RequestApproval).expect("second registration");

    assert_eq!(set.len(), 2);

    let found = set.get("create_ticket").expect("tool is present");
    assert_eq!(found.effect(), Effect::Write);

    assert!(set.get("does_not_exist").is_none());

    // Enumeration is name-sorted (BTreeMap): create_ticket before request_approval.
    let names: Vec<_> = set.descriptors().into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["create_ticket", "request_approval"]);
}

#[test]
fn registry_rejects_duplicate_names() {
    let mut set = ToolSet::new();
    set.register(CreateTicket).expect("first registration");

    let err = set.register(CreateTicket).expect_err("duplicate must fail");
    assert_eq!(
        err,
        RegistryError::DuplicateName {
            name: "create_ticket".to_owned()
        }
    );
    assert_eq!(set.len(), 1, "the duplicate must not be inserted");
}

// --- Criterion 6: retry policy pins the effect-to-retry table -----------

#[test]
fn retry_policy_matches_the_prd_table() {
    assert_eq!(RetryPolicy::for_effect(Effect::Read), RetryPolicy::Retry);
    assert_eq!(
        RetryPolicy::for_effect(Effect::Idempotent),
        RetryPolicy::RetryWithSameKey
    );
    assert_eq!(RetryPolicy::for_effect(Effect::Write), RetryPolicy::Never);

    assert!(RetryPolicy::for_effect(Effect::Read).allows_retry());
    assert!(RetryPolicy::for_effect(Effect::Idempotent).allows_retry());
    assert!(!RetryPolicy::for_effect(Effect::Write).allows_retry());
}
