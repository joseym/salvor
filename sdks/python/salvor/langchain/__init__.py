"""``salvor.langchain``: durability for an agent you already wrote.

Add one middleware to ``create_agent`` and every model call and every tool call
the agent makes is recorded in a salvor client-driven run, keyed by the
LangGraph ``thread_id``. Nothing else about the app changes: the same graph, the
same provider, the same key, the same tools. What changes is what a second
invoke of the same thread costs. The first invoke pays the provider and runs the
tools; a second invoke, after a crash or a redeploy or a retry, meets the
recorded answers at the positions the first one wrote them and returns those
instead.

    from langchain.agents import create_agent
    from salvor import AsyncClient
    from salvor.langchain import SalvorMiddleware

    agent = create_agent(
        model=model,
        tools=tools,
        middleware=[SalvorMiddleware(AsyncClient("http://127.0.0.1:8080"))],
    )

    await agent.ainvoke(
        {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
        {"configurable": {"thread_id": "order-7781"}},
    )

What this is, and what it is not
--------------------------------

It is a recorded effect ledger with exactly-once writes, under LangGraph's
orchestration. Every call that leaves the process goes through salvor's log
first, so nothing is paid for twice and no keyed write lands twice.

It is not replay of the graph. LangGraph still owns the clock, the randomness
and the branch order; salvor sees only the calls, not the decisions between
them. A graph that branches on ``time.time()`` will take a different branch on
the second invoke, and the middleware will honestly tell you so rather than
pretend the recorded answers still apply.

What the operator has to declare
--------------------------------

A tool's effect class, its input schema, its output schema and its idempotency
key are the operator's, not this middleware's. They come from a client-tool
declaration loaded by the server (``salvor serve --client-tool <FILE>``; see
``examples/client-tools/refund-card.toml``). The middleware sends the tool's
name and the model's arguments and nothing else, which is why a tool nobody
declared is refused by name instead of quietly recorded as a harmless read.

Installing
----------

LangChain is an optional extra, so ``import salvor`` pulls none of it in::

    pip install 'salvor[langchain]'

Importing this module without it raises :class:`ImportError` naming that line.
"""

from __future__ import annotations

from .current_call import ToolCallContext, current_tool_call
from .errors import SalvorMiddlewareError
from .hash import canonical_json, hash_value, is_uuid, run_id_for_thread, sha256_hex
from .request import canonical_request, request_hash
from .tape import ModelOutcome, RunTape, ToolOutcome

# LangChain itself is reached only from here down. These are the modules that
# import it, so a checkout without the extra installed fails here and is told
# the one command that fixes it, rather than failing somewhere inside a hook
# halfway through an agent run.
try:
    from .finish import FinishedThread, finish_thread
    from .middleware import SalvorMiddleware, salvor_middleware
    from .replay_model import ReplayChatModel
except ImportError as missing:  # pragma: no cover - depends on what is installed
    raise ImportError(
        "salvor.langchain needs LangChain, which the plain `salvor` package "
        "does not install. Install the extra: pip install 'salvor[langchain]'. "
        "(The underlying import failed with: {reason})".format(reason=missing)
    ) from missing

__all__ = [
    "FinishedThread",
    "ModelOutcome",
    "ReplayChatModel",
    "RunTape",
    "SalvorMiddleware",
    "SalvorMiddlewareError",
    "ToolCallContext",
    "ToolOutcome",
    "canonical_json",
    "canonical_request",
    "current_tool_call",
    "finish_thread",
    "hash_value",
    "is_uuid",
    "request_hash",
    "run_id_for_thread",
    "salvor_middleware",
    "sha256_hex",
]
