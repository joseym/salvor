"""``finish_thread``: close a thread's run for good.

A thread's run stays open by default. :class:`~salvor.langchain.SalvorMiddleware`
never appends ``RunCompleted`` on its own, because there is no point in an
agent's life where LangGraph tells this middleware "this thread will never be
invoked again": a thread that looks done today may get one more turn tomorrow.
Deciding that a thread is actually finished is the operator's call, so it gets
its own function rather than something the middleware infers.

Once :func:`finish_thread` has recorded ``RunCompleted``, the run is closed the
way every salvor run is closed: nothing may be appended to it again. An
``agent.invoke`` on that thread meets this when the middleware opens the run,
finds the log already ends at ``RunCompleted``, and raises
:class:`~salvor.langchain.SalvorMiddlewareError` naming the thread rather than
letting the append fail somewhere less legible.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, List, Tuple, Union

from ..async_client import AsyncClient
from ..client import Client
from ..models import Event
from .errors import SalvorMiddlewareError, thread_abandoned_error
from .hash import run_id_for_thread
from .messages import stored_ai_message

__all__ = ["FinishedThread", "finish_thread"]

#: Where a lease this module could not hand back is mentioned; see
#: :data:`salvor.langchain.run_tape.LOG`.
LOG = logging.getLogger("salvor.langchain")

ThreadIdToRunId = Callable[[str], Union[str, Awaitable[str]]]


@dataclass
class FinishedThread:
    """The receipt from finishing a thread: the run it closed and the seq
    ``RunCompleted`` landed at."""

    run_id: str
    seq: int


def finish_thread(
    client: Union[Client, AsyncClient],
    thread_id: str,
    output: Any = None,
    thread_id_to_run_id: ThreadIdToRunId = run_id_for_thread,
) -> Union[FinishedThread, Awaitable[FinishedThread]]:
    """Append ``RunCompleted`` to the run behind ``thread_id``, closing it.

    Takes whichever client the middleware was given, and answers the way that
    client answers everything else::

        finish_thread(Client("http://127.0.0.1:8080"), "order-7781")
        await finish_thread(AsyncClient("http://127.0.0.1:8080"), "order-7781")

    ``thread_id_to_run_id`` defaults to :func:`~salvor.langchain.run_id_for_thread`,
    the same mapping :class:`~salvor.langchain.SalvorMiddleware` uses by
    default; pass the same function an application gave the middleware when it
    overrode the default, so ``finish_thread`` closes the run the middleware
    actually opened. It may answer with an awaitable only under an
    :class:`~salvor.AsyncClient`, which is the only client with anything to
    await it with.

    Refused, appending nothing, as
    :class:`~salvor.langchain.SalvorMiddlewareError` when:

    * the thread has never been invoked (its run holds no events at all);
    * the run is already finished (its log already ends at ``RunCompleted`` or
      ``RunFailed``);
    * the run was abandoned (its log ends at ``RunAbandoned``), which is
      ``thread_abandoned`` rather than ``thread_finished``, and rather than the
      open-intent refusal below: an abandoned run was retired by hand, and the
      dangling write it was retired on top of is not one anybody is going to
      resolve;
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
    if isinstance(client, AsyncClient):
        return _afinish_thread(client, thread_id, output, thread_id_to_run_id)
    return _finish_thread(client, thread_id, output, thread_id_to_run_id)


def _finish_thread(
    client: Client,
    thread_id: str,
    output: Any,
    thread_id_to_run_id: ThreadIdToRunId,
) -> FinishedThread:
    """:func:`finish_thread` over the synchronous client."""
    run_id = thread_id_to_run_id(thread_id)
    if not isinstance(run_id, str):
        raise SalvorMiddlewareError(
            "`thread_id_to_run_id` returned something to await, and "
            "`finish_thread` was given salvor's synchronous `Client`, which "
            "has nothing to await it with. Return the run id itself, or pass "
            "an `AsyncClient`.",
            code="wrong_client",
        )
    driver = client.open_client_run(run_id=run_id)
    try:
        seq, resolved = _completion(driver.log_envelopes, thread_id, run_id, output)
        appended = driver.append(
            [driver.envelope(seq, "RunCompleted", output=resolved)]
        )
    finally:
        # Closing a thread takes the run's lease to do it, and a refusal takes
        # it just as surely as a success. Either way this drive is over, so the
        # lease goes straight back rather than locking the thread out of an
        # operator's next attempt for the rest of the TTL.
        _hand_back(driver)
    return _receipt(run_id, seq, appended)


