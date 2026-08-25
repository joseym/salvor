"""``current_tool_call()``: what a tool body running under ``wrap_tool_call``
was recorded with.

The middleware derives a call's idempotency key before the tool body ever runs
(:meth:`~salvor.AsyncClientRunDriver.client_tool_intent` returns it as part of
opening the intent), but nothing about ``wrap_tool_call`` hands that key to the
tool itself: LangChain calls a tool with its arguments and nothing else. A tool
that talks to its own provider (a payments API, an email sender, anything that
takes its own idempotency token) needs that key to hand onward, so this module
makes it reachable from inside the tool body without changing the tool's
signature: :func:`run_with_tool_call` sets a :mod:`contextvars` variable around
the live call, and :func:`current_tool_call` reads it back.

``key`` is what salvor recorded for this call, not a suggestion: hand it to the
tool's own provider as the provider's idempotency token, the same way the
client-tool intent's key is meant to be used. A retried write then presents the
key the first attempt used, and the provider collapses the duplicate.

A ``ContextVar`` is per-task, and asyncio copies the current context into every
task and into every ``run_in_executor`` thread LangChain dispatches a
synchronous tool into. So a tool body reads its own call's key whether it is a
coroutine or a plain function, and two tool calls in flight never read each
other's, even though the middleware serialises them anyway.
"""

from __future__ import annotations

from contextvars import ContextVar
from dataclasses import dataclass
from typing import Awaitable, Callable, Optional, TypeVar

__all__ = ["ToolCallContext", "current_tool_call", "run_with_tool_call"]


@dataclass(frozen=True)
class ToolCallContext:
    """What :func:`current_tool_call` returns from inside a tool body."""

    #: The idempotency key salvor derived for this call, from ``(run, seq, tool)``.
    key: str
    #: The log position the call's intent landed at.
    seq: int
    #: The run this call belongs to.
    run_id: str
    #: The tool's name, as the model invoked it.
    tool: str


_CURRENT = ContextVar(
    "salvor_current_tool_call", default=None
)  # type: ContextVar[Optional[ToolCallContext]]

T = TypeVar("T")


def current_tool_call() -> Optional[ToolCallContext]:
    """The idempotency key, seq, run id and tool name salvor recorded for the
    call this tool body is running inside of, or ``None`` when called outside a
    ``wrap_tool_call`` invocation."""
    return _CURRENT.get()


async def run_with_tool_call(
    context: ToolCallContext, body: Callable[[], Awaitable[T]]
) -> T:
    """Await ``body`` with ``context`` reachable from :func:`current_tool_call`
    anywhere it awaits into, including inside the tool body a handler
    eventually calls."""
    token = _CURRENT.set(context)
    try:
        return await body()
    finally:
        _CURRENT.reset(token)
