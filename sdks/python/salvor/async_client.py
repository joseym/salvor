"""The asynchronous transport for the Salvor control plane.

:class:`AsyncClient` is :class:`salvor.Client` awaited. Every method carries the
same name, takes the same arguments and returns the same types, because both
read their whole surface from :mod:`salvor._core`: the path, the body and the
decode are described there once, and a transport only sends. Read
:class:`salvor.Client` for what each method means; the docstrings here say only
what the await changes.

Two methods are not coroutines, on purpose. :meth:`AsyncClient.stream_events`
hands back an async iterator, so it reads ``async for event in
client.stream_events(run_id)`` rather than being awaited first.
:meth:`AsyncClient.open_client_run` IS awaited, because opening a run is a
request.
"""

from __future__ import annotations

import asyncio
from typing import Any, AsyncIterator, Optional, Union

import httpx

from ._core import api, wire
from ._core.sse import EventTail, aframes, event_frame, events_stream
from ._core.wire import Call
from .errors import SalvorAPIError
from .graph import Graph
from .models import (
    AbandonResult,
    ClientToolDecl,
    EndFrame,
    Event,
    ForkPreview,
    ForkResult,
    ForksIndex,
    GraphProjection,
    GraphSubmitted,
    GraphSummary,
    GraphValidation,
    ReplayState,
    ResumeResult,
    RunState,
    RunSummary,
    StoredGraph,
)

__all__ = ["AsyncClient", "AsyncEventStream"]


class AsyncEventStream:
    """An async iterator of :class:`~salvor.models.Event` from a run's stream.

    The async twin of :class:`salvor.EventStream`, with the same contract:
    ``async for`` it to receive each recorded event in order; iteration stops at
    the terminal ``end`` frame, and :attr:`end` then holds the
    :class:`~salvor.models.EndFrame` with the run's resting status.
    """

    def __init__(self) -> None:
        self.end: Optional[EndFrame] = None
        self._gen: Optional[AsyncIterator[Event]] = None

    def __aiter__(self) -> "AsyncEventStream":
        return self

    async def __anext__(self) -> Event:
        assert self._gen is not None
        return await self._gen.__anext__()


