"""The asynchronous client-driven run driver.

:class:`AsyncClientRunDriver` is :class:`salvor.ClientRunDriver` awaited. Same
method names, same arguments, same return types, same refusals, because the wire
shapes and the durable-timer arithmetic both come from
:mod:`salvor._core.driver` rather than from either driver. Read
:class:`salvor.ClientRunDriver` for what each method means and which rule it
carries; the docstrings here say only what the await changes.

    from salvor import AsyncClient

    async with AsyncClient("http://127.0.0.1:8080") as client:
        run = await client.open_client_run()
        await run.append([run.envelope(0, "RunStarted",
                                       agent_def_hash=agent, input=task)])
        result = await run.model_step(1, request)
        await run.append([run.envelope(3, "RunCompleted", output=answer)])

Two members are not coroutines. :meth:`~AsyncClientRunDriver.envelope` builds a
dict and touches nothing, and
:meth:`~AsyncClientRunDriver.model_step_stream` hands back an async iterator to
``async for``. Everything else, opening and closing included, is awaited.
"""

from __future__ import annotations

from datetime import datetime, timedelta
from typing import Any, AsyncIterator, Callable, Optional

import httpx

from ._core import api, driver as rules, wire
from ._core.driver import (
    ClientModelIntentResult,
    ClientToolIntentResult,
    ModelStepResult,
    Waking,
    utc_now as _utc_now,
)
from ._core.sse import aframes as _asse_frames, model_step_frame
from ._core.wire import Call
from .models import Event

__all__ = ["AsyncClientRunDriver", "AsyncModelStepStream"]


class AsyncModelStepStream:
    """The live ticker of a streaming model step, as an async iterator.

    The async twin of :class:`salvor.ModelStepStream`, with the same contract:
    ``async for`` it to receive each ticker delta, and read the assembled
    :class:`~salvor.ModelStepResult` from :attr:`completion` once iteration
    stops. A mid-stream provider failure raises
    :class:`~salvor.errors.SalvorStreamError`.
    """

    def __init__(self) -> None:
        self.completion: Optional[ModelStepResult] = None
        self._gen: Optional[AsyncIterator[dict[str, Any]]] = None

    def __aiter__(self) -> "AsyncModelStepStream":
        return self

    async def __anext__(self) -> dict[str, Any]:
        assert self._gen is not None
        return await self._gen.__anext__()


