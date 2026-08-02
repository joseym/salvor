//! The graph document format: the `Graph` envelope, the six node kinds, the
//! edges that connect them, and the small payload types a node carries.
//!
//! Everything here is pure data. No type reads the clock, draws randomness, or
//! performs IO. That purity is deliberate: this crate is a leaf that the CLI,
//! the control plane, and (later) a wasm dashboard projection all parse graph
//! documents with, so it must drag in no runtime and no host dependency.
//!
//! # Two postures, one format
//!
//! A graph is a CONTROL document, not a data payload. A silently dropped field
//! could drop a gate or an unenforced budget, so parsing is STRICT: every
//! struct and the node enum carry `#[serde(deny_unknown_fields)]`, and a stray
//! key is rejected rather than ignored. That is the opposite posture from the
//! event log, which stays forward-tolerant because it carries recorded data.
//!
//! The one concession the two share is the additive `schema_version`
//! discipline (see `SCHEMA_VERSION`): a graph that was recorded under an
//! older build must still parse and validate under a newer one. Strict in,
//! additive-tolerant out.
//!
//! # The optional node display name, and why it hashes unlike an agent's
//!
//! Every node payload carries an optional `name`: a short, purely
//! presentational label ("Approve the draft") an author can hang on a node so
//! a rendered graph reads by intent instead of by id. Bounds mirror the
//! precedent set by the agent definition's own `name`
//! (`salvor_cli::agent_config::MAX_NAME_LEN`): at most 64 CHARACTERS
//! (`chars().count()`, not bytes), and, when set, not empty or all
//! whitespace. `crate::validate` enforces both, node-precise.
//!
//! An agent's `name` is deliberately excluded from its `agent_def_hash`: an
//! agent is a long-lived identity that a run keeps replaying under the same
//! hash while an operator relabels it, so a rename must not mint a new
//! identity (see `salvor_runtime::Agent::def_hash`). A graph document has no
//! such identity to protect: it IS its hash, the whole reason `POST
//! /v1/graphs` stores it content-addressed. So a node's `name` gets NO
//! special treatment: it is an ordinary field on the payload struct, present
//! on the wire exactly when set (`skip_serializing_if = "Option::is_none"`),
//! and folds into the canonical JSON `salvor_engine::graph_hash` hashes like
//! any other field. Renaming a node is therefore authoring a new document
//! version, by design, the same way changing a `prompt` or an `over`
//! reference is.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The schema version stamped onto every graph document.
///
/// Present from the first document ever written, so an old document is always
/// self-describing and a future reader can branch on it. Start at 1.
///
/// # Why adding a node kind or an optional field does not bump this
///
/// This mirrors the reasoning in `salvor-replay`'s event `SCHEMA_VERSION`.
/// `schema_version` exists so a reader knows how to interpret documents that
/// were already recorded. Adding a variant to `Node` changes nothing about
/// how any previously written document is encoded: a document written before
/// the addition contains none of the new kinds, and every node in it parses to
/// the identical value under the new build. An additive optional field follows
/// the same rule when it carries `#[serde(default, skip_serializing_if =
/// "...")]`: with the field absent the wire form is byte for byte what it was
/// before the field existed, and an old document deserializes with the field
/// defaulted.
///
/// A bump is reserved for a change that alters the meaning or shape of a node
/// or edge a version-1 writer may already have produced: renaming a field,
/// changing the node envelope, or re-encoding a payload.
///
/// # The strict-in direction
///
/// Because a graph is submitted, not just replayed, `crate::validate` also
/// rejects a document whose `schema_version` is FROM THE FUTURE (greater than
/// this constant): a current build cannot promise to understand a shape a newer
/// writer invented. An older-or-equal version is accepted, which is the
/// additive-tolerant-out promise recorded graphs rely on.
pub const SCHEMA_VERSION: u32 = 1;

/// A graph document: the control document authored once, submitted, hashed into
/// a run, and then frozen. It carries the schema version, the set of nodes
/// (each with a stable string id), and the edges that connect them.
///
/// Serializing a `Graph` always includes `schema_version`, so the wire form is
/// self-describing. Unknown top-level keys are rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    /// The document schema version. Always `SCHEMA_VERSION` for documents
    /// this build writes; an older value may appear when reading a recorded
    /// document.
    pub schema_version: u32,
    /// The nodes, each identified by a stable string id unique within the
    /// document.
    pub nodes: Vec<Node>,
    /// The directed edges connecting nodes. A document with a single node and
    /// no edges is legal, so this defaults to empty.
    #[serde(default)]
    pub edges: Vec<Edge>,
}