class AsyncClient:
    """An asynchronous client for one Salvor control plane.

    Takes the same arguments as :class:`salvor.Client` and means the same thing
    by each of them:

        async with AsyncClient("http://127.0.0.1:8080") as client:
            agent = await client.register_agent(open("agent.toml").read())
            run_id = await client.start_run(agent, {"question": "..."})
            stream = client.stream_events(run_id)
            async for event in stream:
                print(event.seq, event.kind)
            print(stream.end.status.state)
    """

    def __init__(
        self,
        base_url: str,
        token: Optional[str] = None,
        *,
        timeout: float = 30.0,
        max_stream_retries: int = 5,
    ) -> None:
        self.base_url, headers = api.connection(base_url, token)
        self._token = token
        self._max_stream_retries = max_stream_retries
        self._http = httpx.AsyncClient(
            base_url=self.base_url, headers=headers, timeout=timeout
        )
        # The event stream needs its own long-lived timeout: the read side of a
        # live tail waits between events, so the read timeout is disabled while
        # connect/write stay bounded.
        self._stream_timeout = httpx.Timeout(timeout, read=None)
        #: The last drive token this client saw for a run it opened, forgotten
        #: when that run's lease is released. See
        #: :attr:`salvor.Client._client_run_tokens`; the rule is identical.
        self._client_run_tokens: dict[str, str] = {}

    async def close(self) -> None:
        """Close the underlying HTTP connection pool."""
        await self._http.aclose()

    #: httpx spells this ``aclose``; both names close the same pool.
    aclose = close

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    # -- agents ---------------------------------------------------------------

    async def register_agent(self, definition: Union[str, dict[str, Any]]) -> str:
        """Await :meth:`salvor.Client.register_agent`."""
        return await self._send(api.register_agent(definition))

    async def list_agents(self) -> list[str]:
        """Await :meth:`salvor.Client.list_agents`."""
        return await self._send(api.list_agents())

    async def get_agent(self, agent_hash: str) -> dict[str, Any]:
        """Await :meth:`salvor.Client.get_agent`."""
        return await self._send(api.get_agent(agent_hash))

    # -- runs -----------------------------------------------------------------

    async def start_run(
        self,
        agent: str,
        input: Any = None,
        *,
        run_id: Optional[str] = None,
        labels: Optional[dict[str, str]] = None,
    ) -> str:
        """Await :meth:`salvor.Client.start_run`."""
        return await self._send(api.start_run(agent, input, run_id, labels))

    async def list_runs(self) -> list[RunSummary]:
        """Await :meth:`salvor.Client.list_runs`."""
        return await self._send(api.list_runs())

    async def get_run(self, run_id: str) -> RunState:
        """Await :meth:`salvor.Client.get_run`."""
        return await self._send(api.get_run(run_id))

    async def replay(self, run_id: str) -> ReplayState:
        """Await :meth:`salvor.Client.replay`."""
        return await self._send(api.replay(run_id))

    async def resume(self, run_id: str, input: Any = None) -> ResumeResult:
        """Await :meth:`salvor.Client.resume`."""
        return await self._send(api.resume(run_id, input))

    async def resolve(self, run_id: str, output: Any) -> RunState:
        """Await :meth:`salvor.Client.resolve`."""
        return await self._send(api.resolve(run_id, output))

    async def abandon(self, run_id: str, reason: Optional[str] = None) -> AbandonResult:
        """Await :meth:`salvor.Client.abandon`."""
        return await self._send(api.abandon(run_id, reason))

    # -- graphs ---------------------------------------------------------------

    async def submit_graph(
        self, document: Union[Graph, dict[str, Any]]
    ) -> GraphSubmitted:
        """Await :meth:`salvor.Client.submit_graph`."""
        return await self._send(api.submit_graph(document))

    async def list_graphs(self) -> list[GraphSummary]:
        """Await :meth:`salvor.Client.list_graphs`."""
        return await self._send(api.list_graphs())

    async def get_graph(self, graph_hash: str) -> StoredGraph:
        """Await :meth:`salvor.Client.get_graph`."""
        return await self._send(api.get_graph(graph_hash))

    async def validate_graph(
        self, document: Union[Graph, dict[str, Any]]
    ) -> GraphValidation:
        """Await :meth:`salvor.Client.validate_graph`."""
        return await self._send(api.validate_graph(document))

    async def start_graph_run(
        self,
        graph_hash: str,
        input: Any = None,
        *,
        labels: Optional[dict[str, str]] = None,
    ) -> str:
        """Await :meth:`salvor.Client.start_graph_run`."""
        return await self._send(api.start_graph_run(graph_hash, input, labels))

    async def get_run_graph(self, run_id: str) -> GraphProjection:
        """Await :meth:`salvor.Client.get_run_graph`."""
        return await self._send(api.get_run_graph(run_id))

    async def fork_run(
        self,
        run_id: str,
        from_node: str,
        *,
        acknowledge_writes: Optional[list[int]] = None,
    ) -> ForkResult:
        """Await :meth:`salvor.Client.fork_run`."""
        return await self._send(
            api.fork_run(run_id, from_node, acknowledge_writes, dry_run=False)
        )

    async def preview_fork(
        self,
        run_id: str,
        from_node: str,
        *,
        acknowledge_writes: Optional[list[int]] = None,
    ) -> ForkPreview:
        """Await :meth:`salvor.Client.preview_fork`."""
        return await self._send(
            api.fork_run(run_id, from_node, acknowledge_writes, dry_run=True)
        )

    async def list_forks(self, run_id: str) -> ForksIndex:
        """Await :meth:`salvor.Client.list_forks`."""
        return await self._send(api.list_forks(run_id))

    # -- client-performed tools -------------------------------------------------

    async def list_client_tools(self) -> list[ClientToolDecl]:
        """Await :meth:`salvor.Client.list_client_tools`."""
        return await self._send(api.list_client_tools())

    # -- event stream ---------------------------------------------------------

    def stream_events(
        self, run_id: str, from_seq: Optional[int] = None
    ) -> AsyncEventStream:
        """:meth:`salvor.Client.stream_events`, as an async iterator.

        Not a coroutine: it hands back the stream to ``async for``, and the
        first connection is opened when iteration starts. Reconnection, the
        cursor and the duplicate-free merge across a drop are the same rules the
        synchronous stream follows, because they are the same code.
        """
        stream = AsyncEventStream()
        stream._gen = self._events(run_id, from_seq or 0, stream)
        return stream

    async def _events(
        self, run_id: str, from_seq: int, stream: AsyncEventStream
    ) -> AsyncIterator[Event]:
        tail = EventTail(run_id, from_seq, self._max_stream_retries)
        while True:
            try:
                async for kind, obj in self._read_frames(run_id, tail.next_seq):
                    what, value = tail.accept(kind, obj)
                    if what == "event":
                        yield value
                    elif what == "end":
                        stream.end = value
                        return
                # The stream closed with no end frame: the connection dropped
                # mid-tail. Fall through to reconnect from the cursor.
            except (httpx.TransportError, httpx.RemoteProtocolError):
                pass  # transient drop; reconnect below
            await asyncio.sleep(tail.backoff())

    async def _read_frames(
        self, run_id: str, from_seq: int
    ) -> AsyncIterator[tuple[str, dict[str, Any]]]:
        """Open one connection and yield ``(kind, obj)`` per server-sent frame,
        the async twin of :meth:`salvor.Client._read_frames` over the same
        :class:`salvor._core.sse.SSEDecoder`."""
        call = events_stream(run_id, from_seq)
        async with self._http.stream(
            call.method,
            call.path,
            params=call.params,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                raise self._error(resp.status_code, await resp.aread())
            async for frame in aframes(resp.aiter_lines(), flush_trailing=False):
                yield event_frame(frame)

    # -- helpers --------------------------------------------------------------

    async def _send(self, call: Call) -> Any:
        """Perform one described call: send it, decode the answer, parse it."""
        resp = await self._http.request(
            call.method, call.path, **wire.request_kwargs(call)
        )
        return call.parse(wire.decode_json(resp.status_code, resp.content))

    def _error(self, status: int, body: bytes) -> SalvorAPIError:
        return wire.error(status, body)

    async def open_client_run(
        self,
        *,
        agent: Optional[str] = None,
        input: Any = None,
        run_id: Optional[str] = None,
        record_prompts: bool = False,
        drive_token: Optional[str] = None,
    ) -> "AsyncClientRunDriver":
        """Open or re-open a client-driven run over this client's connection.

        Awaited, unlike :meth:`salvor.Client.open_client_run`, because opening a
        run is a request. Returns a
        :class:`~salvor.async_client_runs.AsyncClientRunDriver` sharing this
        client's HTTP pool and auth, so it is closed when this client is.

        ``drive_token`` re-opens under a lease this process already holds; left
        unset, this client fills in the last token it remembers for
        ``run_id``, the same auto-fill :meth:`salvor.Client.open_client_run`
        does. See there for the full rule.
        """
        from .async_client_runs import AsyncClientRunDriver

        if drive_token is None:
            drive_token = self._client_run_tokens.get(run_id) if run_id else None
        driver = await AsyncClientRunDriver._open_over(
            self._http,
            owns_http=False,
            stream_timeout=self._stream_timeout,
            agent=agent,
            input=input,
            run_id=run_id,
            record_prompts=record_prompts,
            drive_token=drive_token,
            on_release=self._forget_drive_token,
        )
        self._client_run_tokens[driver.run_id] = driver.drive_token
        return driver

    def _forget_drive_token(self, run_id: str) -> None:
        """Stop remembering a run's drive token once its lease is handed back.
        See :meth:`salvor.Client._forget_drive_token`; the rule is identical."""
        self._client_run_tokens.pop(run_id, None)
