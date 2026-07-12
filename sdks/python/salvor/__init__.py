"""Salvor: a thin Python client over the Salvor control plane.

Submit an agent definition, start a run, stream its events to completion, and
resume or reconcile it, all over HTTP. The client holds no durability logic:
the one Rust process on the other end owns exact replay, crash-safe resume, and
the write-ahead reconciliation rule.

    from salvor import Client

    client = Client("http://127.0.0.1:8080")
    agent = client.register_agent(open("agent.toml").read())
    run_id = client.start_run(agent, {"question": "..."})
    for event in (stream := client.stream_events(run_id)):
        print(event.seq, event.kind)
    print(stream.end.status.output)
"""

from .errors import (
    DivergenceError,
    NeedsReconciliationError,
    SalvorAPIError,
    SalvorError,
    SalvorStreamError,
)
from .graph import (
    SCHEMA_VERSION,
    AgentNode,
    BranchCase,
    BranchNode,
    GateNode,
    Graph,
    GraphBuilder,
    MapNode,
    ToolNode,
    expression,
    map_node,
    map_subgraph,
    model_decision,
)
from .models import (
    EndFrame,
    Event,
    PendingCall,
    ReplayState,
    ResumeResult,
    RunState,
    RunStatus,
    RunSummary,
    Usage,
)

__all__ = [
    "Client",
    "EventStream",
    "ClientRunDriver",
    "ModelStepResult",
    "ModelStepStream",
    "SalvorError",
    "SalvorAPIError",
    "NeedsReconciliationError",
    "DivergenceError",
    "SalvorStreamError",
    "Event",
    "EndFrame",
    "RunState",
    "RunStatus",
    "RunSummary",
    "ReplayState",
    "ResumeResult",
    "PendingCall",
    "Usage",
    "GraphBuilder",
    "Graph",
    "AgentNode",
    "ToolNode",
    "GateNode",
    "BranchNode",
    "BranchCase",
    "MapNode",
    "SCHEMA_VERSION",
    "expression",
    "model_decision",
    "map_node",
    "map_subgraph",
]

__version__ = "0.3.0"


def __getattr__(name: str):
    """Lazily resolve the HTTP client on first access.

    ``Client`` and ``EventStream`` live in :mod:`salvor.client`, which imports
    httpx. Importing them here would make httpx a package-import-time
    requirement, so authoring a graph document (``from salvor import
    GraphBuilder``) would fail without httpx installed. Resolving them through
    a module-level ``__getattr__`` (PEP 562) means httpx is imported only when
    someone actually reaches for the client, never when authoring a graph.
    httpx stays the client's dependency; it is just no longer needed to import
    the package.
    """
    if name in ("Client", "EventStream"):
        from .client import Client, EventStream

        globals()["Client"] = Client
        globals()["EventStream"] = EventStream
        return globals()[name]
    if name in ("ClientRunDriver", "ModelStepResult", "ModelStepStream"):
        from .client_runs import ClientRunDriver, ModelStepResult, ModelStepStream

        globals()["ClientRunDriver"] = ClientRunDriver
        globals()["ModelStepResult"] = ModelStepResult
        globals()["ModelStepStream"] = ModelStepStream
        return globals()[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
