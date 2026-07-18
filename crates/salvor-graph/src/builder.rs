//! A fluent, typed builder for a graph [`Graph`] document.
//!
//! The document model in [`crate::document`] is the wire format: strict,
//! adjacently tagged, and easy to get wrong by hand (a stray key, a payload
//! nested under the wrong tag, an edge that names a field instead of a node).
//! This builder is the author-facing front door. It never invents a new format;
//! it constructs the same [`Graph`] the model already defines, so whatever this
//! builder emits parses and validates exactly as a hand-written document would.
//!
//! # What the types buy you
//!
//! Each node kind has its own spec type ([`AgentSpec`], [`ToolSpec`],
//! [`GateSpec`], [`BranchSpec`], [`MapSpec`]) whose constructor demands the
//! fields that kind cannot do without: an agent needs its hash, a tool needs its
//! name, a gate needs its approval schema. A field that belongs to one kind is
//! not reachable on another, so "a gate with an agent_hash" is not a runtime
//! error, it is a shape you cannot write. The optional fields are chained
//! methods, present only where the model allows them. The result is that a
//! STRUCTURALLY malformed document is hard to express.
//!
//! # Where the builder stops
//!
//! Typed construction stops at structure. It does NOT check that an agent hash
//! is 64 hex digits, that a map's concurrency is positive, that edges name real
//! nodes, or that the graph is acyclic. Those are SEMANTIC rules, and they stay
//! with [`crate::validate`], which runs over the built `Graph` the same way it
//! runs over a parsed one. Build to get a well-shaped document; validate to
//! learn whether it is a legal one.
//!
//! # Authoring the canonical flow
//!
//! ```
//! use salvor_graph::{AgentSpec, GateSpec, GraphBuilder, ToolSpec};
//! use serde_json::json;
//!
//! let draft = json!({
//!     "type": "object",
//!     "properties": { "draft": { "type": "string" } },
//!     "required": ["draft"]
//! });
//!
//! let graph = GraphBuilder::new()
//!     .agent(
//!         AgentSpec::new("research", format!("sha256:{}", "1".repeat(64)))
//!             .output_schema(draft.clone()),
//!     )
//!     .agent(
//!         AgentSpec::new("review", format!("sha256:{}", "2".repeat(64)))
//!             .input_schema(draft.clone())
//!             .output_schema(draft),
//!     )
//!     .gate(
//!         GateSpec::new(
//!             "approve",
//!             json!({
//!                 "type": "object",
//!                 "properties": { "approved": { "type": "boolean" } },
//!                 "required": ["approved"]
//!             }),
//!         )
//!         .prompt("Approve this draft for publication?"),
//!     )
//!     .tool(
//!         ToolSpec::new("publish", "http_post")
//!             .input("body", "approve.draft")
//!             .input("url", "config.publish_url"),
//!     )
//!     .edge("research", "review")
//!     .edge("review", "approve")
//!     .edge("approve", "publish")
//!     .build();
//!
//! // Structure is done; semantics are a separate pass.
//! let summary = salvor_graph::validate(&graph).expect("the canonical flow is valid");
//! assert_eq!(summary.entry_nodes, ["research"]);
//! assert_eq!(summary.terminal_nodes, ["publish"]);
//! ```

use serde_json::Value;

use crate::document::{
    AgentNode, BranchCase, BranchCondition, BranchNode, Edge, GateNode, Graph, MapBody, MapNode,
    Node, SCHEMA_VERSION, ToolNode,
};

/// Accumulates nodes and edges, then freezes them into a [`Graph`].
///
/// Every `node`-adding method takes a per-kind spec and returns `self`, so a
/// whole document reads as one chain ending in [`build`](GraphBuilder::build).
/// The builder stamps [`SCHEMA_VERSION`] onto the document, so an author never
/// writes the version by hand.
#[derive(Clone, Debug, Default)]
pub struct GraphBuilder {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl GraphBuilder {
    /// Starts an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an `agent` node from its [`AgentSpec`].
    #[must_use]
    pub fn agent(mut self, spec: AgentSpec) -> Self {
        self.nodes.push(spec.into_node());
        self
    }

    /// Adds a `tool` node from its [`ToolSpec`].
    #[must_use]
    pub fn tool(mut self, spec: ToolSpec) -> Self {
        self.nodes.push(spec.into_node());
        self
    }

    /// Adds a `gate` node from its [`GateSpec`].
    #[must_use]
    pub fn gate(mut self, spec: GateSpec) -> Self {
        self.nodes.push(spec.into_node());
        self
    }

