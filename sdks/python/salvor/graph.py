"""A typed builder for Salvor graph documents.

The canonical format lives in Rust (``crates/salvor-graph``): a strict,
adjacently tagged document that ``salvor graph validate`` accepts or rejects.
This module mirrors that format with dataclasses and adds a builder, so a
Python author gets editor typing and completion while producing the exact same
wire JSON. It invents no new format: whatever :meth:`GraphBuilder.build`
returns serializes to the same document a hand-written file would parse to.

The dataclasses make a STRUCTURALLY malformed document hard to write: each node
kind has its own type whose required fields are constructor arguments, and an
unset optional field is left OFF the emitted object entirely (never ``None`` on
the wire), so the JSON matches the canonical document after normalization.

SEMANTIC checks (hash shape, referential integrity, acyclicity) are not done
here; they stay with ``salvor graph validate``, run over the emitted JSON.

Standard library only. No third-party dependency is imported.

The optional node display name
-------------------------------

Every node type may carry an optional ``name``: a short, purely
presentational label ("Approve the draft"). Bounds mirror the agent
definition's own ``name`` precedent: at most 64 characters, and, when set,
not empty or all whitespace -- ``salvor graph validate`` (and ``POST
/v1/graphs``) enforce both, node-precise. Unlike an agent's ``name``
(excluded from its definition hash so a rename never mints a new agent
identity), a node's ``name`` is an ordinary field on the payload and hashes
like any other: a graph document IS its content hash, so renaming a node is
authoring a new document version, by design.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Optional, Union

# The schema version this build stamps onto every document it writes.
SCHEMA_VERSION = 1

# A JSON value: the type of the schema fields a node may declare. Kept as Any
# because a JSON Schema is an open-ended nested structure.
JsonValue = Any

# A serialized node or edge: a plain JSON object.
Json = dict[str, Any]


@dataclass
class AgentNode:
    """An ``agent`` node: a full agent loop referenced by content hash."""

    id: str
    agent_hash: str
    name: Optional[str] = None
    input_schema: Optional[JsonValue] = None
    output_schema: Optional[JsonValue] = None

    def to_node(self) -> Json:
        payload: Json = {"id": self.id, "agent_hash": self.agent_hash}
        if self.name is not None:
            payload["name"] = self.name
        if self.input_schema is not None:
            payload["input_schema"] = self.input_schema
        if self.output_schema is not None:
            payload["output_schema"] = self.output_schema
        return {"kind": "agent", "payload": payload}


@dataclass
class ToolNode:
    """A ``tool`` node: one direct tool invocation."""

    id: str
    tool: str
    name: Optional[str] = None
    input: Optional[dict[str, str]] = None
    input_schema: Optional[JsonValue] = None
    output_schema: Optional[JsonValue] = None

    def to_node(self) -> Json:
        payload: Json = {"id": self.id, "tool": self.tool}
        if self.name is not None:
            payload["name"] = self.name
        if self.input:
            payload["input"] = dict(self.input)
        if self.input_schema is not None:
            payload["input_schema"] = self.input_schema
        if self.output_schema is not None:
            payload["output_schema"] = self.output_schema
        return {"kind": "tool", "payload": payload}


@dataclass
class GateNode:
    """A ``gate`` node: human approval that suspends the run."""

    id: str
    approval_schema: JsonValue
    name: Optional[str] = None
    prompt: Optional[str] = None

    def to_node(self) -> Json:
        payload: Json = {"id": self.id}
        if self.name is not None:
            payload["name"] = self.name
        if self.prompt is not None:
            payload["prompt"] = self.prompt
        payload["approval_schema"] = self.approval_schema
        return {"kind": "gate", "payload": payload}


@dataclass
class BranchCase:
    """One case of a branch node: a name and the condition that selects it."""

    name: str
    when: Json

    def to_dict(self) -> Json:
        return {"name": self.name, "when": self.when}


def expression(source: str) -> Json:
    """A branch condition: an opaque boolean expression, recorded as data."""
    return {"kind": "expression", "value": source}


def model_decision() -> Json:
    """A branch condition chosen by a model decision at run time."""
    return {"kind": "model_decision"}


@dataclass
class BranchNode:
    """A ``branch`` node: routes on a typed output."""

    id: str
    cases: list[BranchCase]
    name: Optional[str] = None
    on: Optional[str] = None

    def to_node(self) -> Json:
        payload: Json = {"id": self.id}
        if self.name is not None:
            payload["name"] = self.name
        if self.on is not None:
            payload["on"] = self.on
        payload["cases"] = [case.to_dict() for case in self.cases]
        return {"kind": "branch", "payload": payload}


def map_node(node_id: str) -> Json:
    """A map body: map each element through an existing node, by id."""
    return {"kind": "node", "value": node_id}


def map_subgraph(subgraph: "Graph") -> Json:
    """A map body: map each element through an embedded sub-graph."""
    return {"kind": "subgraph", "value": subgraph.to_dict()}


@dataclass
class MapNode:
    """A ``map`` node: fan-out a sub-run per element of a typed list."""

    id: str
    over: str
    concurrency: int
    body: Json
    name: Optional[str] = None
    output_schema: Optional[JsonValue] = None

    def to_node(self) -> Json:
        payload: Json = {
            "id": self.id,
            "over": self.over,
            "concurrency": self.concurrency,
            "body": self.body,
        }
        if self.name is not None:
            payload["name"] = self.name
        if self.output_schema is not None:
            payload["output_schema"] = self.output_schema
        return {"kind": "map", "payload": payload}


def fold_node(node_id: str) -> Json:
    """A fold body: run each pass through an existing node, by id."""
    return {"kind": "node", "value": node_id}


def fold_subgraph(subgraph: "Graph") -> Json:
    """A fold body: run each pass through an embedded sub-graph."""
    return {"kind": "subgraph", "value": subgraph.to_dict()}


def best_by(reference: str) -> Json:
    """A fold join: produce the pass whose value maximizes ``reference`` (a path
    into the accumulated value). The argmax winner the AARG loop needs."""
    return {"kind": "best_by", "value": reference}


def last() -> Json:
    """A fold join: produce the value of the last pass the loop ran."""
    return {"kind": "last"}


def all_passes() -> Json:
    """A fold join: produce every pass's value as a list, in pass order."""
    return {"kind": "all"}


