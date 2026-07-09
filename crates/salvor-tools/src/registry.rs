//! [`ToolSet`]: the name-keyed registry of tools the runtime dispatches
//! through, plus [`RegistryError`] for a duplicate registration.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::erased::{DynTool, ToolDescriptor, TypedTool};
use crate::handler::ToolHandler;

/// A collection of tools keyed by name.
///
/// This is what an agent is configured with and what the runtime loop
/// dispatches through: the model names a tool, the loop looks it up here and
/// calls it.
/// Tools are stored type-erased as `Box<dyn DynTool>`, so native
/// [`ToolHandler`]s and (later) MCP-backed tools live side by side.
///
/// Names are unique. Registering a second tool under a name already present is
/// a [`RegistryError`], not a silent overwrite: a shadowed tool is a
/// configuration bug the caller should hear about, since the model would be
/// told about one tool and dispatched to another.
///
/// The backing map is a `BTreeMap`, so [`descriptors`](Self::descriptors) and
/// [`tools`](Self::tools) enumerate in a stable, name-sorted order. A
/// deterministic tool list is worth having for a runtime built on
/// deterministic replay: the order the model is shown its tools does not wobble
/// between runs.
#[derive(Default)]
pub struct ToolSet {
    tools: BTreeMap<String, Box<dyn DynTool>>,
}

impl ToolSet {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a typed [`ToolHandler`], wrapping it as a
    /// [`DynTool`](crate::DynTool) automatically.
    ///
    /// Returns [`RegistryError::DuplicateName`] if a tool is already registered
    /// under this handler's [`NAME`](crate::ToolMeta::NAME); the existing tool
    /// is left in place.
    pub fn register<H: ToolHandler + 'static>(&mut self, handler: H) -> Result<(), RegistryError> {
        self.register_dyn(Box::new(TypedTool::new(handler)))
    }

    /// Registers an already type-erased tool.
    ///
    /// This is the path a runtime-defined tool (such as an MCP-backed one)
    /// takes, since it implements [`DynTool`](crate::DynTool) directly rather
    /// than through [`TypedTool`](crate::TypedTool). Same duplicate-name rule
    /// as [`register`](Self::register).
    pub fn register_dyn(&mut self, tool: Box<dyn DynTool>) -> Result<(), RegistryError> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(RegistryError::DuplicateName { name });
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Looks up a tool by the name the model called it by.
    pub fn get(&self, name: &str) -> Option<&dyn DynTool> {
        self.tools.get(name).map(Box::as_ref)
    }

    /// How many tools are registered.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry holds no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Every registered tool, name-sorted, as `dyn DynTool`.
    pub fn tools(&self) -> impl Iterator<Item = &dyn DynTool> {
        self.tools.values().map(Box::as_ref)
    }

    /// A [`ToolDescriptor`] for every registered tool, name-sorted.
    ///
    /// This is the list handed to a model: each tool's name, description,
    /// effect, and input schema.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools().map(DynTool::descriptor).collect()
    }
}

/// The error [`ToolSet`] registration returns.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    /// A tool is already registered under this name.
    #[error("a tool named `{name}` is already registered")]
    DuplicateName {
        /// The name that collided.
        name: String,
    },
}