/// One node in a graph: exactly one of the six kinds the runtime knows how to
/// execute.
///
/// Adjacently tagged like the event enum: each node serializes as `{"kind":
/// "...", "payload": {...}}`. The tag (`kind`) and content (`payload`) live in
/// separate keys, which never collides with a payload field and does not force
/// payloads to be JSON objects. `deny_unknown_fields` on the enum rejects any
/// key other than `kind` and `payload`.
///
/// The stable node id lives inside each payload (every payload struct carries
/// an `id`), reachable generically through `Node::id`. Keeping the id in the
/// payload is what lets the outer shape stay exactly the two-key adjacent
/// tagging the event log uses, with no third common field to special-case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Node {
    /// A full agent loop (model, prompt, tools, budget). The whole v0.1 product
    /// becomes one node kind. It references its agent definition BY CONTENT
    /// HASH, never an embedded definition, so this crate stays a leaf that does
    /// not depend on the agent-definition schema.
    Agent(AgentNode),
    /// A single direct tool invocation, no model in the loop.
    Tool(ToolNode),
    /// Human approval: suspends the graph run and renders in the approval
    /// inbox.
    Gate(GateNode),
    /// Routes on a typed output. The cases (conditions and their names) are
    /// recorded as DATA here; this crate never evaluates them.
    Branch(BranchNode),
    /// Fan-out: spawn a sub-run per element of a typed list, join on
    /// completion, with a concurrency cap.
    Map(MapNode),
    /// Bounded iteration: run a body repeatedly, accumulating across passes,
    /// until a stop predicate holds or an iteration bound is reached, then join
    /// the passes into one value. Models an adversarial refine loop (draft,
    /// score, review, revise) as one node. Execution is not implemented: the
    /// fold exists in the format, the validator, the projection, and the
    /// canvas; the engine records a typed refusal for it.
    Fold(FoldNode),
}

impl Node {
    /// The node's stable id, whatever its kind.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Node::Agent(n) => &n.id,
            Node::Tool(n) => &n.id,
            Node::Gate(n) => &n.id,
            Node::Branch(n) => &n.id,
            Node::Map(n) => &n.id,
            Node::Fold(n) => &n.id,
        }
    }

    /// The kind name (`"agent"`, `"tool"`, ...), for error messages.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Node::Agent(_) => "agent",
            Node::Tool(_) => "tool",
            Node::Gate(_) => "gate",
            Node::Branch(_) => "branch",
            Node::Map(_) => "map",
            Node::Fold(_) => "fold",
        }
    }

    /// The node's optional display name, whatever its kind. See the module
    /// docs' "The optional node display name" section.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Node::Agent(n) => n.name.as_deref(),
            Node::Tool(n) => n.name.as_deref(),
            Node::Gate(n) => n.name.as_deref(),
            Node::Branch(n) => n.name.as_deref(),
            Node::Map(n) => n.name.as_deref(),
            Node::Fold(n) => n.name.as_deref(),
        }
    }

    /// The JSON Schema this node declares for the payload it CONSUMES, if any.
    /// Absent means the node does not declare an input type, and an edge into
    /// it passes the type-compatibility check unchecked.
    #[must_use]
    pub fn input_schema(&self) -> Option<&Value> {
        match self {
            Node::Agent(n) => n.input_schema.as_ref(),
            Node::Tool(n) => n.input_schema.as_ref(),
            // Gate, branch, map, and fold do not declare a consumed type;
            // they pass typed payloads through untyped. A fold's
            // `accumulator_schema` is data only, deliberately not wired into
            // the edge type-compatibility check while its execution is not
            // implemented.
            Node::Gate(_) | Node::Branch(_) | Node::Map(_) | Node::Fold(_) => None,
        }
    }

    /// The JSON Schema this node declares for the payload it PRODUCES, if any.
    /// Absent means the node does not declare an output type, and an edge out
    /// of it passes the type-compatibility check unchecked.
    #[must_use]
    pub fn output_schema(&self) -> Option<&Value> {
        match self {
            Node::Agent(n) => n.output_schema.as_ref(),
            Node::Tool(n) => n.output_schema.as_ref(),
            Node::Map(n) => n.output_schema.as_ref(),
            // A fold's produced-value type is not implemented with its
            // execution: its `accumulator_schema` is data only and does not
            // gate outbound edges.
            Node::Gate(_) | Node::Branch(_) | Node::Fold(_) => None,
        }
    }
}

