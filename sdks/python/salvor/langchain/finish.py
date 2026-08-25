"""``finish_thread``: close a thread's run for good.

A thread's run stays open by default. :class:`~salvor.langchain.SalvorMiddleware`
never appends ``RunCompleted`` on its own, because there is no point in an
agent's life where LangGraph tells this middleware "this thread will never be
invoked again": a thread that looks done today may get one more turn tomorrow.
Deciding that a thread is actually finished is the operator's call, so it gets
its own function rather than something the middleware infers.

Once :func:`finish_thread` has recorded ``RunCompleted``, the run is closed the
way every salvor run is closed: nothing may be appended to it again. An
``agent.ainvoke`` on that thread meets this when the middleware opens the run,
finds the log already ends at ``RunCompleted``, and raises
:class:`~salvor.langchain.SalvorMiddlewareError` naming the thread rather than
letting the append fail somewhere less legible.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Awaitable, Callable, List, Union

from ..async_client import AsyncClient
from ..models import Event
from .errors import SalvorMiddlewareError
from .hash import run_id_for_thread
from .messages import stored_ai_message

__all__ = ["FinishedThread", "finish_thread"]


@dataclass
class FinishedThread:
    """The receipt from finishing a thread: the run it closed and the seq
    ``RunCompleted`` landed at."""

    run_id: str
    seq: int


async def finish_thread(
    client: AsyncClient,
    thread_id: str,
    output: Any = None,
    thread_id_to_run_id: Callable[
        [str], Union[str, Awaitable[str]]
    ] = run_id_for_thread,
) -> FinishedThread:
    """Append ``RunCompleted`` to the run behind ``thread_id``, closing it.

    ``thread_id_to_run_id`` defaults to :func:`~salvor.langchain.run_id_for_thread`,
    the same mapping :class:`~salvor.langchain.SalvorMiddleware` uses by
    default; pass the same function an application gave the middleware when it
    overrode the default, so ``finish_thread`` closes the run the middleware
    actually opened.

    Refused, appending nothing, as
    :class:`~salvor.langchain.SalvorMiddlewareError` when:

    * the thread has never been invoked (its run holds no events at all);
    * the run is already finished (its log already ends at ``RunCompleted`` or
      ``RunFailed``);
    * the log ends at an open intent: a model or tool call salvor recorded as
      requested but never recorded as completed. That call has to be settled
      first (``salvor run resolve <run> <output>``, or the resolve endpoint),
      because a ``RunCompleted`` appended past it would silently abandon
      whatever that call was doing.

    ``output`` defaults to the content of the last recorded AI message, read
    back from the run's own log the same way a replayed model call is; when the
    log holds no such message, or holds one this SDK cannot read back, the
    default is ``None`` rather than a raised error, because a thread is worth
    closing even when its last answer cannot be recovered from the log. Python
    has no ``undefined``, so passing ``output=None`` explicitly asks for that
    same default rather than forcing a null; pass the value you want recorded
    when you want a particular one.
    """
    run_id = thread_id_to_run_id(thread_id)
    if not isinstance(run_id, str):
        run_id = await run_id
    driver = await client.open_client_run(run_id=run_id)
    log = driver.log_envelopes

    if not log:
        raise SalvorMiddlewareError(
            "thread `{thread}` (run {run}) has never been invoked, so there is "
            "no run to finish.".format(thread=thread_id, run=run_id)
        )

    tail = log[-1]
    if tail.kind in ("RunCompleted", "RunFailed"):
        raise SalvorMiddlewareError(
            "thread `{thread}` (run {run}) is already finished.".format(
                thread=thread_id, run=run_id
            )
        )
    if tail.kind in ("ModelCallRequested", "ToolCallRequested"):
        what = "a model call" if tail.kind == "ModelCallRequested" else "a tool call"
        raise SalvorMiddlewareError(
            "run {run} (thread `{thread}`) ends at {what} (seq {seq}) that was "
            "requested and never completed. Settle it first (`salvor run resolve "
            "{run} <output>`, or the resolve endpoint) and finish the thread "
            "again.".format(run=run_id, thread=thread_id, what=what, seq=tail.seq)
        )

    resolved = output if output is not None else _last_ai_message_content(log)
    seq = tail.seq + 1
    appended = await driver.append(
        [driver.envelope(seq, "RunCompleted", output=resolved)]
    )
    return FinishedThread(run_id=run_id, seq=appended[0] if appended else seq)


def _last_ai_message_content(log: List[Event]) -> Any:
    """The content of the most recently recorded AI message, or ``None`` when
    the log holds none or holds one shaped some other way than the LangChain
    stored form this middleware writes."""
    for event in reversed(log):
        if event.kind != "ModelCallCompleted":
            continue
        try:
            return stored_ai_message(event.payload.get("response")).content
        except Exception:
            return None
    return None