    /// Adds a `branch` node from its [`BranchSpec`].
    #[must_use]
    pub fn branch(mut self, spec: BranchSpec) -> Self {
        self.nodes.push(spec.into_node());
        self
    }

    /// Adds a `map` node from its [`MapSpec`].
    #[must_use]
    pub fn map(mut self, spec: MapSpec) -> Self {
        self.nodes.push(spec.into_node());
        self
    }

    /// Adds a plain edge from one node id to another.
    #[must_use]
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            label: None,
        });
        self
    }

    /// Adds a labeled edge. The label names the [`BranchCase`] this edge
    /// realizes when the source is a `branch`.
    #[must_use]
    pub fn labeled_edge(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            label: Some(label.into()),
        });
        self
    }

    /// Freezes the accumulated nodes and edges into a [`Graph`], stamping the
    /// current [`SCHEMA_VERSION`].
    ///
    /// Structure only: run [`crate::validate`] on the result to check the
    /// semantic rules (hash shape, referential integrity, acyclicity, and the
    /// rest).
    #[must_use]
    pub fn build(self) -> Graph {
        Graph {
            schema_version: SCHEMA_VERSION,
            nodes: self.nodes,
            edges: self.edges,
        }
    }
}

/// The spec for an `agent` node: a full agent loop referenced by content hash.
#[derive(Clone, Debug)]
pub struct AgentSpec {
    id: String,
    agent_hash: String,
    name: Option<String>,
    input_schema: Option<Value>,
    output_schema: Option<Value>,
}

impl AgentSpec {
    /// Starts an agent spec with its two required fields: the node id and the
    /// `sha256:<64 hex>` agent hash. The hash form is checked by
    /// [`crate::validate`], not here.
    pub fn new(id: impl Into<String>, agent_hash: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            agent_hash: agent_hash.into(),
            name: None,
            input_schema: None,
            output_schema: None,
        }
    }

    /// Sets a short display label for this node. Bounds (a 64-character cap,
    /// not empty or all whitespace) are checked by [`crate::validate`], not
    /// here; see [`crate::document`]'s "The optional node display name"
    /// section for why this field, unlike an agent's own `name`, is part of
    /// the graph's content hash.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Declares the JSON Schema for the payload this agent consumes.
    #[must_use]
    pub fn input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Declares the JSON Schema for the payload this agent produces.
    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    fn into_node(self) -> Node {
        Node::Agent(AgentNode {
            id: self.id,
            agent_hash: self.agent_hash,
            name: self.name,
            input_schema: self.input_schema,
            output_schema: self.output_schema,
        })
    }
}

/// The spec for a `tool` node: one direct tool invocation.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    id: String,
    tool: String,
    name: Option<String>,
    input: std::collections::BTreeMap<String, String>,
    input_schema: Option<Value>,
    output_schema: Option<Value>,
}

impl ToolSpec {
    /// Starts a tool spec with its required fields: the node id and the
    /// registered tool name.
    pub fn new(id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tool: tool.into(),
            name: None,
            input: std::collections::BTreeMap::new(),
            input_schema: None,
            output_schema: None,
        }
    }

    /// Sets a short display label for this node. Bounds (a 64-character cap,
    /// not empty or all whitespace) are checked by [`crate::validate`], not
    /// here; see [`crate::document`]'s "The optional node display name"
    /// section for why this field, unlike an agent's own `name`, is part of
    /// the graph's content hash.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Adds one input mapping: a tool input field name to an opaque source
    /// reference. Recorded as data; not resolved by this crate.
    #[must_use]
    pub fn input(mut self, field: impl Into<String>, source: impl Into<String>) -> Self {
        self.input.insert(field.into(), source.into());
        self
    }

    /// Declares the JSON Schema for the payload this tool consumes.
    #[must_use]
    pub fn input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Declares the JSON Schema for the payload this tool produces.
    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    fn into_node(self) -> Node {
        Node::Tool(ToolNode {
            id: self.id,
            tool: self.tool,
            name: self.name,
            input: self.input,
            input_schema: self.input_schema,
            output_schema: self.output_schema,
        })
    }
}

/// The spec for a `gate` node: human approval that suspends the run.
#[derive(Clone, Debug)]
pub struct GateSpec {
    id: String,
    name: Option<String>,
    prompt: Option<String>,
    approval_schema: Value,
}