/// An `agent` node: a full agent loop referenced by content hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// Content hash of the agent definition (model, prompt, tools, budget) this
    /// node runs, in `sha256:<64 lowercase hex>` form. A hash, never an
    /// embedded definition: that keeps this crate independent of the
    /// agent-definition schema and lets the same definition be shared across
    /// nodes and runs by identity.
    pub agent_hash: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional JSON Schema for the payload this node consumes. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Optional JSON Schema for the payload this node produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// A `tool` node: one direct tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// The tool's name, as registered with the runtime.
    pub tool: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The input mapping: tool input field name to an opaque source reference.
    /// Recorded as DATA; this crate does not resolve or evaluate the references.
    /// Additive: omitted on the wire when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<String, String>,
    /// Optional JSON Schema for the payload this node consumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Optional JSON Schema for the payload this node produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// A `gate` node: human approval that suspends the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GateNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional human-readable prompt shown in the approval inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// JSON Schema the human approval input must satisfy, mirroring the recorded
    /// `Suspended` event's `input_schema`. Required: a gate with no declared
    /// approval shape is meaningless.
    pub approval_schema: Value,
}

/// A `branch` node: routes on a typed output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional opaque reference to the typed value the branch routes on.
    /// Recorded as DATA; not resolved in this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Content hash of the agent that decides a `BranchCondition::ModelDecision`
    /// case, in `sha256:<64 lowercase hex>` form. Present only on a branch that
    /// carries a model-decision case: the engine drives this agent with the
    /// routed value and maps its reply to a case name. Additive: absent on the
    /// wire when unset, so a purely expression-driven branch (and every document
    /// written before this field existed) serializes byte for byte as before.
    /// `crate::validate` reports a model-decision case with no agent here as a
    /// node-precise error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_hash: Option<String>,
    /// The cases, each a named condition. An expression condition is evaluated
    /// against the routed value; a model-decision condition is resolved by the
    /// node's `agent_hash` agent. The first matching case in
    /// author order wins, and the engine records the choice as a
    /// `crate::document`-external `BranchTaken` event.
    pub cases: Vec<BranchCase>,
}

/// One case of a `BranchNode`: a name and the condition that selects it.
///
/// The realized routing (which downstream node a fired case flows to) is
/// carried by an `Edge` whose `label` matches the case `name`, so topology
/// stays entirely in the edge list and a branch has real outbound edges.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BranchCase {
    /// The case name. An edge labeled with this name realizes the route.
    pub name: String,
    /// The condition that selects this case. Data only, never evaluated here.
    pub when: BranchCondition,
}

/// How a `BranchCase` is selected. Modeled as data; not evaluated in this
/// crate.
///
/// Adjacently tagged (`{"kind": "...", "value": ...}`) so it stays additive:
/// a future condition kind is a new variant, which does not change how an
/// existing document encodes.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BranchCondition {
    /// A constrained boolean expression over the routed value, recorded as an
    /// opaque string. NOT parsed or evaluated in this crate.
    Expression(String),
    /// The case is chosen by a model decision at run time, recorded as an event.
    /// Carries no author-time data.
    ModelDecision,
}

/// The wire shape `BranchCondition` parses as: the exact same
/// tag/content/deny_unknown_fields attributes as the type it mirrors, so
/// what this accepts and rejects is unchanged. It exists only as a target for
/// `BranchCondition`'s hand-written `Deserialize` impl below, so a
/// malformed `when` can be reported in product language instead of serde's
/// "adjacently tagged enum" internals.
#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum BranchConditionShape {
    Expression(String),
    ModelDecision,
}

