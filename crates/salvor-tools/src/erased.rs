//! The type-erased dispatch layer: [`DynTool`], the `Value`-in/`Value`-out
//! trait every registered tool is stored behind, and [`TypedTool`], the
//! adapter that turns any typed [`ToolHandler`] into a `DynTool`.
//!
//! The runtime loop cannot be generic over each tool's `Input`/`Output`: it
//! dispatches whichever tool the model named, by string, with JSON arguments.
//! So it works against `dyn DynTool`. This module is the bridge from the typed
//! world (where `#[derive(Tool)]` tools live) to that erased world, and it is
//! the same erased shape the future MCP-backed tools implement directly.

use async_trait::async_trait;
use salvor_core::Effect;
use serde_json::Value;

use crate::context::ToolCtx;
use crate::error::ToolError;
use crate::handler::ToolHandler;
use crate::outcome::ToolOutcome;

/// The metadata a [`ToolSet`](crate::ToolSet) exposes for one tool when
/// enumerating tools for a model: everything the model needs to decide whether
/// and how to call it.
///
/// This is a snapshot, owned and cloneable, so it can be serialized into a
/// model request or sent over a future control plane without holding a borrow
/// on the registry.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDescriptor {
    /// The name the model calls the tool by.
    pub name: String,
    /// The human-readable description handed to the model.
    pub description: String,
    /// The side-effect class governing retry and resume behavior.
    pub effect: Effect,
    /// The JSON Schema the tool's input must satisfy.
    pub input_schema: Value,
    /// The JSON Schema a completion for this tool must satisfy, if the tool
    /// declares one. See [`DynTool::output_schema`] for what this is for and
    /// why it is `None` unless a tool opts in.
    pub output_schema: Option<Value>,
}

/// A tool with its types erased: `Value` in, `Value` out, dispatched by name.
///
/// This is the object-safe (dyn-compatible) trait the registry stores as
/// `Box<dyn DynTool>` and the runtime loop calls through. Erasure is what lets
/// tools with different `Input`/`Output` types sit in one collection and be
/// dispatched uniformly.
///
/// Two kinds of tool implement it:
///
/// - Native Rust tools, indirectly: a [`ToolHandler`] is wrapped in
///   [`TypedTool`], which implements `DynTool` by deserializing the input,
///   calling the typed handler, and serializing the output.
/// - MCP-backed tools (a later task), directly: their name, description, and
///   schema are known only at runtime, so they implement these methods rather
///   than the compile-time-constant [`ToolMeta`](crate::ToolMeta).
///
/// The metadata accessors are methods (not associated constants) precisely so
/// the second kind is expressible. `async fn call_json` uses `#[async_trait]`,
/// which rewrites it to return a boxed future; a native `async fn` in a trait
/// is not yet dyn-compatible, and this trait must be `dyn`.
#[async_trait]
pub trait DynTool: Send + Sync {
    /// The name the model calls this tool by.
    fn name(&self) -> &str;
    /// The human-readable description handed to the model.
    fn description(&self) -> &str;
    /// The side-effect class governing retry and resume behavior.
    fn effect(&self) -> Effect;
    /// The JSON Schema the tool's input must satisfy.
    fn input_schema(&self) -> Value;

    /// The JSON Schema a completion for this tool must satisfy, if the tool
    /// declares one. Provided, defaulting to `None`, so no existing
    /// implementor of this trait breaks.
    ///
    /// See [`ToolHandler::output_schema`](crate::ToolHandler::output_schema)
    /// for what this is for: it declares the shape a completion for this
    /// tool must have, which is the mechanism by which a tool call performed
    /// outside the salvor process can be required to carry a verifiable
    /// receipt rather than a bare claim. Stage one only declares it; nothing
    /// in this crate validates a completion against it yet.
    fn output_schema(&self) -> Option<Value> {
        None
    }

    /// The idempotency key this tool declares for `input`, if it declares one.
    /// Provided, defaulting to `None`, so no existing implementor breaks.
    ///
    /// This is the tool naming the effect a call *is*, in its own terms:
    /// `"pay_claim:wreck-9931"` says "this is the payout for claim 9931",
    /// whoever asks for it and whenever. That is a stronger statement than the
    /// keys the runtime derives on a tool's behalf, which only ever say "this
    /// is the same attempt as that attempt" within one run, and it is the
    /// statement the runtime needs before it can promise that a second run will
    /// not pay the claim again. See
    /// [`RunCtx::tool_call`](../../salvor_runtime/struct.RunCtx.html#method.tool_call)
    /// for what the runtime does with it.
    ///
    /// The key must be a pure function of `input`: the same input yields the
    /// same key, on this process and the next one, or the promise it carries is
    /// worth nothing. Do not fold a clock, a counter, or randomness into it.
    /// Return `None` for a call that has no business identity, which is the
    /// honest answer for most tools and the default here.
    ///
    /// A key naming an effect that money or people depend on should be as
    /// specific as the effect is. `"pay_claim"` alone would collapse every
    /// payout the system ever makes into one; the claim id is what makes the
    /// key mean one payment.
    ///
    /// A tool whose code is not here (an MCP server's, a wasm component's)
    /// still gets to make this statement: its operator names the identifying
    /// input field in the agent file and the tool derives the key from it. See
    /// [`IdempotencyPath`](crate::IdempotencyPath) for the format and for what
    /// a call missing that field gets, which is a refusal rather than a
    /// quietly unkeyed call.
    fn idempotency_key(&self, input: &Value) -> Option<String> {
        let _ = input;
        None
    }

