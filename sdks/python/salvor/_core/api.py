"""Every server-driven control-plane operation, as a described :class:`Call`.

One function per public client method, each returning the request to send and
the parse that turns the answer into a typed model. The synchronous and
asynchronous clients both read their whole surface from here, so a path, a body
key or a decode lives in exactly one place and cannot drift between them.
"""

from __future__ import annotations

import json
from typing import Any, Optional, Union

from ..graph import Graph
from ..models import (
    ClientToolDecl,
    ForkPreview,
    ForkResult,
    ForksIndex,
    GraphProjection,
    GraphSubmitted,
    GraphSummary,
    GraphValidation,
    ReplayState,
    ResumeResult,
    RunState,
    RunSummary,
    StoredGraph,
)
from .wire import Call, identity


def document(document: Union[Graph, dict[str, Any]]) -> dict[str, Any]:
    """The wire JSON for a graph document, from either a built
    :class:`~salvor.graph.Graph` or a plain dict of the same fields. A dict is
    passed through untouched, so a document authored by hand (or read from a
    file) needs no builder."""
    return document.to_dict() if isinstance(document, Graph) else document


def connection(base_url: str, token: Optional[str]) -> tuple[str, dict[str, str]]:
    """The base URL and standing headers one client is built over: the trailing
    slash trimmed, and the bearer header present only when there is a token."""
    headers: dict[str, str] = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return base_url.rstrip("/"), headers


# -- agents -------------------------------------------------------------------


def register_agent(definition: Union[str, dict[str, Any]]) -> Call:
    """A TOML string goes up as ``application/toml``; the same fields as a dict
    go up as ``application/json``. Either way the answer is the content hash."""
    if isinstance(definition, str):
        content = definition.encode("utf-8")
        content_type = "application/toml"
    else:
        content = json.dumps(definition).encode("utf-8")
        content_type = "application/json"
    return Call(
        "POST",
        "/v1/agents",
        parse=lambda obj: obj["agent"],
        headers={"Content-Type": content_type},
        content=content,
    )


def list_agents() -> Call:
    return Call(
        "GET",
        "/v1/agents",
        parse=lambda obj: [a["agent"] for a in obj.get("agents", [])],
    )


def get_agent(agent_hash: str) -> Call:
    return Call("GET", f"/v1/agents/{agent_hash}", parse=identity)


# -- runs ---------------------------------------------------------------------


def start_run(
    agent: str,
    input: Any,
    run_id: Optional[str],
    labels: Optional[dict[str, str]],
) -> Call:
    """An omitted ``run_id`` or ``labels`` leaves the key off the body entirely,
    which is the server's additive-optional contract: a caller that passes
    neither sends the bytes a caller predating both parameters sent."""
    body: dict[str, Any] = {"agent": agent, "input": input}
    if run_id is not None:
        body["run_id"] = run_id
    if labels is not None:
        body["labels"] = labels
    return Call("POST", "/v1/runs", parse=lambda obj: obj["run"], json_body=body)


def list_runs() -> Call:
    return Call(
        "GET",
        "/v1/runs",
        parse=lambda obj: [RunSummary.from_json(r) for r in obj.get("runs", [])],
    )


def get_run(run_id: str) -> Call:
    return Call("GET", f"/v1/runs/{run_id}", parse=RunState.from_json)


def replay(run_id: str) -> Call:
    return Call("GET", f"/v1/runs/{run_id}/replay", parse=ReplayState.from_json)


def resume(run_id: str, input: Any) -> Call:
    body = {} if input is None else {"input": input}
    return Call(
        "POST",
        f"/v1/runs/{run_id}/resume",
        parse=ResumeResult.from_json,
        json_body=body,
    )


def resolve(run_id: str, output: Any) -> Call:
    """The resolve response nests the status, so the parse reshapes it into the
    view :func:`get_run` returns: a caller reads the run one way."""

    def parse(obj: dict[str, Any]) -> RunState:
        return RunState.from_json(
            {
                "run": obj.get("run", run_id),
                "status": obj.get("status", {}),
                "event_count": obj.get("event_count", 0),
            }
        )

    return Call(
        "POST",
        f"/v1/runs/{run_id}/resolve",
        parse=parse,
        json_body={"output": output},
    )


# -- graphs -------------------------------------------------------------------


def submit_graph(doc: Union[Graph, dict[str, Any]]) -> Call:
    return Call(
        "POST",
        "/v1/graphs",
        parse=GraphSubmitted.from_json,
        json_body=document(doc),
    )


def list_graphs() -> Call:
    return Call(
        "GET",
        "/v1/graphs",
        parse=lambda obj: [GraphSummary.from_json(g) for g in obj.get("graphs", [])],
    )


def get_graph(graph_hash: str) -> Call:
    return Call("GET", f"/v1/graphs/{graph_hash}", parse=StoredGraph.from_json)


def validate_graph(doc: Union[Graph, dict[str, Any]]) -> Call:
    return Call(
        "POST",
        "/v1/graphs/validate",
        parse=GraphValidation.from_json,
        json_body=document(doc),
    )


def start_graph_run(
    graph_hash: str, input: Any, labels: Optional[dict[str, str]]
) -> Call:
    body: dict[str, Any] = {"graph_hash": graph_hash, "input": input}
    if labels is not None:
        body["labels"] = labels
    return Call(
        "POST", "/v1/graph-runs", parse=lambda obj: obj["run"], json_body=body
    )


def get_run_graph(run_id: str) -> Call:
    return Call(
        "GET", f"/v1/runs/{run_id}/graph", parse=GraphProjection.from_json
    )


def fork_run(
    run_id: str,
    from_node: str,
    acknowledge_writes: Optional[list[int]],
    *,
    dry_run: bool,
) -> Call:
    """One endpoint answers both the fork and its preview; ``dry_run`` is the
    whole difference, and the parse follows it."""
    body: dict[str, Any] = {"from_node": from_node}
    if dry_run:
        body["dry_run"] = True
    if acknowledge_writes is not None:
        body["acknowledge_writes"] = acknowledge_writes
    parse = ForkPreview.from_json if dry_run else ForkResult.from_json
    return Call(
        "POST", f"/v1/runs/{run_id}/fork", parse=parse, json_body=body
    )


def list_forks(run_id: str) -> Call:
    return Call("GET", f"/v1/runs/{run_id}/forks", parse=ForksIndex.from_json)


# -- client-performed tools ----------------------------------------------------


def list_client_tools() -> Call:
    return Call(
        "GET",
        "/v1/client-tools",
        parse=lambda obj: [
            ClientToolDecl.from_json(t) for t in obj.get("client_tools", [])
        ],
    )