impl From<BranchConditionShape> for BranchCondition {
    fn from(shape: BranchConditionShape) -> Self {
        match shape {
            BranchConditionShape::Expression(expr) => BranchCondition::Expression(expr),
            BranchConditionShape::ModelDecision => BranchCondition::ModelDecision,
        }
    }
}

impl<'de> Deserialize<'de> for BranchCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Buffer as generic JSON first, then parse that buffer through the
        // identical adjacently tagged shape the derive would have used.
        // Acceptance does not change: a value `BranchConditionShape`
        // rejects was always rejected. Only the failure message changes, from
        // serde's enum-internals wording to a product-language description of
        // the two accepted shapes with the offending value echoed back.
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value::<BranchConditionShape>(value.clone())
            .map(Into::into)
            .map_err(|_| D::Error::custom(describe_branch_condition_error(&value)))
    }
}

/// Build the error text for a `when` that failed to parse as a
/// `BranchCondition`: the two accepted shapes, then what was actually
/// found, so a bare string (the obvious skim-and-adapt mistake, writing
/// `"when": "value > 10000"` instead of the object form) reads as data
/// against the expected shape rather than a serde internals error.
fn describe_branch_condition_error(value: &Value) -> String {
    format!(
        "a branch condition must be an object shaped \
         `{{\"kind\": \"expression\", \"value\": \"<expr>\"}}` or \
         `{{\"kind\": \"model_decision\"}}`; got {}",
        describe_json_value(value)
    )
}

/// Describe a JSON value's shape and content for an error message: a bare
/// string echoes as `a bare string "..."`, an object as `an object {...}`,
/// and so on.
fn describe_json_value(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "<unrepresentable>".to_string());
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => format!("a bare boolean {text}"),
        Value::Number(_) => format!("a bare number {text}"),
        Value::String(_) => format!("a bare string {text}"),
        Value::Array(_) => format!("a bare array {text}"),
        Value::Object(_) => format!("an object {text}"),
    }
}

/// A `map` node: fan-out a sub-run per element of a typed list, with a
/// concurrency cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opaque reference to the typed list this node fans out over. Data only,
    /// not resolved in this crate.
    pub over: String,
    /// The maximum number of sub-runs in flight at once. Must be at least 1;
    /// `crate::validate` reports a non-positive cap by node id.
    pub concurrency: u32,
    /// What each element is mapped through: a node already in this document, or
    /// an embedded sub-graph.
    pub body: MapBody,
    /// Optional JSON Schema for the joined list this node produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// The body a `MapNode` maps each element through.
///
/// Adjacently tagged, so adding a third form later is additive. A `node` body
/// names an existing node by id (checked for existence during validation); a
/// `subgraph` body embeds a whole `Graph`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MapBody {
    /// Map each element through an existing node in this document, by id.
    Node(String),
    /// Map each element through an embedded sub-graph. Boxed because a `Graph`
    /// contains nodes, one of which may be a `map`, so the type is recursive.
    Subgraph(Box<Graph>),
}

/// A `fold` node: bounded iteration that accumulates across passes.
///
/// Models an adversarial refine loop as one node: a `body` is run up to
/// `max_iterations` times, each pass folding into an accumulated value, and the
/// loop stops when `stop_when` holds over that value (or the bound is reached).
/// The `join` rule then selects the value the node produces. Every field is
/// author-time data; this crate never runs the loop.
///
/// Grounded in the AARG tailor loop the graph wiring models: bounded revisions
/// (`max_iterations`), a stop predicate over the accumulated score
/// (`stop_when`, an expression in the same language a branch case uses), and an
/// argmax winner (`join` = `FoldJoin::BestBy` over the score). See the
/// crate-level docs and the graph wiring plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FoldNode {
    /// The node's stable id, unique within the document.
    pub id: String,
    /// Optional short display label for this node. See the module docs' "The
    /// optional node display name" section for the bound and the deliberate
    /// hash-inclusion contrast with the agent `name` field. Additive: absent
    /// on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What each pass runs: a node already in this document, or an embedded
    /// sub-graph. Not implemented exactly as `MapBody`'s subgraph form is not:
    /// the shape is legal, but no engine runs it yet.
    pub body: FoldBody,
    /// The iteration bound: the most passes the loop may run. Must be at least
    /// 1; `crate::validate` reports a zero bound by node id.
    pub max_iterations: u32,
    /// A boolean expression over the accumulated value that stops the loop when
    /// it holds. Written in the `crate::expr` condition language, the same one
    /// a `BranchCondition::Expression` uses, and validated at submit so a
    /// malformed predicate is a node-precise error, never a run-time failure.
    pub stop_when: String,
    /// How the passes are folded into the value the node produces.
    pub join: FoldJoin,
    /// Optional JSON Schema for the accumulated value the loop carries and
    /// produces. Data only, like an `AgentNode`'s `output_schema`: recorded
    /// for authoring and tooling, never wired into the edge type-compatibility
    /// check (a fold's produced-value semantics are not implemented with
    /// its execution). Additive: absent on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accumulator_schema: Option<Value>,
}

