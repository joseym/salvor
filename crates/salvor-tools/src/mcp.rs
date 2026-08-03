//! Model Context Protocol (MCP) integration: connect to an MCP server over
//! stdio (a spawned child process) or streamable HTTP (a remote server by URL),
//! and surface each tool it reports as a [`DynTool`](crate::DynTool) the runtime
//! dispatches through like any native tool.
//!
//! This whole module sits behind the `mcp` cargo feature. Everything MCP lives
//! here and nowhere else in the workspace: the rmcp SDK, the Tokio runtime it
//! needs, and the mapping from MCP's wire types to this crate's tool contract.
//! That isolation is deliberate. rmcp/MCP protocol churn is a
//! standing risk, and the mitigation is exactly this: one module, one
//! feature, one pinned dependency, so a protocol shift touches one file set and
//! the executor-agnostic contract layer never learns MCP exists.
//!
//! # Layout
//!
//! - [`McpServer`] (in the `server` submodule) owns one server connection over
//!   either transport: [`connect`](McpServer::connect) spawns a child process
//!   and speaks stdio, [`connect_http`](McpServer::connect_http) reaches a
//!   remote server by URL over streamable HTTP. Either way it initializes the
//!   MCP session, lists the tools, and shuts the session down cleanly on close
//!   or drop. A stdio server is a real child process of this one, and how it is
//!   held (its own process group, kill on drop, and a parent-death signal where
//!   the platform has one) is stated in that submodule's docs, along with the
//!   one case that can still outlive an operator's `kill -9`.
//! - [`McpTool`] (in the `tool` submodule) is one MCP tool, implementing
//!   [`DynTool`](crate::DynTool) directly. Its name, description, and JSON
//!   schema are the server's own; its [`Effect`] is decided by the mapping
//!   below.
//! - [`EffectOverrides`] and [`effect_for`] decide a tool's [`Effect`] from the
//!   server's annotation hints, subject to per-tool operator overrides.
//! - [`IdempotencyKeys`] carries the operator's per-tool declaration of which
//!   input field identifies a call, which is what lets a server's tool
//!   participate in cross-run deduplication. See
//!   [`IdempotencyPath`](crate::IdempotencyPath) for the key format and the
//!   refusal rule.
//!
//! # Effect mapping: hints are not guarantees
//!
//! The MCP specification is explicit that a tool's annotations are *hints* and
//! that a server may lie about them; a client must not make trust decisions on
//! annotations from an untrusted server. So the mapping is conservative:
//!
//! - `readOnlyHint == true` maps to [`Effect::Read`].
//! - otherwise `idempotentHint == true` maps to [`Effect::Idempotent`].
//! - otherwise [`Effect::Write`], the safe default: an unknown tool is presumed
//!   to have side effects, because presuming otherwise is the dangerous guess.
//!
//! Because the mapping is only as trustworthy as the server, [`EffectOverrides`]
//! lets the operator pin an [`Effect`] per tool name at connection time. An
//! override wins over whatever the server annotated. That is the operator's
//! trust decision to make: they are asserting "I know this server's `delete`
//! tool is a write regardless of what it claims," and the runtime honors it.
//!
//! # Suspension does not apply
//!
//! MCP has no concept of suspending a call to await a human. An [`McpTool`]
//! therefore never returns [`ToolOutcome::Suspend`](crate::ToolOutcome::Suspend);
//! its [`call_json`](crate::DynTool::call_json) resolves to an output or an
//! error, never a parked run. Human-in-the-loop lives on native tools.
//!
//! # Client-side input validation is structural only
//!
//! A native [`TypedTool`](crate::TypedTool) validates the model's JSON against
//! a typed `Input` before running. An MCP tool has no typed `Input` on this
//! side of the wire, only the server's declared JSON Schema. This module does
//! *not* embed a JSON Schema validator, so it does not check arguments against
//! that schema. What it does check, locally and before any network hop, is that
//! the arguments are structurally an MCP argument object (a JSON object, or
//! absent): anything else is [`ToolError::InvalidInput`](crate::ToolError::InvalidInput)
//! with no round trip. Semantic validation against the schema is the server's
//! job, and a server-reported failure comes back as
//! [`ToolError::Handler`](crate::ToolError::Handler). See [`McpTool`] for the
//! exact contract.

mod server;
mod tool;

use std::collections::BTreeMap;

use rmcp::model::ToolAnnotations;
use salvor_core::Effect;

use crate::idempotency::IdempotencyPath;

pub use server::{McpError, McpServer};
pub use tool::McpTool;

/// Per-tool [`Effect`] overrides supplied by the operator at connection time.
///
/// An entry pins the [`Effect`] for one tool name, overriding whatever the
/// server annotated (or failed to annotate). This is the operator's trust
/// decision: MCP annotations are hints a server may misstate, so an operator
/// who knows a tool's true side-effect class states it here and the runtime
/// honors it over the wire hints. A tool with no override falls back to the
/// annotation mapping described on the [module docs](crate::mcp).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectOverrides {
    by_name: BTreeMap<String, Effect>,
}

impl EffectOverrides {
    /// An empty override set. Every tool falls back to the annotation mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins `effect` for the tool named `name`, returning `self` so overrides
    /// chain: `EffectOverrides::new().with("delete", Effect::Write)`.
    pub fn with(mut self, name: impl Into<String>, effect: Effect) -> Self {
        self.by_name.insert(name.into(), effect);
        self
    }