@dataclass
class FoldNode:
    """A ``fold`` node: bounded iteration that accumulates across passes."""

    id: str
    body: Json
    max_iterations: int
    stop_when: str
    join: Json
    name: Optional[str] = None
    accumulator_schema: Optional[JsonValue] = None

    def to_node(self) -> Json:
        payload: Json = {"id": self.id}
        if self.name is not None:
            payload["name"] = self.name
        payload["body"] = self.body
        payload["max_iterations"] = self.max_iterations
        payload["stop_when"] = self.stop_when
        payload["join"] = self.join
        if self.accumulator_schema is not None:
            payload["accumulator_schema"] = self.accumulator_schema
        return {"kind": "fold", "payload": payload}


# Any of the six node dataclasses.
NodeSpec = Union[AgentNode, ToolNode, GateNode, BranchNode, MapNode, FoldNode]


@dataclass
class Graph:
    """A built graph document: schema version, nodes, and edges.

    ``nodes`` and ``edges`` are already serialized JSON objects, so
    :meth:`to_dict` is a shallow assembly and :meth:`to_json` a plain dump.
    """

    schema_version: int
    nodes: list[Json] = field(default_factory=list)
    edges: list[Json] = field(default_factory=list)

    def to_dict(self) -> Json:
        return {
            "schema_version": self.schema_version,
            "nodes": self.nodes,
            "edges": self.edges,
        }

    def to_json(self, **kwargs: Any) -> str:
        """Serializes the document to a JSON string (``json.dumps`` kwargs pass
        through, e.g. ``indent=2``)."""
        return json.dumps(self.to_dict(), **kwargs)