/// The body a `FoldNode` runs each pass. Adjacently tagged, mirroring
/// `MapBody`, so adding a third form later stays additive. A `node` body names
/// an existing node by id (checked for existence during validation); a
/// `subgraph` body embeds a whole `Graph` and is deferred exactly as the map's
/// subgraph body is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FoldBody {
    /// Run each pass through an existing node in this document, by id.
    Node(String),
    /// Run each pass through an embedded sub-graph. Boxed because a `Graph`
    /// contains nodes, one of which may itself be a `fold`, so the type is
    /// recursive.
    Subgraph(Box<Graph>),
}

/// How a `FoldNode` folds its passes into the single value it produces.
///
/// Adjacently tagged (`{"kind": "...", "value": ...}` for the variant that
/// carries data, `{"kind": "..."}` for the unit variants) so a future join rule
/// is a new variant that does not change how an existing document encodes,
/// exactly like `BranchCondition`.
///
/// The variants are grounded in what the AARG loop actually needs. `best_by` is
/// the argmax winner the loop's "best draft wins, never the last pass" rule
/// requires; `last` and `all` are the two obvious simpler folds a different
/// consumer might want.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FoldJoin {
    /// Produce the pass whose value MAXIMIZES the given reference (a path into
    /// the accumulated value, `score` or `review.overall_score`). This is the
    /// argmax the AARG loop needs: the best draft wins, never the last. The
    /// reference is parsed at submit like a `crate::expr` path, so a malformed
    /// one is a node-precise error.
    BestBy(String),
    /// Produce the value of the last pass the loop ran.
    Last,
    /// Produce every pass's value as a list, in pass order.
    All,
}