impl GateSpec {
    /// Starts a gate spec with its required fields: the node id and the JSON
    /// Schema the approval input must satisfy. A gate with no declared approval
    /// shape is meaningless, so the schema is not optional.
    pub fn new(id: impl Into<String>, approval_schema: Value) -> Self {
        Self {
            id: id.into(),
            name: None,
            prompt: None,
            approval_schema,
        }
    }

    /// Sets a short display label for this node. Bounds (a 64-character cap,
    /// not empty or all whitespace) are checked by [`crate::validate`], not
    /// here; see [`crate::document`]'s "The optional node display name"
    /// section for why this field, unlike an agent's own `name`, is part of
    /// the graph's content hash.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the human-readable prompt shown in the approval inbox.
    #[must_use]
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    fn into_node(self) -> Node {
        Node::Gate(GateNode {
            id: self.id,
            name: self.name,
            prompt: self.prompt,
            approval_schema: self.approval_schema,
        })
    }
}

/// The spec for a `branch` node: routes on a typed output.
#[derive(Clone, Debug)]
pub struct BranchSpec {
    id: String,
    name: Option<String>,
    on: Option<String>,
    agent_hash: Option<String>,
    cases: Vec<BranchCase>,
}

impl BranchSpec {
    /// Starts a branch spec with its required node id. Cases are added with
    /// [`case`](BranchSpec::case).
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            on: None,
            agent_hash: None,
            cases: Vec::new(),
        }
    }

    /// Sets a short display label for this node. Bounds (a 64-character cap,
    /// not empty or all whitespace) are checked by [`crate::validate`], not
    /// here; see [`crate::document`]'s "The optional node display name"
    /// section for why this field, unlike an agent's own `name`, is part of
    /// the graph's content hash.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the opaque reference to the typed value the branch routes on.
    #[must_use]
    pub fn on(mut self, on: impl Into<String>) -> Self {
        self.on = Some(on.into());
        self
    }

    /// Sets the `sha256:<64 hex>` hash of the agent that decides a
    /// [`BranchCondition::ModelDecision`] case. Required by [`crate::validate`]
    /// on any branch that carries a model-decision case; the hash form is
    /// checked there, not here.
    #[must_use]
    pub fn agent_hash(mut self, agent_hash: impl Into<String>) -> Self {
        self.agent_hash = Some(agent_hash.into());
        self
    }

    /// Adds a named case selected by the given condition. The route it realizes
    /// is a [`labeled_edge`](GraphBuilder::labeled_edge) whose label matches the
    /// case name.
    #[must_use]
    pub fn case(mut self, name: impl Into<String>, when: BranchCondition) -> Self {
        self.cases.push(BranchCase {
            name: name.into(),
            when,
        });
        self
    }

    fn into_node(self) -> Node {
        Node::Branch(BranchNode {
            id: self.id,
            name: self.name,
            on: self.on,
            agent_hash: self.agent_hash,
            cases: self.cases,
        })
    }
}

/// The spec for a `map` node: fan-out a sub-run per element of a typed list,
/// with a concurrency cap.
#[derive(Clone, Debug)]
pub struct MapSpec {
    id: String,
    name: Option<String>,
    over: String,
    concurrency: u32,
    body: MapBody,
    output_schema: Option<Value>,
}

impl MapSpec {
    /// Starts a map spec with its required fields: the node id, the opaque
    /// reference to the list it fans out over, the concurrency cap, and the
    /// [`MapBody`] each element is mapped through.
    pub fn new(
        id: impl Into<String>,
        over: impl Into<String>,
        concurrency: u32,
        body: MapBody,
    ) -> Self {
        Self {
            id: id.into(),
            name: None,
            over: over.into(),
            concurrency,
            body,
            output_schema: None,
        }
    }