class AsyncClientRunDriver:
    """Drives one client-driven run against a Salvor control plane, awaited.

    Open one with :meth:`salvor.AsyncClient.open_client_run` or :meth:`open`,
    both of which are coroutines. The driver holds the same state its
    synchronous twin does: the run id, the current ``drive_token``, the log
    returned when re-opening, and the deadline this drive is parked on.
    """

    def __init__(
        self,
        http: httpx.AsyncClient,
        *,
        run_id: str,
        drive_token: str,
        log: list[Event],
        owns_http: bool,
        stream_timeout: httpx.Timeout,
        on_release: Optional[Callable[[str], None]] = None,
    ) -> None:
        self._http = http
        self._owns_http = owns_http
        self._stream_timeout = stream_timeout
        # Told the run id when this driver hands the lease back; see
        # :meth:`salvor.ClientRunDriver.release`.
        self._on_release = on_release
        self.run_id = run_id
        self.drive_token = drive_token
        #: The envelopes returned when this run was opened. Empty for a fresh
        #: run; the full recorded log for a re-open, ready to rebuild a cursor.
        self.log_envelopes = log
        #: The clock the durable-timer methods read, returning a timezone-aware
        #: datetime. A plain callable, not a coroutine: reading a clock waits for
        #: nothing, and a test replaces it the same way it does on the
        #: synchronous driver.
        self.clock: Callable[[], datetime] = _utc_now
        # The deadline set earlier in THIS drive, live or replayed.
        self._sleeping_until: Optional[datetime] = None

    # -- construction ---------------------------------------------------------

    @classmethod
    async def open(
        cls,
        base_url: str,
        *,
        agent: Optional[str] = None,
        input: Any = None,
        run_id: Optional[str] = None,
        record_prompts: bool = False,
        drive_token: Optional[str] = None,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> "AsyncClientRunDriver":
        """Await :meth:`salvor.ClientRunDriver.open`: a fresh client-driven run,
        or a re-open of an existing one.

        Passing the held lease's own ``drive_token`` re-opens under the SAME
        token instead of raising :class:`~salvor.errors.LeaseHeldError`; see
        :meth:`salvor.ClientRunDriver.open` for the full rule.

        The driver owns its own HTTP connection; close it with :meth:`close`.
        """
        base, headers = api.connection(base_url, token)
        http = httpx.AsyncClient(base_url=base, headers=headers, timeout=timeout)
        return await cls._open_over(
            http,
            owns_http=True,
            stream_timeout=httpx.Timeout(timeout, read=None),
            agent=agent,
            input=input,
            run_id=run_id,
            record_prompts=record_prompts,
            drive_token=drive_token,
        )

    @classmethod
    async def _open_over(
        cls,
        http: httpx.AsyncClient,
        *,
        owns_http: bool,
        stream_timeout: httpx.Timeout,
        agent: Optional[str],
        input: Any,
        run_id: Optional[str],
        record_prompts: bool,
        drive_token: Optional[str] = None,
        on_release: Optional[Callable[[str], None]] = None,
    ) -> "AsyncClientRunDriver":
        call = rules.open_run(agent, input, run_id, record_prompts, drive_token)
        resp = await http.request(call.method, call.path, **wire.request_kwargs(call))
        opened = call.parse(wire.decode_json(resp.status_code, resp.content))
        return cls(
            http,
            run_id=opened.run_id,
            drive_token=opened.drive_token,
            log=opened.log,
            owns_http=owns_http,
            stream_timeout=stream_timeout,
            on_release=on_release,
        )

    async def close(self) -> None:
        """Close the HTTP connection, if this driver owns it."""
        if self._owns_http:
            await self._http.aclose()

    #: httpx spells this ``aclose``; both names close the same connection.
    aclose = close

    async def __aenter__(self) -> "AsyncClientRunDriver":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    # -- the lease ------------------------------------------------------------

    async def release(self) -> bool:
        """Await :meth:`salvor.ClientRunDriver.release`: hand the lease back so
        the next open takes the run at once, and let the client that opened it
        forget the token."""
        try:
            return await self._send(rules.release(self.run_id, self.drive_token))
        finally:
            if self._on_release is not None:
                self._on_release(self.run_id)

    async def heartbeat(self) -> int:
        """Await :meth:`salvor.ClientRunDriver.heartbeat`: say "still here"
        without driving, and hear the whole seconds the lease has left."""
        return await self._send(rules.heartbeat(self.run_id, self.drive_token))

    # -- building envelopes ---------------------------------------------------

    def envelope(self, seq: int, kind: str, **payload: Any) -> dict[str, Any]:
        """:meth:`salvor.ClientRunDriver.envelope`. Not a coroutine: it builds a
        dict and touches nothing."""
        return rules.envelope(self.run_id, seq, kind, **payload)

    # -- log ------------------------------------------------------------------

    async def log(self, from_seq: int = 0) -> list[Event]:
        """Await :meth:`salvor.ClientRunDriver.log`."""
        return await self._send(rules.read_log(self.run_id, from_seq))

    # -- generic append -------------------------------------------------------

    async def append(self, events: list[dict[str, Any]]) -> list[int]:
        """Await :meth:`salvor.ClientRunDriver.append`."""
        return await self._send(rules.append(self.run_id, self.drive_token, events))

    # -- model step -----------------------------------------------------------

    async def model_step(self, seq: int, request: Any) -> ModelStepResult:
        """Await :meth:`salvor.ClientRunDriver.model_step`."""
        return await self._send(
            rules.model_step(self.run_id, self.drive_token, seq, request)
        )

    def model_step_stream(self, seq: int, request: Any) -> AsyncModelStepStream:
        """:meth:`salvor.ClientRunDriver.model_step_stream`, as an async iterator.

        Not a coroutine: it hands back the stream to ``async for``, and the
        request is sent when iteration starts.
        """
        stream = AsyncModelStepStream()
        stream._gen = self._stream_model(seq, request, stream)
        return stream

    async def _stream_model(
        self, seq: int, request: Any, stream: AsyncModelStepStream
    ) -> AsyncIterator[dict[str, Any]]:
        call = rules.model_step_stream(self.run_id, self.drive_token, seq, request)
        async with self._http.stream(
            call.method,
            call.path,
            json=call.json_body,
            headers=call.headers,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                raise wire.error(resp.status_code, await resp.aread())
            async for frame in _asse_frames(resp.aiter_lines()):
                what, value = model_step_frame(self.run_id, frame)
                if what == "delta":
                    yield value
                elif what == "complete":
                    stream.completion = value
                    return

    # -- tool step ------------------------------------------------------------

    async def tool_step(
        self,
        seq: int,
        tool: str,
        input: Any,
        idempotency_key: Optional[str] = None,
    ) -> Any:
        """Await :meth:`salvor.ClientRunDriver.tool_step`."""
        return await self._send(
            rules.tool_step(
                self.run_id, self.drive_token, seq, tool, input, idempotency_key
            )
        )

    # -- client-performed tool calls -------------------------------------------

    async def client_tool_intent(
        self, seq: int, tool: str, input: Any
    ) -> ClientToolIntentResult:
        """Await :meth:`salvor.ClientRunDriver.client_tool_intent`."""
        return await self._send(
            rules.client_tool_intent(self.run_id, self.drive_token, seq, tool, input)
        )

    async def client_tool_completion(self, seq: int, output: Any) -> None:
        """Await :meth:`salvor.ClientRunDriver.client_tool_completion`."""
        await self._send(
            rules.client_tool_completion(self.run_id, self.drive_token, seq, output)
        )

    async def client_tool_failure(
        self, seq: int, message: str, kind: str = "handler"
    ) -> None:
        """Await :meth:`salvor.ClientRunDriver.client_tool_failure`."""
        await self._send(
            rules.client_tool_failure(
                self.run_id, self.drive_token, seq, message, kind
            )
        )

    # -- client-performed model calls -------------------------------------------

    async def client_model_intent(
        self, seq: int, request_hash: str, request_body: Any = None
    ) -> ClientModelIntentResult:
        """Await :meth:`salvor.ClientRunDriver.client_model_intent`."""
        return await self._send(
            rules.client_model_intent(
                self.run_id, self.drive_token, seq, request_hash, request_body
            )
        )

    async def client_model_completion(
        self, seq: int, response: Any, usage: dict[str, int]
    ) -> None:
        """Await :meth:`salvor.ClientRunDriver.client_model_completion`."""
        await self._send(
            rules.client_model_completion(
                self.run_id, self.drive_token, seq, response, usage
            )
        )

    # -- durable timers --------------------------------------------------------

    async def now(self, seq: int) -> datetime:
        """Await :meth:`salvor.ClientRunDriver.now`."""
        return await self._timed(
            rules.now_step(
                self.run_id,
                seq,
                await self._event_at(seq),
                self.clock,
                self._sleeping_until,
            )
        )

    async def sleep_until(self, seq: int, wake_at: datetime) -> datetime:
        """Await :meth:`salvor.ClientRunDriver.sleep_until`."""
        return await self._timed(
            rules.sleep_step(self.run_id, seq, wake_at, await self._event_at(seq))
        )

    async def sleep_for(self, seq: int, duration: timedelta) -> datetime:
        """Await :meth:`salvor.ClientRunDriver.sleep_for`."""
        return await self.sleep_until(seq + 1, await self.now(seq) + duration)

    async def await_wake(self, seq: int) -> Waking:
        """Await :meth:`salvor.ClientRunDriver.await_wake`."""
        return await self._timed(
            rules.wake_step(
                self.run_id,
                seq,
                await self._event_at(seq),
                self.clock,
                self._sleeping_until,
            )
        )

    async def _timed(self, step: rules.TimerStep) -> Any:
        """Carry out one durable-timer verdict: append what it asks for, then
        adopt the deadline it leaves behind."""
        if step.events:
            await self.append(step.events)
        self._sleeping_until = step.sleeping_until
        return step.result

    async def _event_at(self, seq: int) -> Optional[Event]:
        """The recorded event at ``seq``, or ``None`` when the log has not
        reached that position yet. One log read, deliberately."""
        return rules.event_at(await self.log(from_seq=seq), seq)

    # -- resolve --------------------------------------------------------------

    async def resolve(self, output: Any) -> None:
        """Await :meth:`salvor.ClientRunDriver.resolve`."""
        await self._send(rules.resolve(self.run_id, self.drive_token, output))

    # -- helpers --------------------------------------------------------------

    def _lease(self) -> dict[str, str]:
        return rules.lease(self.drive_token)

    async def _send(self, call: Call) -> Any:
        """Perform one described call: send it, decode the answer, parse it."""
        resp = await self._http.request(
            call.method, call.path, **wire.request_kwargs(call)
        )
        return call.parse(wire.decode_json(resp.status_code, resp.content))