/// A directed edge: a typed payload flows from one node to another.
///
/// Edges are the single source of graph topology. Referential integrity, the
/// acyclic check, and the entry/terminal summary all read the edge list. No
/// ports are modeled here; a `port` pair is a documented additive
/// follow-up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// The source node id.
    pub from: String,
    /// The destination node id.
    pub to: String,
    /// Optional label. When the source is a `BranchNode`, this names the
    /// `BranchCase` this edge realizes. Data only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A small, valid document used across the round-trip tests.
    fn sample() -> Graph {
        Graph {
            schema_version: SCHEMA_VERSION,
            nodes: vec![
                Node::Agent(AgentNode {
                    id: "research".into(),
                    agent_hash: format!("sha256:{}", "a".repeat(64)),
                    name: None,
                    input_schema: None,
                    output_schema: Some(json!({"type": "object"})),
                }),
                Node::Gate(GateNode {
                    id: "approve".into(),
                    name: None,
                    prompt: Some("Approve publication?".into()),
                    approval_schema: json!({"type": "object"}),
                }),
            ],
            edges: vec![Edge {
                from: "research".into(),
                to: "approve".into(),
                label: None,
            }],
        }
    }

    /// Serializing then deserializing a document yields an equal value.
    #[test]
    fn round_trips_through_json() {
        let original = sample();
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Graph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored, "round trip changed the value: {json}");
    }

    /// A node serializes with the adjacent `kind`/`payload` shape, and the id
    /// rides inside the payload. No `name` was set, so none appears on the
    /// wire: this is the byte-stability guarantee the optional node name
    /// must not disturb.
    #[test]
    fn node_uses_adjacent_kind_payload_shape() {
        let node = Node::Tool(ToolNode {
            id: "publish".into(),
            tool: "http_post".into(),
            name: None,
            input: BTreeMap::new(),
            input_schema: None,
            output_schema: None,
        });
        let json = serde_json::to_string(&node).expect("serialize");
        assert_eq!(
            json,
            r#"{"kind":"tool","payload":{"id":"publish","tool":"http_post"}}"#
        );
    }

    /// Setting a node's `name` puts it on the wire; leaving it unset keeps the
    /// payload byte-identical to a document written before the field existed.
    #[test]
    fn node_name_is_present_only_when_set() {
        let named = Node::Tool(ToolNode {
            id: "publish".into(),
            tool: "http_post".into(),
            name: Some("Publish the draft".into()),
            input: BTreeMap::new(),
            input_schema: None,
            output_schema: None,
        });
        let json = serde_json::to_string(&named).expect("serialize");
        assert_eq!(
            json,
            r#"{"kind":"tool","payload":{"id":"publish","tool":"http_post","name":"Publish the draft"}}"#
        );

        let unnamed = Node::Tool(ToolNode {
            id: "publish".into(),
            tool: "http_post".into(),
            name: None,
            input: BTreeMap::new(),
            input_schema: None,
            output_schema: None,
        });
        assert_eq!(
            serde_json::to_string(&unnamed).expect("serialize"),
            r#"{"kind":"tool","payload":{"id":"publish","tool":"http_post"}}"#,
            "an unset name must not appear on the wire"
        );
    }

    /// A fold node round-trips and serializes with the adjacent kind/payload
    /// shape, its `join` as an adjacently tagged sub-object, and an unset
    /// `accumulator_schema` staying off the wire.
    #[test]
    fn fold_node_serializes_with_join_and_body_shapes() {
        let node = Node::Fold(FoldNode {
            id: "refine".into(),
            name: None,
            body: FoldBody::Node("tailor".into()),
            max_iterations: 3,
            stop_when: "score >= 0.85".into(),
            join: FoldJoin::BestBy("score".into()),
            accumulator_schema: None,
        });
        let json = serde_json::to_string(&node).expect("serialize");
        assert_eq!(
            json,
            r#"{"kind":"fold","payload":{"id":"refine","body":{"kind":"node","value":"tailor"},"max_iterations":3,"stop_when":"score >= 0.85","join":{"kind":"best_by","value":"score"}}}"#
        );
        let restored: Node = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(node, restored, "fold round trip changed the value: {json}");
    }

    /// The two unit `join` variants serialize with just their `kind` tag, no
    /// `value`; this is the additive-tagged shape a future join rule extends.
    #[test]
    fn fold_join_unit_variants_carry_only_the_kind_tag() {
        assert_eq!(
            serde_json::to_string(&FoldJoin::Last).expect("serialize"),
            r#"{"kind":"last"}"#
        );
        assert_eq!(
            serde_json::to_string(&FoldJoin::All).expect("serialize"),
            r#"{"kind":"all"}"#
        );
    }

    /// A stray top-level key on the document is rejected, not ignored: strict
    /// in, because a graph is a control document.
    #[test]
    fn unknown_document_field_is_rejected() {
        let text = r#"{"schema_version":1,"nodes":[],"edges":[],"surprise":true}"#;
        let error = serde_json::from_str::<Graph>(text).expect_err("must reject");
        assert!(
            error.to_string().contains("surprise"),
            "error should name the stray field: {error}"
        );
    }

    /// A stray key inside a node payload is rejected too.
    #[test]
    fn unknown_payload_field_is_rejected() {
        let text = r#"{"kind":"gate","payload":{"id":"g","approval_schema":{},"oops":1}}"#;
        let error = serde_json::from_str::<Node>(text).expect_err("must reject");
        assert!(
            error.to_string().contains("oops"),
            "error should name the stray field: {error}"
        );
    }

    /// A key other than `kind`/`payload` alongside a node is rejected.
    #[test]
    fn unknown_node_envelope_field_is_rejected() {
        let text = r#"{"kind":"gate","payload":{"id":"g","approval_schema":{}},"extra":1}"#;
        let error = serde_json::from_str::<Node>(text).expect_err("must reject");
        assert!(
            error.to_string().contains("extra"),
            "error should name the stray key: {error}"
        );
    }

    /// A `when` written as a bare string, the obvious skim-and-adapt mistake
    /// (copying `"value > 10000"` straight in instead of wrapping it in the
    /// `{"kind": "expression", "value": ...}` object), gets a product-language
    /// error naming both accepted shapes and echoing the string back, not
    /// serde's "adjacently tagged enum" internals.
    #[test]
    fn bare_string_branch_condition_names_accepted_shapes() {
        let text = r#"{"name":"big","when":"value > 10000"}"#;
        let error = serde_json::from_str::<BranchCase>(text).expect_err("must reject");
        let message = error.to_string();
        assert!(
            !message.contains("adjacently tagged enum"),
            "error should not leak serde internals: {message}"
        );
        assert!(
            message.contains(r#"{"kind": "expression", "value": "<expr>"}"#)
                && message.contains(r#"{"kind": "model_decision"}"#),
            "error should name both accepted shapes: {message}"
        );
        assert!(
            message.contains(r#"a bare string "value > 10000""#),
            "error should echo the offending value: {message}"
        );
    }

    /// A `when` written as some other wrong type (here, a bare number) also
    /// reads sensibly: same two accepted shapes, with the actual value
    /// described instead of a string-specific phrasing.
    #[test]
    fn non_string_branch_condition_also_names_accepted_shapes() {
        let text = r#"{"name":"big","when":10000}"#;
        let error = serde_json::from_str::<BranchCase>(text).expect_err("must reject");
        let message = error.to_string();
        assert!(
            !message.contains("adjacently tagged enum"),
            "error should not leak serde internals: {message}"
        );
        assert!(
            message.contains("a bare number 10000"),
            "error should describe the actual value: {message}"
        );
    }

    /// A `when` object with an unrecognized `kind` (a wrong-shaped object,
    /// not a bare scalar) still reads as an error about the object found,
    /// not serde internals.
    #[test]
    fn wrong_kind_branch_condition_names_accepted_shapes() {
        let text = r#"{"name":"big","when":{"kind":"regex","value":"x"}}"#;
        let error = serde_json::from_str::<BranchCase>(text).expect_err("must reject");
        let message = error.to_string();
        assert!(
            !message.contains("adjacently tagged enum"),
            "error should not leak serde internals: {message}"
        );
        assert!(
            message.contains("an object"),
            "error should describe the value as an object: {message}"
        );
    }

    /// The two valid `when` shapes still parse exactly as before: the custom
    /// `Deserialize` impl only changes the message on rejection, never
    /// acceptance.
    #[test]
    fn valid_branch_conditions_still_round_trip() {
        let expression = BranchCase {
            name: "big".into(),
            when: BranchCondition::Expression("value > 10000".into()),
        };
        let json = serde_json::to_string(&expression).expect("serialize");
        assert_eq!(
            json,
            r#"{"name":"big","when":{"kind":"expression","value":"value > 10000"}}"#
        );
        let restored: BranchCase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(expression, restored, "round trip changed the value: {json}");

        let model_decision = BranchCase {
            name: "review".into(),
            when: BranchCondition::ModelDecision,
        };
        let json = serde_json::to_string(&model_decision).expect("serialize");
        assert_eq!(
            json,
            r#"{"name":"review","when":{"kind":"model_decision"}}"#
        );
        let restored: BranchCase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            model_decision, restored,
            "round trip changed the value: {json}"
        );
    }

    /// A full document carrying a branch node serializes to the same bytes
    /// before and after this change, since only the `Deserialize` error path
    /// moved; `Serialize` is still the plain derive.
    #[test]
    fn branch_node_document_serializes_byte_identical() {
        let mut graph = sample();
        graph.nodes.push(Node::Branch(BranchNode {
            id: "route".into(),
            name: None,
            on: None,
            agent_hash: None,
            cases: vec![
                BranchCase {
                    name: "big".into(),
                    when: BranchCondition::Expression("value > 10000".into()),
                },
                BranchCase {
                    name: "small".into(),
                    when: BranchCondition::ModelDecision,
                },
            ],
        }));
        let json = serde_json::to_string(&graph).expect("serialize");
        let restored: Graph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(graph, restored, "round trip changed the value: {json}");
        let json_again = serde_json::to_string(&restored).expect("re-serialize");
        assert_eq!(
            json, json_again,
            "serialization is not byte-identical across a round trip"
        );
    }
}
