//! Salvor tools: the typed tool-contract layer for the agent runtime.
//!
//! A tool is a typed, effect-classified operation a model may call. This crate
//! defines what a tool *is* and how the runtime dispatches one; it declares
//! contracts and performs no IO. Persisting tool-call events and driving the
//! dispatch is the runtime's job, built on the seam this crate exposes.
//!
//! # The layers
//!
//! - **The typed contract.** [`ToolMeta`] carries a tool's identity and
//!   [`Effect`](salvor_core::Effect); [`ToolHandler`] adds its typed `Input`,
//!   `Output`, and async [`call`](ToolHandler::call). The split is the seam the
//!   future `#[derive(Tool)]` macro cuts: the macro generates `ToolMeta`, the
//!   user writes `ToolHandler`. See [`ToolMeta`]'s docs for the exact contract.
//! - **The tool outcome.** [`ToolHandler::call`] returns a [`ToolOutcome`]:
//!   either an `Output`, or a [`Suspension`] that parks the run for a human.
//!   Suspension is a return value in v0.1, not a runtime call.
//! - **Type-erased dispatch.** [`DynTool`] is the `Value`-in/`Value`-out,
//!   dyn-compatible trait the runtime dispatches through. [`TypedTool`] adapts
//!   any [`ToolHandler`] into a `DynTool`, validating the model's JSON against
//!   the input type before the handler runs. MCP-backed tools (a later task)
//!   implement `DynTool` directly.
//! - **The registry.** [`ToolSet`] registers tools by name, looks them up, and
//!   enumerates them as [`ToolDescriptor`]s for a model. Duplicate names are a
//!   [`RegistryError`].
//! - **Retry policy.** [`RetryPolicy`] encodes the per-effect rule for
//!   retrying a failed *live* execution. It classifies; the runtime loop enforces.
//!
//! # Errors
//!
//! A tool's own failure is a [`HandlerError`]. The erased layer's error is a
//! [`ToolError`], whose [`InvalidInput`](ToolError::InvalidInput) variant (the
//! model sent malformed arguments, and the handler never ran) is deliberately
//! distinct from [`Handler`](ToolError::Handler) (the tool ran and failed), so
//! the runtime loop can route them differently.

mod context;
mod erased;
mod error;
mod handler;
mod outcome;
mod registry;
mod retry;

/// Derives the [`ToolMeta`] impl for a tool struct from `#[tool(...)]`
/// attributes. See the macro's own documentation for the attribute keys, the
/// default-name rule, and what it rejects.
pub use salvor_tools_macros::Tool;

/// The side-effect classification a tool declares, re-exported from
/// `salvor_core` so a tool author needs only this crate. Both the hand-written
/// [`ToolMeta::EFFECT`] and the [`Tool`] derive name it through here.
pub use salvor_core::Effect;

pub use context::ToolCtx;
pub use erased::{DynTool, ToolDescriptor, TypedTool};
pub use error::{HandlerError, ToolError};
pub use handler::{ToolHandler, ToolMeta};
pub use outcome::{Suspension, ToolOutcome};
pub use registry::{RegistryError, ToolSet};
pub use retry::RetryPolicy;