    /// Dispatches the tool with JSON input, returning JSON output or a
    /// suspension.
    ///
    /// The contract every implementation honors:
    ///
    /// - Validate `input` against the tool's input type **first**. If it does
    ///   not deserialize, return [`ToolError::InvalidInput`] and do **not** run
    ///   the tool's work. This is what lets the runtime loop feed a bad-arguments
    ///   error back to the model without any side effect having happened.
    /// - On valid input, run the tool and return
    ///   [`ToolOutcome::Output`]`(value)` or [`ToolOutcome::Suspend`]. A
    ///   suspension is a success outcome, not an error.
    async fn call_json(&self, ctx: &ToolCtx, input: Value)
    -> Result<ToolOutcome<Value>, ToolError>;

    /// A cloneable metadata snapshot for handing to a model. Provided; built
    /// from the accessors above.
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            effect: self.effect(),
            input_schema: self.input_schema(),
            output_schema: self.output_schema(),
        }
    }
}

/// Wraps a typed [`ToolHandler`] as a type-erased [`DynTool`].
///
/// This adapter is where erasure happens for native Rust tools:
/// [`call_json`](DynTool::call_json) deserializes the incoming `Value` into the
/// handler's `Input`, calls the typed handler, and serializes its `Output` back
/// to a `Value`. A [`Suspension`](crate::Suspension) passes through untouched.
///
/// A newtype wrapper is used rather than a blanket `impl<H: ToolHandler>
/// DynTool for H`, so that MCP-backed tools remain free to implement `DynTool`
/// directly for their own types without a coherence conflict.
///
/// [`ToolSet::register`](crate::ToolSet::register) wraps handlers in this
/// automatically, so most callers never name it.
pub struct TypedTool<H>(pub H);

impl<H> TypedTool<H> {
    /// Wraps a handler.
    pub fn new(handler: H) -> Self {
        Self(handler)
    }
}

#[async_trait]
impl<H: ToolHandler> DynTool for TypedTool<H> {
    fn name(&self) -> &str {
        H::NAME
    }

    fn description(&self) -> &str {
        H::DESCRIPTION
    }

    fn effect(&self) -> Effect {
        H::EFFECT
    }

    fn input_schema(&self) -> Value {
        H::input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        H::output_schema()
    }

    /// Forwards to the typed [`ToolHandler::idempotency_key`], which sees the
    /// deserialized input rather than raw JSON.
    ///
    /// The input is deserialized a second time here, once per keyed dispatch,
    /// because the key is needed *before*
    /// [`call_json`](DynTool::call_json) runs and that is where the first
    /// deserialization happens. Input that does not deserialize yields `None`:
    /// the dispatch below is about to reject it as
    /// [`ToolError::InvalidInput`](crate::ToolError::InvalidInput) anyway, and a
    /// call that cannot run has no effect to name.
    fn idempotency_key(&self, input: &Value) -> Option<String> {
        let typed: H::Input = serde_json::from_value(input.clone()).ok()?;
        self.0.idempotency_key(&typed)
    }

    async fn call_json(
        &self,
        ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        // Validate first: a deserialization failure is the model's bad input,
        // and the handler must not run in that case.
        let typed: H::Input =
            serde_json::from_value(input).map_err(|source| ToolError::InvalidInput {
                tool: H::NAME.to_owned(),
                source,
            })?;

        let outcome = self
            .0
            .call(ctx, typed)
            .await
            .map_err(|source| ToolError::Handler {
                tool: H::NAME.to_owned(),
                source,
            })?;

        match outcome {
            ToolOutcome::Output(output) => {
                let value = serde_json::to_value(output).map_err(|source| {
                    ToolError::OutputSerialization {
                        tool: H::NAME.to_owned(),
                        source,
                    }
                })?;
                Ok(ToolOutcome::Output(value))
            }
            // A suspension is identical on both sides of the erasure boundary,
            // so it crosses unchanged.
            ToolOutcome::Suspend(suspension) => Ok(ToolOutcome::Suspend(suspension)),
        }
    }
}