    /// Sets a short display label for this node. Bounds (a 64-character cap,
    /// not empty or all whitespace) are checked by [`crate::validate`], not
    /// here; see [`crate::document`]'s "The optional node display name"
    /// section for why this field, unlike an agent's own `name`, is part of
    /// the graph's content hash.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Declares the JSON Schema for the joined list this node produces.
    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    fn into_node(self) -> Node {
        Node::Map(MapNode {
            id: self.id,
            name: self.name,
            over: self.over,
            concurrency: self.concurrency,
            body: self.body,
            output_schema: self.output_schema,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A schema shape reused by several nodes in the canonical flow.
    fn draft_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "draft": { "type": "string" } },
            "required": ["draft"]
        })
    }

    /// Builds the exact research -> review -> approve -> publish flow the
    /// canonical fixture records.
    fn canonical_flow() -> Graph {
        GraphBuilder::new()
            .agent(
                AgentSpec::new("research", format!("sha256:{}", "1".repeat(64)))
                    .output_schema(draft_schema()),
            )
            .agent(
                AgentSpec::new("review", format!("sha256:{}", "2".repeat(64)))
                    .input_schema(draft_schema())
                    .output_schema(draft_schema()),
            )
            .gate(
                GateSpec::new(
                    "approve",
                    json!({
                        "type": "object",
                        "properties": { "approved": { "type": "boolean" } },
                        "required": ["approved"]
                    }),
                )
                .prompt("Approve this draft for publication?"),
            )
            .tool(
                ToolSpec::new("publish", "http_post")
                    .input("body", "approve.draft")
                    .input("url", "config.publish_url"),
            )
            .edge("research", "review")
            .edge("review", "approve")
            .edge("approve", "publish")
            .build()
    }

    /// The builder emits a document structurally equal to the canonical fixture
    /// that keeps all three language builders honest.
    #[test]
    fn builds_the_canonical_document() {
        let built = serde_json::to_value(canonical_flow()).expect("serialize built graph");
        let canonical: Value = serde_json::from_str(include_str!(
            "../../../examples/graphs/research-review-publish.json"
        ))
        .expect("parse canonical fixture");
        assert_eq!(
            built, canonical,
            "builder output must match the canonical fixture exactly"
        );
    }

    /// The built canonical flow passes semantic validation.
    #[test]
    fn canonical_document_validates() {
        let summary = crate::validate(&canonical_flow()).expect("canonical flow is valid");
        assert_eq!(summary.node_count, 4);
        assert_eq!(summary.edge_count, 3);
        assert_eq!(summary.entry_nodes, vec!["research"]);
        assert_eq!(summary.terminal_nodes, vec!["publish"]);
    }

    /// The branch and map specs build the shapes the model expects: an unset
    /// optional field stays off the wire, and a labeled edge realizes a case.
    #[test]
    fn branch_and_map_specs_build_expected_shapes() {
        let graph = GraphBuilder::new()
            .agent(AgentSpec::new(
                "score",
                format!("sha256:{}", "a".repeat(64)),
            ))
            .branch(
                BranchSpec::new("route")
                    .on("score.value")
                    .case("high", BranchCondition::Expression("score > 0.8".into()))
                    .case("ask", BranchCondition::ModelDecision),
            )
            .agent(AgentSpec::new(
                "worker",
                format!("sha256:{}", "b".repeat(64)),
            ))
            .map(MapSpec::new(
                "fanout",
                "route.items",
                4,
                MapBody::Node("worker".into()),
            ))
            .edge("score", "route")
            .labeled_edge("route", "fanout", "high")
            .build();

        // The map node serializes with no output_schema key, because the spec
        // never set one.
        let value = serde_json::to_value(&graph).expect("serialize");
        let map_payload = value["nodes"][3]["payload"].clone();
        assert!(
            map_payload.get("output_schema").is_none(),
            "unset optional field must stay off the wire: {map_payload}"
        );
        assert_eq!(value["edges"][1]["label"], json!("high"));
    }

    /// `.name(...)` is available on every node kind's spec, puts the name on
    /// the wire when set, and validates clean; a sibling node with no `.name`
    /// call carries none, proving the two coexist in one document.
    #[test]
    fn every_spec_kind_accepts_a_display_name() {
        let graph = GraphBuilder::new()
            .agent(
                AgentSpec::new("research", format!("sha256:{}", "1".repeat(64)))
                    .name("Research the topic"),
            )
            .tool(ToolSpec::new("publish", "http_post").name("Publish the draft"))
            .gate(GateSpec::new("approve", json!({"type": "object"})).name("Approve the draft"))
            .branch(
                BranchSpec::new("route")
                    .name("Route on confidence")
                    .case("high", BranchCondition::Expression("score > 0.8".into())),
            )
            .map(
                MapSpec::new("fanout", "route.items", 2, MapBody::Node("research".into()))
                    .name("Notify each watcher"),
            )
            .edge("research", "publish")
            .build();

        let summary = crate::validate(&graph).expect("named nodes still validate");
        assert_eq!(summary.node_count, 5);

        let value = serde_json::to_value(&graph).expect("serialize");
        for (index, expected) in [
            (0, "Research the topic"),
            (1, "Publish the draft"),
            (2, "Approve the draft"),
            (3, "Route on confidence"),
            (4, "Notify each watcher"),
        ] {
            assert_eq!(
                value["nodes"][index]["payload"]["name"],
                json!(expected),
                "node {index} carries its display name on the wire"
            );
        }
    }
}