    /// Inserts an override for `name`, replacing any previous one.
    pub fn insert(&mut self, name: impl Into<String>, effect: Effect) {
        self.by_name.insert(name.into(), effect);
    }

    /// The override for `name`, if one was set.
    pub fn get(&self, name: &str) -> Option<Effect> {
        self.by_name.get(name).copied()
    }

    /// Whether no overrides are set.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Per-tool idempotency key declarations supplied by the operator at connection
/// time.
///
/// An entry names, for one tool, the input field whose value identifies the
/// operation the call performs. A `pay_claim` entry of `"claim_id"` says that a
/// call carrying `{"claim_id": "wreck-9931"}` *is* the payout for that claim,
/// whichever run asks for it, which is the statement the store needs before it
/// can let exactly one run execute it (see
/// [`RunCtx::tool_call`](../../salvor_runtime/struct.RunCtx.html#method.tool_call)).
///
/// The declaration is the operator's, not the server's, for the same reason
/// [`EffectOverrides`] is: a server's own account of its tools is a hint, and
/// nothing on the wire says which field of a call names the money.
///
/// A tool with no entry is untouched: it declares no key, exactly as before.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdempotencyKeys {
    by_name: BTreeMap<String, IdempotencyPath>,
}

impl IdempotencyKeys {
    /// An empty declaration set. Every tool is keyless.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares `path` as the identifying field for the tool named `name`,
    /// returning `self` so declarations chain.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, path: IdempotencyPath) -> Self {
        self.by_name.insert(name.into(), path);
        self
    }

    /// Inserts a declaration for `name`, replacing any previous one.
    pub fn insert(&mut self, name: impl Into<String>, path: IdempotencyPath) {
        self.by_name.insert(name.into(), path);
    }

    /// The declared path for `name`, if there is one.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&IdempotencyPath> {
        self.by_name.get(name)
    }

    /// Every declared tool name, in sorted order. The caller checks these
    /// against the tools a server actually advertises.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Decides a tool's [`Effect`] from its name, its server-reported annotations,
/// and the operator's overrides.
///
/// The rule, in precedence order:
///
/// 1. An [`EffectOverrides`] entry for this name wins outright.
/// 2. Else `readOnlyHint == true` gives [`Effect::Read`].
/// 3. Else `idempotentHint == true` gives [`Effect::Idempotent`].
/// 4. Else [`Effect::Write`], the safe default for an unannotated or otherwise
///    unclassified tool.
///
/// Only `Some(true)` counts for a hint; a missing hint or an explicit
/// `Some(false)` is not treated as a promise of read-only or idempotent
/// behavior, which keeps the fall-through on the safe side.
pub fn effect_for(
    name: &str,
    annotations: Option<&ToolAnnotations>,
    overrides: &EffectOverrides,
) -> Effect {
    if let Some(effect) = overrides.get(name) {
        return effect;
    }
    match annotations {
        Some(a) if a.read_only_hint == Some(true) => Effect::Read,
        Some(a) if a.idempotent_hint == Some(true) => Effect::Idempotent,
        _ => Effect::Write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `ToolAnnotations` is `#[non_exhaustive]`, so it is built through its
    // setters, not a struct literal. Each setter records `Some(value)`; a `None`
    // argument leaves the hint unset, which is what the mapping treats as "no
    // promise."
    fn annotations(read_only: Option<bool>, idempotent: Option<bool>) -> ToolAnnotations {
        let mut ann = ToolAnnotations::new();
        if let Some(v) = read_only {
            ann = ann.read_only(v);
        }
        if let Some(v) = idempotent {
            ann = ann.idempotent(v);
        }
        ann
    }

    #[test]
    fn read_only_hint_maps_to_read() {
        let ann = annotations(Some(true), None);
        let effect = effect_for("fetch", Some(&ann), &EffectOverrides::new());
        assert_eq!(effect, Effect::Read);
    }

    #[test]
    fn idempotent_hint_maps_to_idempotent_when_not_read_only() {
        let ann = annotations(Some(false), Some(true));
        let effect = effect_for("upsert", Some(&ann), &EffectOverrides::new());
        assert_eq!(effect, Effect::Idempotent);
    }

    #[test]
    fn unannotated_defaults_to_write() {
        assert_eq!(
            effect_for("mutate", None, &EffectOverrides::new()),
            Effect::Write
        );
        // An annotations object present but silent on both hints still defaults
        // to Write: silence is not a promise.
        let ann = annotations(None, None);
        assert_eq!(
            effect_for("mutate", Some(&ann), &EffectOverrides::new()),
            Effect::Write
        );
    }

    #[test]
    fn read_only_wins_over_idempotent() {
        // Both hints true: read-only is checked first, so Read wins.
        let ann = annotations(Some(true), Some(true));
        assert_eq!(
            effect_for("t", Some(&ann), &EffectOverrides::new()),
            Effect::Read
        );
    }

    #[test]
    fn override_beats_annotation() {
        // The server annotates read-only, but the operator says Write.
        let ann = annotations(Some(true), None);
        let overrides = EffectOverrides::new().with("delete", Effect::Write);
        assert_eq!(effect_for("delete", Some(&ann), &overrides), Effect::Write);
    }

    #[test]
    fn override_applies_even_with_no_annotations() {
        let overrides = EffectOverrides::new().with("fetch", Effect::Read);
        assert_eq!(effect_for("fetch", None, &overrides), Effect::Read);
    }
}
