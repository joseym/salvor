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
    from salvor import Client
    from salvor.langchain import SalvorMiddleware

    agent = create_agent(
        model=model,
        tools=tools,
        middleware=[SalvorMiddleware(Client("http://127.0.0.1:8080"))],
    )

    agent.invoke(
        {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
        {"configurable": {"thread_id": "order-7781"}},
    )

Give it an ``AsyncClient`` instead and the same agent is driven with
``await agent.ainvoke(...)`` and ``agent.astream(...)``. The recording is the
same recording either way: the same positions, the same request hashes, the same
derived keys, so a thread recorded by one can be resumed by the other.

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

What a returned message says about itself
----------------------------------------

Every model answer and every tool result this middleware hands back carries a
marker on ``response_metadata["salvor"]``: ``{"replayed": True, "seq": n,
"run": ...}`` for one read out of the log, ``{"live": True, "seq": n, "run":
...}`` for one this invoke paid for or performed, and ``{"forked": {"at": n,
"thread": ..., "run": ...}}`` once the thread has left its recorded path. Every
message carries one, so no marker is never a fact about a message. A fork is
also said out loud once per invoke, through ``on_fork`` (the default writes one
warning to the ``salvor.langchain`` logger), because the usual cause is a tool
whose result differs between invokes or a graph branching on the clock, and
neither shows up anywhere else.

When something is refused
-------------------------

Everything this middleware refuses is a :class:`SalvorMiddlewareError` carrying
a stable ``code``, and :func:`salvor_error` is how an application gets to one::

    try:
        agent.invoke(ask, {"configurable": {"thread_id": thread}})
    except Exception as error:
        refusal = salvor_error(error)
        if refusal is None:
            raise
        if refusal.code == "lease_held":
            time.sleep(refusal.lapses_in_seconds + 1)

LangChain re-raises a middleware's error exactly as it was raised, so the
refusal usually arrives bare; the helper looks under a wrapper, an implicit
context and an exception group all the same, so one handler covers both shapes.
See :mod:`salvor.langchain.errors` for the codes.

The lease is part of the same story. One driver per thread at a time, so a
second instance invoking a thread the first is still driving is refused
``lease_held`` before it runs anything. An invoke hands its lease back when it
ends, raising or returning, so the next process takes the thread up at once;
while a tool body or a live model call runs, a heartbeat keeps that lease from
lapsing under a driver that never went anywhere.

What the operator has to declare
--------------------------------

A tool's effect class, its input schema, its output schema, its idempotency key
and whether a client may report its own result are the operator's, not this
middleware's. They come from a client-tool
declaration loaded by the server (``salvor serve --client-tool <FILE>``; see
``examples/client-tools/refund-card.toml``). The middleware sends the tool's
name and the model's arguments and nothing else, which is why a tool nobody
declared is refused by name instead of quietly recorded as a harmless read. A
tool declared ``trust_completion = false`` is performed and then stops the
invoke with :class:`ToolNeedsResolution`: its operator settles such a call by
hand, and the next invoke replays what that person recorded.

Installing
----------

LangChain is an optional extra, so ``import salvor`` pulls none of it in::

    pip install 'salvor[langchain]'

Importing this module without it raises :class:`ImportError` naming that line.
"""

from __future__ import annotations

from .async_run_tape import AsyncRunTape
from .current_call import ToolCallContext, current_tool_call
from .errors import SalvorMiddlewareError, ToolNeedsResolution, salvor_error
from .hash import canonical_json, hash_value, is_uuid, run_id_for_thread, sha256_hex
from .request import canonical_request, request_hash
from .run_tape import RunTape
from .tape import Drive, ForkInfo, ModelOutcome, ToolOutcome, TurnPosition

# LangChain itself is reached only from here down. These are the modules that
# import it, so a checkout without the extra installed fails here and is told
# the one command that fixes it, rather than failing somewhere inside a hook
# halfway through an agent run.
try:
    from .finish import FinishedThread, finish_thread
    from .middleware import SalvorMiddleware, salvor_middleware, warn_of_fork
    from .replay_model import ReplayChatModel
except ImportError as missing:  # pragma: no cover - depends on what is installed
    raise ImportError(
        "salvor.langchain needs LangChain, which the plain `salvor` package "
        "does not install. Install the extra: pip install 'salvor[langchain]'. "
        "(The underlying import failed with: {reason})".format(reason=missing)
    ) from missing

__all__ = [
    "AsyncRunTape",
    "Drive",
    "FinishedThread",
    "ForkInfo",
    "ModelOutcome",
    "ReplayChatModel",
    "RunTape",
    "SalvorMiddleware",
    "SalvorMiddlewareError",
    "ToolCallContext",
    "ToolNeedsResolution",
    "ToolOutcome",
    "TurnPosition",
    "canonical_json",
    "canonical_request",
    "current_tool_call",
    "finish_thread",
    "hash_value",
    "is_uuid",
    "request_hash",
    "run_id_for_thread",
    "salvor_error",
    "salvor_middleware",
    "sha256_hex",
    "warn_of_fork",
]
