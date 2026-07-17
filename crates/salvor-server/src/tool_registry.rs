//! The tool-registry seam: the general injection point a host supplies so the
//! server can perform a tool call on a client-driven run's behalf.
//!
//! # Why it is a seam, not baked in
//!
//! This mirrors the [`ModelExecutor`](crate::ModelExecutor) decision exactly.
//! The server owns the *mechanism* of a durable tool step (append the
//! write-ahead intent, dispatch the tool, append the completion, answer retries
//! and reconciliation from the log), but it must not own the *policy* of which
//! tools exist or what they do. Salvor is for anyone building on it, browser or
//! backend; aarg is only the first consumer, and its typst render tool is one
//! registration, nothing salvor knows about. So the tools are registered by the
//! embedding binary and injected, never hard-wired.
//!
//! `salvor serve` wires an EMPTY registry by default: it holds no tools, so a
//! tool-step for any name is a clean `unknown_tool` `404` with no intent
//! written. Another host (a future `aarg serve`) registers its render tool
//! here, and nothing consumer-specific leaks into salvor-server. Tests inject
//! scripted tools with execution counters.
//!
//! # The operator-declared effect
//!
//! A registered tool carries its own [`Effect`](salvor_core::Effect) through
//! [`DynTool::effect`]. That is the operator's declaration, fixed at
//! registration. The tool-step endpoint takes the effect from the registration
//! and never from the client, so a caller cannot up- or down-grade a `Write`
//! into a freely retried `Read`. The recorded intent's effect is always the
//! registry's.
//!
//! # Reusing `DynTool`
//!
//! The registry stores `salvor_tools::DynTool` trait objects rather than a
//! server-local trait. salvor-server already depends on `salvor-tools` (its
//! agent factory builds agents out of the same tools), and `DynTool` is the
//! type-erased, `Value`-in/`Value`-out contract the runtime already dispatches
//! every tool through. The contract layer that defines `DynTool`, `ToolCtx`,
//! `ToolOutcome`, and `ToolError` builds without the crate's `mcp` feature, so
//! reusing it drags in no MCP stack. A registry tool and an agent tool are then
//! the same abstraction, and the server-performed tool step dispatches exactly
//! as `RunCtx::tool_call` does.

use std::collections::HashMap;
use std::sync::Arc;

use salvor_tools::DynTool;

/// A set of named tools the server may dispatch on a client-driven run's
/// behalf. Injected by the embedding binary, exactly like
/// [`ModelExecutor`](crate::ModelExecutor).
///
/// Each tool is keyed by its [`DynTool::name`]. A later registration under a
/// name already present replaces the earlier tool, so a host composing a
/// registry keeps the last word.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn DynTool>>,
}

impl ToolRegistry {
    /// An empty registry. This is the `salvor serve` default: every tool-step
    /// is an `unknown_tool` until a host registers a tool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Registers `tool` under its own [`DynTool::name`], replacing any tool
    /// already registered under that name.
    pub fn register(&mut self, tool: Arc<dyn DynTool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Registers `tool` and returns the registry, for the builder style a host
    /// composes with (`ToolRegistry::new().with_tool(a).with_tool(b)`).
    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn DynTool>) -> Self {
        self.register(tool);
        self
    }

    /// The tool registered under `name`, if any. `None` is the `unknown_tool`
    /// case the tool-step endpoint reports without writing an intent.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn DynTool>> {
        self.tools.get(name).cloned()
    }

    /// A borrowed view of the tool registered under `name`, if any. This is what
    /// a graph run's `tool` node resolves through: the engine's
    /// [`ToolResolver`](salvor_engine::ToolResolver) hands the engine a
    /// `&dyn DynTool`, and this registry is the server's whole tool inventory —
    /// the SAME seam a client-driven tool step dispatches through. `salvor serve`
    /// ships it empty, so a graph `tool` node is a precise `unknown_tool` until a
    /// host registers the tool it names.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&dyn DynTool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// Whether the registry holds no tools (the `salvor serve` default).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// How many tools are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }
}
