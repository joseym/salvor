"""The server-sent-event line protocol, and the two things the SDK reads over it.

:class:`SSEDecoder` is fed one line at a time and hands back a frame when a
blank line dispatches one. Pushing rather than pulling is what lets a
synchronous ``for line in ...`` and an asynchronous ``async for line in ...``
share a single parser: the loop differs, the parsing does not.

Above it sit the two readers' rules. :class:`EventTail` is the run stream's
cursor: which sequence to ask for next, which arriving event was already
delivered before a drop, and how long to wait before reconnecting.
:func:`model_step_frame` is the model step's, turning a named frame into a
delta, the assembled completion, or the failure it reports.
"""

from __future__ import annotations

import json
from typing import Any, AsyncIterable, AsyncIterator, Iterable, Iterator, Optional

from ..errors import SalvorStreamError
from ..models import EndFrame, Event
from .driver import ModelStepResult
from .wire import Call, identity


class SSEDecoder:
    """The SSE line protocol, one line at a time.

    ``event:`` and ``data:`` fields accumulate until a blank line dispatches the
    frame; comment lines are skipped. :meth:`push` returns the dispatched
    ``(event, data)`` frame or ``None``. A frame with no ``event:`` field is
    named ``"message"``.
    """

    def __init__(self) -> None:
        self._event: Optional[str] = None
        self._data: list[str] = []

    def push(self, line: str) -> Optional[tuple[str, str]]:
        if line == "":
            return self.flush()
        if line.startswith(":"):
            return None  # a comment / keep-alive line
        field, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field == "event":
            self._event = value
        elif field == "data":
            self._data.append(value)
        # `id:` lines carry the seq; the seq is also inside the data envelope,
        # so there is nothing extra to track here.
        return None

    def flush(self) -> Optional[tuple[str, str]]:
        """Dispatch whatever has accumulated, or ``None`` when nothing has.

        Called on every blank line, and once more at the end of a body that
        stopped without one.
        """
        frame: Optional[tuple[str, str]] = None
        if self._data:
            frame = (self._event or "message", "\n".join(self._data))
        self._event = None
        self._data = []
        return frame


def frames(
    lines: Iterable[str], *, flush_trailing: bool = True
) -> Iterator[tuple[str, str]]:
    """Parse a line iterator into ``(event, data)`` frames.

    ``flush_trailing`` says what a body that stopped without a blank line means.
    The model step's stream takes the partial frame, because the server closes
    the body right after the last one and there is nothing more coming. The run
    event tail does not: a body that stopped early there is a dropped
    connection, and half a frame is not an event, so it is left for the
    reconnect to fetch again from the cursor.
    """
    decoder = SSEDecoder()
    for line in lines:
        frame = decoder.push(line)
        if frame is not None:
            yield frame
    if flush_trailing:
        trailing = decoder.flush()
        if trailing is not None:
            yield trailing


async def aframes(
    lines: AsyncIterable[str], *, flush_trailing: bool = True
) -> AsyncIterator[tuple[str, str]]:
    """:func:`frames` over an async line iterator, feeding the same decoder.

    The two differ in how a line arrives and in nothing else, which is the whole
    reason :class:`SSEDecoder` is pushed to rather than pulled from.
    """
    decoder = SSEDecoder()
    async for line in lines:
        frame = decoder.push(line)
        if frame is not None:
            yield frame
    if flush_trailing:
        trailing = decoder.flush()
        if trailing is not None:
            yield trailing


# -- the run event tail ---------------------------------------------------------


def events_stream(run_id: str, from_seq: int) -> Call:
    """The request for one connection to a run's event stream. The body is a
    frame stream rather than a JSON document, so the parse is never used; the
    reader takes the path and params and reads the frames itself."""
    return Call(
        "GET",
        f"/v1/runs/{run_id}/events",
        parse=identity,
        params={"from_seq": from_seq},
    )


def event_frame(frame: tuple[str, str]) -> tuple[str, dict[str, Any]]:
    """One raw stream frame as ``(kind, obj)``, where ``kind`` is ``"event"``
    (an envelope) or ``"end"`` (the terminal frame)."""
    name, data = frame
    return ("end" if name == "end" else "event", json.loads(data))


class EventTail:
    """The cursor and retry budget one :meth:`stream_events` call carries.

    The log is gap-free and duplicate-free by construction, so one "next
    sequence" number is enough to resume a dropped connection, and one
    "last seen" number is enough to drop the events that arrived just before the
    drop. Forward progress refreshes the retry budget, so a stream that keeps
    delivering never exhausts it.
    """

    def __init__(self, run_id: str, from_seq: int, max_retries: int) -> None:
        self.run_id = run_id
        self.next_seq = from_seq
        self.end: Optional[EndFrame] = None
        self._max_retries = max_retries
        self._last_seen: Optional[int] = None
        self._attempts = 0

    def accept(self, kind: str, obj: dict[str, Any]) -> tuple[str, Any]:
        """Take one frame, returning ``("event", Event)``, ``("end", EndFrame)``
        or ``("skip", None)`` for an event already delivered before a drop."""
        if kind == "end":
            self.end = EndFrame.from_json(obj)
            return ("end", self.end)
        event = Event.from_envelope(obj)
        if self._last_seen is not None and event.seq <= self._last_seen:
            return ("skip", None)  # already delivered before a drop; skip it
        self._last_seen = event.seq
        self.next_seq = event.seq + 1
        self._attempts = 0  # forward progress refreshes the retry budget
        return ("event", event)

    def backoff(self) -> float:
        """How long to wait before reconnecting, after a connection that ended
        without the terminal frame. Raises
        :class:`~salvor.errors.SalvorStreamError` once the budget is spent."""
        self._attempts += 1
        if self._attempts > self._max_retries:
            raise SalvorStreamError(
                f"event stream for run {self.run_id} dropped and did not resume "
                f"after {self._max_retries} attempts"
            )
        return min(0.25 * self._attempts, 2.0)


# -- the model step ticker -------------------------------------------------------


def model_step_frame(run_id: str, frame: tuple[str, str]) -> tuple[str, Any]:
    """One model-step frame as ``("delta", payload)``, ``("complete", result)``
    or ``("ignore", None)``. An ``error`` frame raises
    :class:`~salvor.errors.SalvorStreamError`: a mid-stream provider failure is
    the caller's, not a value to hand back."""
    name, data = frame
    if name == "delta":
        return ("delta", json.loads(data))
    if name == "complete":
        return ("complete", ModelStepResult.from_json(json.loads(data)))
    if name == "error":
        message = json.loads(data).get("message", "model step failed")
        raise SalvorStreamError(f"model step for run {run_id}: {message}")
    return ("ignore", None)
