//! Salvor runtime: the IO edge where core (replay), store, llm, and tools
//! meet. Durable, resumable agent runs with hard budgets, in two tiers.
//!
//! # The two tiers
//!
//! - **The substrate: [`RunCtx`].** The library-first tier. A team that
//!   owns its control flow writes an ordinary async function against
//!   `RunCtx` (recorded model calls, recorded tool dispatch, `now`,
//!   `random`, suspension) and gets durability, crash-exact replay, and the
//!   suspension protocol with no framework in the way. A runtime that sits
//!   under your loop is infrastructure; this type is that runtime.
//! - **The batteries: [`Agent`] + [`Runtime`].** [`Agent::builder`]
//!   configures model, prompt, tools, budgets, pricing;
//!   [`Runtime::start`] / [`Runtime::recover`] / [`Runtime::resume`] drive
//!   the single built-in loop. The loop is implemented entirely against the
//!   public `RunCtx` surface and doubles as its reference example; it holds
//!   no private hooks, by design and by test.
//!
//! # Where this crate sits
//!
//! The agent loop cannot live inside `salvor-core`: `salvor-store` and
//! `salvor-tools` both depend on
//! `salvor-core`, and the loop depends on both of them (plus `salvor-llm`),
//! so a loop in core would be a dependency cycle. This crate is the
//! resolution: core keeps the pure event model and replay engine with **no
//! new dependencies**, and `salvor-runtime` sits above all four crates as
//! the one place allowed to do IO. For the same reason this is the one
//! crate that may be tokio-bound: it is the IO edge, while core and store
//! stay executor-agnostic.
//!
//! # Constraints this crate honors
//!
//! The replay engine fixes the ground rules the runtime builds on:
//!
//! - **Budget checks are computed from replayed data** (recorded usage,
//!   recorded `now` observations), so a crossing re-fires identically at
//!   the same position on replay ([`Budgets`]).
//! - **Suspension is `suspend` + `await_resume`**, and budget crossings
//!   park through the same protocol ([`RunCtx::suspend`],
//!   [`RunCtx::await_resume`], [`RunCtx::budget_exceeded`]).
//! - **Divergence detection is always on**: every replayed step is compared
//!   structurally against the log, and a mismatch is a typed error, never a
//!   silent drift.
//! - **Random values are raw recorded bits** ([`RunCtx::random`] returns
//!   `u64`); richer values derive from the bits deterministically, which is
//!   how idempotency keys reproduce on replay.
//! - **Sequence numbers are 0-based and contiguous**, with completions
//!   correlated to their intent's position; the cursor enforces it and this
//!   crate persists exactly what the cursor emits, in order.
//!
//! # Recorded wire shapes this crate defines
//!
//! On top of the core event vocabulary, three shapes are contracts here:
//! the suspension sentinel and the structured tool-error object recorded in
//! tool-call completions (module docs of [`wire`], exported constants
//! [`SUSPEND_SENTINEL_KEY`] / [`ERROR_SENTINEL_KEY`]), and the
//! budget-extension resume input (module docs of [`budgets`]). Error
//! compaction, what a failed tool call puts into the model's context, is
//! specified and exported in [`compact`].
//!
//! [`wire`]: crate::wire
//! [`budgets`]: crate::Budgets
//! [`compact`]: crate::compact_error_message

#![warn(missing_docs)]

mod agent;
mod budgets;
mod compact;
mod ctx;
mod driver;
mod error;
mod hash;
mod labels;
mod model;
mod progress;
mod runtime;
mod validate;
mod wire;

pub use agent::{Agent, AgentBuildError, AgentBuilder, DEFAULT_MAX_RESPONSE_TOKENS};
pub use budgets::{
    BudgetExtensions, BudgetObservations, Budgets, Pricing, validate_extension_input,
};
pub use compact::{
    COMPACT_HEAD_CHARS, COMPACT_MESSAGE_CAP, COMPACT_TAIL_CHARS, FailureTracker,
    compact_error_message,
};
pub use ctx::{
    ClockFn, MAX_TOOL_ATTEMPTS, ModelTurn, RandomFn, Resumption, RunCtx, ToolCallResult,
};
pub use error::RuntimeError;
pub use hash::{canonical_json, hash_value, sha256_hex};
pub use labels::{MAX_LABEL_KEY_LEN, MAX_LABEL_VALUE_LEN, MAX_LABELS, validate_labels};
pub use model::{clamp_tokens, response_value, usage_of};
pub use progress::{event_detail, event_kind};
pub use runtime::{ParkReason, RunOutcome, Runtime};
pub use validate::validate_against_schema;
pub use wire::{
    ERROR_SENTINEL_KEY, SUSPEND_SENTINEL_KEY, ToolFailure, ToolFailureKind, content_string,
    decode_failure, decode_suspension, encode_failure, encode_suspension, error_chain,
};