async def _afinish_thread(
    client: AsyncClient,
    thread_id: str,
    output: Any,
    thread_id_to_run_id: ThreadIdToRunId,
) -> FinishedThread:
    """:func:`finish_thread` over the asynchronous client."""
    run_id = thread_id_to_run_id(thread_id)
    if not isinstance(run_id, str):
        run_id = await run_id
    driver = await client.open_client_run(run_id=run_id)
    try:
        seq, resolved = _completion(driver.log_envelopes, thread_id, run_id, output)
        appended = await driver.append(
            [driver.envelope(seq, "RunCompleted", output=resolved)]
        )
    finally:
        await _ahand_back(driver)
    return _receipt(run_id, seq, appended)


def _completion(
    log: List[Event], thread_id: str, run_id: str, output: Any
) -> Tuple[int, Any]:
    """Where this run's ``RunCompleted`` goes and what it records, or the
    refusal that says why the run cannot be closed at all.

    Every rule :func:`finish_thread` carries is here, decided from the log
    alone, so the two transports differ only in how they read it and write it.
    """
    if not log:
        raise SalvorMiddlewareError(
            "thread `{thread}` (run {run}) has never been invoked, so there is "
            "no run to finish.".format(thread=thread_id, run=run_id),
            code="thread_never_invoked",
        )

    tail = log[-1]
    # An abandoned run is over too, and says so in its own words: an operator
    # retired it, often on top of the very open intent the rule below would
    # otherwise tell this caller to settle. Checked first for that reason.
    if tail.kind == "RunAbandoned":
        raise thread_abandoned_error(thread_id, run_id)
    if tail.kind in ("RunCompleted", "RunFailed"):
        raise SalvorMiddlewareError(
            "thread `{thread}` (run {run}) is already finished.".format(
                thread=thread_id, run=run_id
            ),
            code="thread_finished",
        )
    if tail.kind in ("ModelCallRequested", "ToolCallRequested"):
        what = "a model call" if tail.kind == "ModelCallRequested" else "a tool call"
        raise SalvorMiddlewareError(
            "run {run} (thread `{thread}`) ends at {what} (seq {seq}) that was "
            "requested and never completed. Settle it first (`POST "
            "/v1/runs/{run}/resolve` on the live server, or `salvor resolve "
            "{run} --store <path to the server's store> --output '<json>'`) and "
            "finish the thread again.".format(
                run=run_id, thread=thread_id, what=what, seq=tail.seq
            ),
            code="open_intent",
        )

    resolved = output if output is not None else _last_ai_message_content(log)
    return tail.seq + 1, resolved


def _hand_back(driver: Any) -> None:
    """Give the run's lease back, whatever happened to the close attempt.

    Never raises: a release that fails leaves the lease to lapse the way it
    would have anyway, and this runs while the refusal that says why the thread
    could not be closed is on its way out.
    """
    try:
        driver.release()
    except Exception as refused:  # noqa: BLE001 - the lease lapses either way
        LOG.debug("salvor: run %s kept its lease: %s", driver.run_id, refused)


async def _ahand_back(driver: Any) -> None:
    """:func:`_hand_back`, awaited."""
    try:
        await driver.release()
    except Exception as refused:  # noqa: BLE001 - the lease lapses either way
        LOG.debug("salvor: run %s kept its lease: %s", driver.run_id, refused)


def _receipt(run_id: str, seq: int, appended: List[int]) -> FinishedThread:
    """The receipt, preferring the seq the server reports over the one asked for."""
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
