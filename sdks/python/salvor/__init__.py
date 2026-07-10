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

from .client import Client, EventStream
from .errors import (
    NeedsReconciliationError,
    SalvorAPIError,
    SalvorError,
    SalvorStreamError,
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
    "SalvorError",
    "SalvorAPIError",
    "NeedsReconciliationError",
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
]

__version__ = "0.3.0"
