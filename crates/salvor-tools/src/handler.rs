//! The typed tool contract: [`ToolMeta`] (a tool's identity and effect) and
//! [`ToolHandler`] (its typed input, output, and behavior).
//!
//! These two traits are split along the seam the future `#[derive(Tool)]`
//! macro will cut, described under [the derive seam](#the-derive-seam).

use async_trait::async_trait;
use salvor_core::Effect;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::context::ToolCtx;
use crate::error::HandlerError;
use crate::outcome::ToolOutcome;

/// A tool's static identity: the name a model calls it by, a human
/// description, and its side-effect [`Effect`] class.
///
/// This is the metadata half of the tool contract, split out from
/// [`ToolHandler`] on purpose (see [the derive seam](#the-derive-seam)). It
/// carries no behavior and no associated types, so a proc-macro can generate
/// it from struct-level attributes with nothing to infer.
///
/// The three members are associated constants because a native Rust tool knows
/// all three at compile time. (Tools whose identity is known only at runtime,
/// such as MCP-backed tools, do not implement this trait at all; they
/// implement the type-erased [`DynTool`](crate::DynTool) directly, whose
/// name/description/effect are methods.)
///
/// # The derive seam
///
/// Tool definition is a derive plus a hand-written impl:
///
/// ```ignore
/// #[derive(Tool)]
/// #[tool(effect = "write", description = "Create a Jira ticket")]
/// struct CreateTicket;
///
/// impl ToolHandler for CreateTicket {
///     type Input = TicketRequest;
///     type Output = TicketRef;
///     async fn call(&self, ctx: &ToolCtx, input: TicketRequest) -> Result<...> { ... }
/// }
/// ```
///
/// The split between these two traits is exactly the split between what the
/// macro writes and what the user writes:
///
/// - **`#[derive(Tool)]` generates the `ToolMeta` impl.** It reads the struct
///   name for [`NAME`](Self::NAME) (overridable by a `name = "..."` attribute),
///   the `description = "..."` attribute for [`DESCRIPTION`](Self::DESCRIPTION),
///   and the `effect = "read" | "idempotent" | "write"` attribute for
///   [`EFFECT`](Self::EFFECT). It generates no `call` body and touches none of
///   the typed input or output.
/// - **The user writes the `ToolHandler` impl.** They choose the `Input` and
///   `Output` types and write the async `call`. The input JSON Schema is not
///   hand-written: it is derived from `Input` by `schemars`, surfaced by the
///   provided [`ToolHandler::input_schema`] method.
///
/// Because the metadata lives in its own trait with no reference to `Input`,
/// `Output`, or `call`, the macro never has to parse or reason about the
/// handler body. That is the whole reason for the split, and it is why this
/// trait must stay behavior-free.
pub trait ToolMeta {
    /// The name a model calls this tool by. Unique within a
    /// [`ToolSet`](crate::ToolSet).
    const NAME: &'static str;
    /// A human-readable description handed to the model alongside the schema.
    const DESCRIPTION: &'static str;
    /// The side-effect class that governs this tool's retry and resume
    /// behavior. See [`Effect`] and [`RetryPolicy`](crate::RetryPolicy).
    const EFFECT: Effect;
}

/// The behavior half of the tool contract: typed input, typed output, and the
/// async `call` that turns one into the other.
///
/// A type implements this by hand (the macro only generates its
/// [`ToolMeta`] supertrait; see [the derive seam](ToolMeta#the-derive-seam)).
/// The associated types carry the bounds the rest of the system needs:
///
/// - `Input: DeserializeOwned` so the type-erased layer can turn the model's
///   JSON into it, and `Input: JsonSchema` so a schema can be generated to
///   hand the model in the first place.
/// - `Output: Serialize` so the type-erased layer can turn the handler's
///   result back into JSON for the event log.
///
/// The `Send`/`Sync` bounds and the `Send` bound on `Input` are what let a
/// handler be wrapped as a [`DynTool`](crate::DynTool) and dispatched from an
/// async runtime that may move the work across threads.
///
/// `call` returns a [`ToolOutcome`], not a bare `Output`: a human-in-the-loop
/// tool may return [`ToolOutcome::Suspend`] to park the run. Its error type is
/// [`HandlerError`], the tool's own failure; a schema mismatch is impossible
/// here because `call` only ever receives an already-deserialized `Input`.
#[async_trait]
pub trait ToolHandler: ToolMeta + Send + Sync {
    /// The typed input the model must supply. `DeserializeOwned` drives erased
    /// dispatch; `JsonSchema` drives the schema handed to the model.
    type Input: DeserializeOwned + JsonSchema + Send;
    /// The typed output the tool produces on success.
    type Output: Serialize;

    /// Runs the tool for one attempt.
    ///
    /// `ctx` carries the idempotency key for this attempt (see [`ToolCtx`]).
    /// The input is already validated and typed. Return
    /// [`ToolOutcome::Output`] on success, [`ToolOutcome::Suspend`] to park the
    /// run for a human, or `Err(`[`HandlerError`]`)` on a genuine failure.
    async fn call(
        &self,
        ctx: &ToolCtx,
        input: Self::Input,
    ) -> Result<ToolOutcome<Self::Output>, HandlerError>;

    /// The JSON Schema for [`Input`](Self::Input), generated by `schemars`.
    ///
    /// This is a provided method: it lives on the handler trait because it
    /// needs the `Input` type, and no tool should override it. The runtime
    /// hands this schema to the model so the model knows how to shape its tool
    /// call. It is surfaced without erasure here and, after wrapping, through
    /// [`DynTool::input_schema`](crate::DynTool::input_schema).
    fn input_schema() -> Value
    where
        Self: Sized,
    {
        serde_json::to_value(schemars::schema_for!(Self::Input))
            .expect("a schemars-generated schema always serializes to JSON")
    }

    /// The JSON Schema a *completion* for this tool must satisfy, if the tool
    /// declares one. `None` by default.
    ///
    /// This is not a schema for `Output`, and nothing validates against it
    /// yet: stage one only lets a tool declare it. What it is for is the
    /// stage that follows. A tool call performed outside the salvor process
    /// (a client recording work it did itself, rather than salvor dispatching
    /// it) has no handler run to trust; the only thing standing between that
    /// and a bare unverifiable claim of "I did the work" is a schema the
    /// completion is required to match, one shaped to demand something a
    /// forger cannot cheaply produce, such as a provider transaction
    /// reference or a settlement id. Declaring the schema here is what makes
    /// that requirement expressible; nothing in this crate checks a
    /// completion against it yet.
    ///
    /// `Self::Output` carries no `JsonSchema` bound (unlike `Self::Input`,
    /// which does), so this cannot default to deriving one: adding that bound
    /// would break every existing implementor of this trait outside this
    /// crate, most of which have no need for an output schema at all. A tool
    /// whose `Output` does implement `JsonSchema` can opt in with one line:
    ///
    /// ```ignore
    /// fn output_schema() -> Option<Value> {
    ///     Some(serde_json::to_value(schemars::schema_for!(Self::Output)).unwrap())
    /// }
    /// ```
    fn output_schema() -> Option<Value>
    where
        Self: Sized,
    {
        None
    }
}