class GraphBuilder:
    """Accumulates nodes and edges, then freezes them into a :class:`Graph`.

    Each node method takes the id plus that kind's required fields, then the
    optional ones as keyword arguments. Every method returns ``self``, so a
    whole document reads as one chain ending in :meth:`build`. The builder
    stamps :data:`SCHEMA_VERSION`, so an author never writes the version by
    hand.
    """

    def __init__(self) -> None:
        self._nodes: list[Json] = []
        self._edges: list[Json] = []

    def agent(
        self,
        id: str,
        agent_hash: str,
        *,
        name: Optional[str] = None,
        input_schema: Optional[JsonValue] = None,
        output_schema: Optional[JsonValue] = None,
    ) -> "GraphBuilder":
        """Adds an ``agent`` node, referenced by its ``sha256:<64 hex>`` hash.

        ``name`` is an optional short display label (see the module docs'
        "The optional node display name" section); its bounds are checked by
        ``salvor graph validate``, not here.
        """
        self._nodes.append(
            AgentNode(id, agent_hash, name, input_schema, output_schema).to_node()
        )
        return self

    def tool(
        self,
        id: str,
        tool: str,
        *,
        name: Optional[str] = None,
        input: Optional[dict[str, str]] = None,
        input_schema: Optional[JsonValue] = None,
        output_schema: Optional[JsonValue] = None,
    ) -> "GraphBuilder":
        """Adds a ``tool`` node for one direct invocation of a registered tool."""
        self._nodes.append(
            ToolNode(id, tool, name, input, input_schema, output_schema).to_node()
        )
        return self

    def gate(
        self,
        id: str,
        approval_schema: JsonValue,
        *,
        name: Optional[str] = None,
        prompt: Optional[str] = None,
    ) -> "GraphBuilder":
        """Adds a ``gate`` node. The approval schema is required."""
        self._nodes.append(GateNode(id, approval_schema, name, prompt).to_node())
        return self

    def branch(
        self,
        id: str,
        cases: list[BranchCase],
        *,
        name: Optional[str] = None,
        on: Optional[str] = None,
    ) -> "GraphBuilder":
        """Adds a ``branch`` node with its named cases."""
        self._nodes.append(BranchNode(id, cases, name, on).to_node())
        return self

    def map(
        self,
        id: str,
        over: str,
        concurrency: int,
        body: Json,
        *,
        name: Optional[str] = None,
        output_schema: Optional[JsonValue] = None,
    ) -> "GraphBuilder":
        """Adds a ``map`` node with its fan-out reference, cap, and body."""
        self._nodes.append(
            MapNode(id, over, concurrency, body, name, output_schema).to_node()
        )
        return self

    def fold(
        self,
        id: str,
        body: Json,
        max_iterations: int,
        stop_when: str,
        join: Json,
        *,
        name: Optional[str] = None,
        accumulator_schema: Optional[JsonValue] = None,
    ) -> "GraphBuilder":
        """Adds a ``fold`` node: bounded iteration with a stop predicate and a
        join rule. The bound's positivity, the predicate's parse, and a
        ``best_by`` reference's shape are checked by ``salvor graph validate``,
        not here."""
        self._nodes.append(
            FoldNode(
                id, body, max_iterations, stop_when, join, name, accumulator_schema
            ).to_node()
        )
        return self

    def edge(
        self,
        source: str,
        target: str,
        *,
        label: Optional[str] = None,
    ) -> "GraphBuilder":
        """Adds an edge. Pass a ``label`` to name the branch case it realizes."""
        edge: Json = {"from": source, "to": target}
        if label is not None:
            edge["label"] = label
        self._edges.append(edge)
        return self

    def build(self) -> Graph:
        """Freezes the accumulated nodes and edges into a :class:`Graph`,
        stamping the current :data:`SCHEMA_VERSION`. Structure only: pipe the
        result through ``salvor graph validate`` for the semantic checks."""
        return Graph(
            schema_version=SCHEMA_VERSION,
            nodes=list(self._nodes),
            edges=list(self._edges),
        )
