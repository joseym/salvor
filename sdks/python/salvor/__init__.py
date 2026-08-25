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

Every surface here comes in two transports over one set of rules. `AsyncClient`
and `AsyncClientRunDriver` carry the same method names as `Client` and
`ClientRunDriver`, awaited, and streaming reads `async for`. The rules they both
apply, the wire shapes, the event tail's cursor, the durable timer's arithmetic,
live in one sans-IO core, so the two transports cannot drift apart.
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
    DelayNode,
    FoldNode,
    GateNode,
    Graph,
    GraphBuilder,
    MapNode,
    ToolNode,
    all_passes,
    best_by,
    expression,
    fold_node,
    fold_subgraph,
    last,
    map_node,
    map_subgraph,
    model_decision,
)
from .models import (
    ClientToolDecl,
    EndFrame,
    Event,
    ForkEntry,
    ForkOrigin,
    ForkPreview,
    ForkResult,
    ForksIndex,
    GraphNodeProgress,
    GraphProjection,
    GraphShape,
    GraphSubmitted,
    GraphSummary,
    GraphValidation,
    GraphValidationError,
    PendingCall,
    RecordedWrite,
    ReplayState,
    ResumeResult,
    RunState,
    RunStatus,
    RunSummary,
    StoredGraph,
    Usage,
)

__all__ = [
    "Client",
    "EventStream",
    "ClientRunDriver",
    "ModelStepResult",
    "ModelStepStream",
    "AsyncClient",
    "AsyncEventStream",
    "AsyncClientRunDriver",
    "AsyncModelStepStream",
    "ClientToolDecl",
    "ClientToolIntentResult",
    "ClientModelIntentResult",
    "Waking",
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
    "GraphShape",
    "GraphSubmitted",
    "GraphSummary",
    "StoredGraph",
    "GraphValidation",
    "GraphValidationError",
    "GraphProjection",
    "GraphNodeProgress",
    "ForkOrigin",
    "ForkResult",
    "ForkPreview",
    "ForkEntry",
    "ForksIndex",
    "RecordedWrite",
    "GraphBuilder",
    "Graph",
    "AgentNode",
    "ToolNode",
    "GateNode",
    "BranchNode",
    "BranchCase",
    "MapNode",
    "FoldNode",
    "DelayNode",
    "SCHEMA_VERSION",
    "expression",
    "model_decision",
    "map_node",
    "map_subgraph",
    "fold_node",
    "fold_subgraph",
    "best_by",
    "last",
    "all_passes",
]

# Read from the installed distribution's metadata rather than hardcoded here: a literal is a
# second place the version lives, and it drifted two minor releases behind pyproject.toml before
# anyone noticed. Running from a source tree that was never installed has no metadata to read.
try:
    # Underscore-aliased: an unprefixed import here would export importlib's names as part of this
    # package's public surface, which is not something callers should be able to rely on.
    from importlib.metadata import PackageNotFoundError as _PackageNotFoundError
    from importlib.metadata import version as _dist_version

    __version__ = _dist_version("salvor")
except _PackageNotFoundError:  # pragma: no cover - a source checkout, not an install
    __version__ = "0.0.0+source"


def __getattr__(name: str):
    """Lazily resolve the HTTP client on first access.

    ``Client`` and ``EventStream`` live in :mod:`salvor.client`, and their async
    twins in :mod:`salvor.async_client`, all of which import httpx. Importing them here would make httpx a package-import-time
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
    if name in ("AsyncClient", "AsyncEventStream"):
        from .async_client import AsyncClient, AsyncEventStream

        globals()["AsyncClient"] = AsyncClient
        globals()["AsyncEventStream"] = AsyncEventStream
        return globals()[name]
    if name in ("AsyncClientRunDriver", "AsyncModelStepStream"):
        from .async_client_runs import AsyncClientRunDriver, AsyncModelStepStream

        globals()["AsyncClientRunDriver"] = AsyncClientRunDriver
        globals()["AsyncModelStepStream"] = AsyncModelStepStream
        return globals()[name]
    if name in (
        "ClientRunDriver",
        "ModelStepResult",
        "ModelStepStream",
        "ClientToolIntentResult",
        "ClientModelIntentResult",
        "Waking",
    ):
        from .client_runs import (
            ClientModelIntentResult,
            ClientRunDriver,
            ClientToolIntentResult,
            ModelStepResult,
            ModelStepStream,
            Waking,
        )

        globals()["ClientRunDriver"] = ClientRunDriver
        globals()["ModelStepResult"] = ModelStepResult
        globals()["ModelStepStream"] = ModelStepStream
        globals()["ClientToolIntentResult"] = ClientToolIntentResult
        globals()["ClientModelIntentResult"] = ClientModelIntentResult
        globals()["Waking"] = Waking
        return globals()[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
