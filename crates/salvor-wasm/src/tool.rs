//! [`WasmTool`]: one sandboxed component exposed as a
//! [`DynTool`](salvor_tools::DynTool), dispatched by the runtime exactly like
//! a native or MCP tool.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use salvor_core::Effect;
use salvor_tools::{DynTool, HandlerError, ToolCtx, ToolError, ToolOutcome};
use serde_json::Value;

use crate::engine::{CallFailure, WasmEngine};
use crate::error::WasmError;
use crate::grants::DirGrant;
use crate::limits::ToolLimits;

/// Everything the operator declares about one sandboxed tool.
///
/// Every field is an operator statement, none is negotiated with the guest:
/// the model-facing metadata (name, description, schema) because a hostile
/// binary's self-description is a prompt-injection surface, the effect
/// because retry and resume semantics are a trust decision the sandbox
/// cannot be allowed to make about itself, and the limits and grants because
/// they *are* the sandbox.
#[derive(Clone, Debug)]
pub struct WasmToolSpec {
    /// The name the model calls the tool by.
    pub name: String,
    /// The model-facing description, operator-authored.
    pub description: String,
    /// The side-effect class governing retry and resume behavior. Required
    /// in configuration with no default; the guest has no channel to
    /// influence it.
    pub effect: Effect,
    /// The JSON Schema the model is told the input must satisfy. Advisory to
    /// the model only: the host does not validate calls against it, and the
    /// guest must treat its input as untrusted regardless.
    pub input_schema: Value,
    /// Per-call resource caps.
    pub limits: ToolLimits,
    /// Directory preopens, the only capability grant. Empty means the guest
    /// can open nothing.
    pub grants: Vec<DirGrant>,
}

/// A sandboxed WebAssembly component tool.
///
/// Like [`McpTool`](https://docs.rs/salvor-tools), this implements
/// [`DynTool`] directly rather than through `TypedTool`, because there is no
/// compile-time `Input` type: the boundary is JSON by contract. Unlike MCP,
/// *nothing* about it is learned from the other side of the boundary.
///
/// # Input validation is the guest's job
///
/// `call_json` performs no schema validation: the operator-declared schema
/// steers the model, and the guest, which by premise trusts nothing, decides
/// what its input means. A guest rejecting its input returns the WIT
/// `result`'s `err` side, which surfaces as
/// [`ToolError::Handler`], not [`ToolError::InvalidInput`].
///
/// # Never suspends
///
/// The `salvor:tool@0.1.0` world has no suspension channel, so `call_json`
/// never returns [`ToolOutcome::Suspend`]; a wasm tool resolves to an output
/// or an error. Human-in-the-loop stays on native tools.
///
/// # Error mapping
///
/// - Guest `err(string)`: [`ToolError::Handler`] with the guest's message.
/// - Host-enforced limit trap (wall time, memory, fuel):
///   [`ToolError::Handler`] whose source chain carries a typed
///   [`LimitExceeded`](crate::LimitExceeded); see that type for why limit
///   exhaustion is deterministic, what that means against
///   [`RetryPolicy`](salvor_tools::RetryPolicy), and why v0.2 keeps it under
///   `Handler` anyway.
/// - Guest output that is not valid JSON:
///   [`ToolError::OutputSerialization`], the contract-violation bucket.
/// - Guest crash or setup failure: [`ToolError::Handler`] with the
///   attribution message.
pub struct WasmTool {
    engine: Arc<WasmEngine>,
    component: wasmtime::component::Component,
    spec: WasmToolSpec,
}

impl WasmTool {
    /// Loads a component from `path` and binds it to an operator spec.
    ///
    /// When `expected_sha256` is set, the file's bytes must hash to it or
    /// loading is refused before compilation; see
    /// [`WasmEngine::load_component`].
    ///
    /// # Errors
    ///
    /// Any [`WasmError`] from reading, pin-checking, or compiling the file.
    pub fn load(
        engine: Arc<WasmEngine>,
        path: &Path,
        expected_sha256: Option<&str>,
        spec: WasmToolSpec,
    ) -> Result<Self, WasmError> {
        let component = engine.load_component(path, expected_sha256)?;
        Ok(Self {
            engine,
            component,
            spec,
        })
    }
}

#[async_trait]
impl DynTool for WasmTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn effect(&self) -> Effect {
        self.spec.effect
    }

    fn input_schema(&self) -> Value {
        self.spec.input_schema.clone()
    }

    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        // `_ctx` carries the attempt's idempotency key; the WIT world has no
        // parameter to forward it to (same posture as MCP: the key still
        // governs the loop's retry decision from the event log).
        let tool = self.spec.name.clone();

        // A `Value` always serializes: object keys are strings by
        // construction. The map_err is defensive completeness, not a path
        // tests can reach.
        let input_json = serde_json::to_string(&input).map_err(|source| ToolError::Handler {
            tool: tool.clone(),
            source: HandlerError::new(source),
        })?;

        // The wasmtime call is synchronous and CPU-bound, so it runs on the
        // blocking pool; the epoch deadline bounds how long it can occupy a
        // blocking thread. Everything it needs is cheaply cloned in:
        // `Component` is internally reference-counted.
        let engine = Arc::clone(&self.engine);
        let component = self.component.clone();
        let limits = self.spec.limits;
        let grants = self.spec.grants.clone();
        let verdict = tokio::task::spawn_blocking(move || {
            engine.execute(&component, limits, &grants, &input_json)
        })
        .await
        .map_err(|join_error| ToolError::Handler {
            tool: tool.clone(),
            source: HandlerError::message(format!("sandbox task failed to run: {join_error}")),
        })?;

        match verdict {
            // The guest answered. Its output must be JSON by contract; a
            // string that does not parse is a contract violation by the tool
            // (not the model's doing, not retryable), which is exactly the
            // OutputSerialization bucket.
            Ok(Ok(output_json)) => {
                let value: Value = serde_json::from_str(&output_json).map_err(|source| {
                    ToolError::OutputSerialization {
                        tool: tool.clone(),
                        source,
                    }
                })?;
                Ok(ToolOutcome::Output(value))
            }
            // The guest ran and said no: its own error message, verbatim.
            Ok(Err(guest_error)) => Err(ToolError::Handler {
                tool,
                source: HandlerError::message(guest_error),
            }),
            // The host killed the call at a cap. The typed LimitExceeded
            // rides the source chain so callers can downcast and route.
            Err(CallFailure::Limit(limit)) => Err(ToolError::Handler {
                tool,
                source: HandlerError::new(limit),
            }),
            Err(CallFailure::Setup(message)) | Err(CallFailure::Trap(message)) => {
                Err(ToolError::Handler {
                    tool,
                    source: HandlerError::message(message),
                })
            }
        }
    }
}
