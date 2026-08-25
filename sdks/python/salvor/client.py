"""A thin synchronous client for the Salvor control plane.

The client holds no durability logic and no run state. It submits definitions
and inputs, reads events, and decodes the error envelope into exceptions. Every
guarantee, exact replay, crash-safe resume, the write-ahead reconciliation
rule, lives in the one Rust process the client talks to.

It holds no protocol either. Which path each method calls, what goes in the
body, and how the answer decodes all live in :mod:`salvor._core`, which knows
nothing about sockets; this module is the half that sends. :class:`salvor.AsyncClient`
is the same surface over ``httpx.AsyncClient``, reading the same core, which is
why the two can carry the same behaviour without carrying it twice.
"""

from __future__ import annotations

import time
from typing import Any, Iterator, Optional, Union

import httpx

from ._core import api, wire
from ._core.sse import EventTail, event_frame, events_stream, frames as sse_frames
from ._core.wire import Call
from .errors import SalvorAPIError
from .graph import Graph
from .models import (
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


class EventStream:
    """An iterator of :class:`~salvor.models.Event` from a run's stream.

    Iterate it to receive each recorded event in order; iteration stops at the
    terminal ``end`` frame. After it stops, :attr:`end` holds the
    :class:`~salvor.models.EndFrame` with the run's resting status.
    """

    def __init__(self) -> None:
        self.end: Optional[EndFrame] = None
        self._gen: Optional[Iterator[Event]] = None

    def __iter__(self) -> "EventStream":
        return self

    def __next__(self) -> Event:
        assert self._gen is not None
        return next(self._gen)


class Client:
    """A synchronous client for one Salvor control plane.

    Args:
        base_url: The control plane's base URL, for example
            ``http://127.0.0.1:8080``.
        token: An optional shared-secret bearer token. When set, it is sent as
            ``Authorization: Bearer <token>`` on every request.
        timeout: Per-request timeout in seconds for non-streaming calls.
        max_stream_retries: How many times to reconnect a dropped event stream
            before giving up. Each reconnect resumes from the cursor, so no
            event is missed or repeated.
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
        self._http = httpx.Client(base_url=self.base_url, headers=headers, timeout=timeout)
        # The event stream needs its own long-lived timeout: the read side of a
        # live tail blocks between events, so the read timeout is disabled while
        # connect/write stay bounded.
        self._stream_timeout = httpx.Timeout(timeout, read=None)
        #: The last drive token this client saw for a run it opened, so a
        #: later `open_client_run` for that same run presents its own held
        #: lease back rather than asking with no token and being refused
        #: `lease_held` by a lease this same client minted a moment ago (a
        #: lease is held until it lapses, not until a newer caller asks for
        #: it; see ``API.md``'s drive-token section). Never cleared: a stale
        #: entry is harmless (an open honours it only when it is the run's
        #: CURRENT lease, and ignores it otherwise), while forgetting it would
        #: needlessly refuse this same client's own next open of an idle
        #: thread until the lease it minted lapsed on its own. Passing
        #: ``drive_token`` explicitly always wins over what is remembered
        #: here; the underlying :class:`~salvor.client_runs.ClientRunDriver`
        #: stays stateless, so a genuinely different client (or the driver
        #: used directly) is still refused.
        self._client_run_tokens: dict[str, str] = {}

    def close(self) -> None:
        """Close the underlying HTTP connection pool."""
        self._http.close()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # -- agents ---------------------------------------------------------------

    def register_agent(self, definition: Union[str, dict[str, Any]]) -> str:
        """Register and validate an agent definition; return its content hash.

        An agent is data, so it has a content hash (the same id every
        ``RunStarted`` event records). Submit a definition once and reference it
        by that hash from then on. The server builds the agent to validate it
        and compute the hash, so a definition that will not build is a 400.

        Args:
            definition: The agent as a TOML string, or the same fields as a
                dict (sent as JSON).

        Returns:
            The agent hash, for example ``"sha256:34e0..."``.
        """
        return self._send(api.register_agent(definition))

    def list_agents(self) -> list[str]:
        """List the registered agent hashes."""
        return self._send(api.list_agents())

    def get_agent(self, agent_hash: str) -> dict[str, Any]:
        """Read one registered definition back: its hash, format, and body."""
        return self._send(api.get_agent(agent_hash))

    # -- runs -----------------------------------------------------------------

    def start_run(
        self,
        agent: str,
        input: Any = None,
        *,
        run_id: Optional[str] = None,
        labels: Optional[dict[str, str]] = None,
    ) -> str:
        """Start a fresh run of a registered agent; return its run id.

        The call returns as soon as the run is accepted; the run then drives in
        the background on the server. Open :meth:`stream_events` to watch it.

        Args:
            agent: The agent hash from :meth:`register_agent`.
            input: The run input, any JSON value. Defaults to ``None``.
            run_id: An optional client-chosen run id (a UUID). Omit to let the
                server mint one.
            labels: Optional correlation tags (a build id, an environment)
                recorded once on the run's ``RunStarted`` event and readable
                back from :meth:`list_runs`. See ``API.md`` for the bounds (at
                most 16 labels, keys under 64 bytes, values under 256 bytes)
                and the honest-absence rule. Omitted entirely, a run records
                none, byte-identical to a caller that predates this parameter.

        Returns:
            The run id.
        """
        return self._send(api.start_run(agent, input, run_id, labels))

    def list_runs(self) -> list[RunSummary]:
        """List every run with its folded status and counts."""
        return self._send(api.list_runs())

    def get_run(self, run_id: str) -> RunState:
        """Get one run's derived state (status, usage, pending call, counts)."""
        return self._send(api.get_run(run_id))

    def replay(self, run_id: str) -> ReplayState:
        """Dry-run replay: the derived state as a pure fold of the log,
        executing nothing. This is what ``salvor replay --dry-run`` prints."""
        return self._send(api.replay(run_id))

    def resume(self, run_id: str, input: Any = None) -> ResumeResult:
        """Continue a run: resume a parked one, or recover a crashed one.

        The server reads the run's state and dispatches: a parked run (suspended
        or budget-exceeded) needs an ``input`` validated against its recorded
        schema; a crashed run recovers with no input (any input is ignored); a
        finished run is reported and left alone. A run that needs reconciliation
        is refused, raising :class:`~salvor.errors.NeedsReconciliationError`;
        use :meth:`resolve` to move past it.
        """
        return self._send(api.resume(run_id, input))

    def resolve(self, run_id: str, output: Any) -> RunState:
        """Record a dangling write's completion by hand, the operator side of
        reconciliation.

        After verifying externally what a recorded-but-never-completed write
        did, this records the output it produced, so replay treats the call as
        done and never re-runs it. It records exactly one event and drives
        nothing.
        """
        return self._send(api.resolve(run_id, output))

    # -- graphs ---------------------------------------------------------------

    def submit_graph(self, document: Union[Graph, dict[str, Any]]) -> GraphSubmitted:
        """Submit and strictly validate a graph document; return its content
        hash and whether this call is what stored it.

        A graph document IS its hash, so re-submitting an identical document is
        idempotent: the same hash comes back with ``created=False``. A validation
        failure raises :class:`~salvor.errors.SalvorAPIError` with code
        ``invalid_graph``, whose ``details["errors"]`` is the complete node- and
        edge-precise list (nothing short-circuits); :meth:`validate_graph` asks
        the same question without storing and without raising.

        The server keeps submitted documents IN MEMORY only, so a restart drops
        them and a hash from a previous process no longer resolves. That is safe
        rather than lossy: submit the identical document again and it mints the
        identical hash, so a caller can simply re-submit before starting a run.

        Args:
            document: A :class:`~salvor.graph.Graph` from
                :meth:`~salvor.graph.GraphBuilder.build`, or the same document
                as a dict.
        """
        return self._send(api.submit_graph(document))

    def list_graphs(self) -> list[GraphSummary]:
        """List the stored graphs, each with its hash and shape summary."""
        return self._send(api.list_graphs())

    def get_graph(self, graph_hash: str) -> StoredGraph:
        """Read one stored document back by hash.

        Raises :class:`~salvor.errors.SalvorAPIError` with code
        ``unknown_graph`` when nothing is stored under it, which is also what a
        hash from before a server restart gets: see :meth:`submit_graph` on the
        in-memory store.
        """
        return self._send(api.get_graph(graph_hash))

    def validate_graph(self, document: Union[Graph, dict[str, Any]]) -> GraphValidation:
        """Validate a document without storing it: :meth:`submit_graph`'s dry
        run, the graph counterpart of :meth:`replay`.

        It answers the question rather than refusing the request, so an invalid
        document comes back with ``valid=False`` and the full error list instead
        of raising. Nothing is ever stored.
        """
        return self._send(api.validate_graph(document))

    def start_graph_run(
        self,
        graph_hash: str,
        input: Any = None,
        *,
        labels: Optional[dict[str, str]] = None,
    ) -> str:
        """Start a fresh run of a STORED graph; return its run id.

        Fire-and-return, exactly as :meth:`start_run` is for an agent run: the
        call returns as soon as the run is accepted, and the walk then drives in
        the background on the server. A graph run is an ordinary run with a
        richer log, so :meth:`get_run`, :meth:`stream_events`, :meth:`replay`
        and :meth:`resume` all work on it unchanged; :meth:`get_run_graph` adds
        the per-node view only a graph run has.

        Everything the document references is resolved BEFORE the run is
        spawned, so a reference that cannot resolve is a refusal rather than a
        run that fails halfway:

        - ``unknown_graph`` when the hash names no stored graph (including a
          hash from before a server restart, since the graph store is in
          memory).
        - ``unknown_agent``, naming the node, when an ``agent`` node references
          a hash no agent is registered under.
        - ``unknown_tool``, naming the node, when a ``tool`` node names a tool
          the server's registry does not hold. A stock ``salvor serve`` wires
          that registry EMPTY, so on a default server EVERY ``tool`` node
          refuses this way until a host registers the tool it names
          (``salvor serve --demo-tools`` is the built-in way to get a non-empty
          registry).

        Args:
            graph_hash: The stored graph's hash from :meth:`submit_graph`.
            input: The run input, any JSON value. Defaults to ``None``.
            labels: Optional correlation tags under the same bounds an agent
                run's carry. Omitted entirely, the run records none.

        Returns:
            The run id.
        """
        return self._send(api.start_graph_run(graph_hash, input, labels))

    def get_run_graph(self, run_id: str) -> GraphProjection:
        """A graph run's per-node projection: which nodes the walk has reached,
        which case each ``branch`` fired, and the node it is inside right now.

        A node the walk has not reached is absent rather than reported as some
        pending state. Refuses with ``not_a_graph_run`` for an ordinary agent
        run, which has no walk to project.
        """
        return self._send(api.get_run_graph(run_id))

    def fork_run(
        self,
        run_id: str,
        from_node: str,
        *,
        acknowledge_writes: Optional[list[int]] = None,
    ) -> ForkResult:
        """Fork a graph run from a node boundary into a NEW run.

        The child opens with the origin's log prefix rewritten under its own id
        and then continues from ``from_node``; the origin is never touched, and
        the child reuses the origin's graph unchanged (to change the graph,
        submit a new document and start a fresh run).

        A fork that would re-execute a recorded write the operator has not
        acknowledged is REFUSED, not silently replayed:
        :class:`~salvor.errors.SalvorAPIError` with code
        ``write_replay_hazard``, whose ``details["writes"]`` lists exactly the
        writes still needing acknowledgement. Pass their ``seq`` values as
        ``acknowledge_writes`` to accept that they may re-fire, and they are
        recorded permanently on the child. :meth:`preview_fork` asks the same
        question first, without creating anything.
        """
        return self._send(
            api.fork_run(run_id, from_node, acknowledge_writes, dry_run=False)
        )

    def preview_fork(
        self,
        run_id: str,
        from_node: str,
        *,
        acknowledge_writes: Optional[list[int]] = None,
    ) -> ForkPreview:
        """Preview a fork without creating one: which writes the re-walked
        segment holds, which of them are still unacknowledged, and whether the
        fork would proceed.

        The structural refusals (:meth:`fork_run`'s ``invalid_fork_node``,
        ``origin_needs_reconciliation``, ``not_a_graph_run``) still raise here,
        so a fork that could never proceed is reported rather than faked.
        """
        return self._send(
            api.fork_run(run_id, from_node, acknowledge_writes, dry_run=True)
        )

    def list_forks(self, run_id: str) -> ForksIndex:
        """The forks of a run, as the server's own DERIVED index: an origin is
        immutable and never points forward at its children, so this is a scan of
        every run's recorded origin, labelled ``derived`` to say so."""
        return self._send(api.list_forks(run_id))

    # -- client-performed tools -------------------------------------------------

    def list_client_tools(self) -> list[ClientToolDecl]:
        """List the client-performed tool declarations this server holds:
        tools an operator declared with ``salvor serve --client-tool <FILE>``,
        which the client runs itself (see
        :meth:`~salvor.client_runs.ClientRunDriver.client_tool_intent`).

        This is how a client-driven loop gets the function definitions to
        hand the model: a declaration's ``input_schema`` IS the model tool's
        parameter schema, the same schema the server checks a call's input
        against, published here so a client never keeps a second copy that
        can quietly drift from it.
        """
        return self._send(api.list_client_tools())

    # -- event stream ---------------------------------------------------------

    def stream_events(self, run_id: str, from_seq: Optional[int] = None) -> EventStream:
        """Stream a run's events in order until it rests.

        On connect the server replays every recorded event at or after the
        cursor, then tails new events as they land, then sends one terminal
        ``end`` frame and closes. The log is gap-free and duplicate-free by
        construction, so tracking one "next sequence" number is enough.

        A dropped connection is resumed automatically from that cursor
        (``?from_seq``), and any event seen before the drop is skipped, so the
        merged stream stays gap-free and duplicate-free across reconnects. The
        iterator stops at the ``end`` frame; the frame's status is then on
        :attr:`EventStream.end`.

        Args:
            run_id: The run to stream.
            from_seq: The sequence number to start from. Omit for a full replay
                from sequence 0.
        """
        stream = EventStream()
        stream._gen = self._events(run_id, from_seq or 0, stream)
        return stream

    def _events(self, run_id: str, from_seq: int, stream: EventStream) -> Iterator[Event]:
        tail = EventTail(run_id, from_seq, self._max_stream_retries)
        while True:
            try:
                for kind, obj in self._read_frames(run_id, tail.next_seq):
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
            time.sleep(tail.backoff())

    def _read_frames(self, run_id: str, from_seq: int) -> Iterator[tuple[str, dict[str, Any]]]:
        """Open one connection and yield ``(kind, obj)`` per server-sent frame,
        where ``kind`` is ``"event"`` (an envelope) or ``"end"`` (the terminal
        frame). The line protocol itself is
        :class:`salvor._core.sse.SSEDecoder`, fed one line at a time."""
        call = events_stream(run_id, from_seq)
        with self._http.stream(
            call.method,
            call.path,
            params=call.params,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                raise self._error(resp.status_code, resp.read())
            for frame in sse_frames(resp.iter_lines(), flush_trailing=False):
                yield event_frame(frame)

    # -- helpers --------------------------------------------------------------

    def _send(self, call: Call) -> Any:
        """Perform one described call: send it, decode the answer, parse it."""
        resp = self._http.request(call.method, call.path, **wire.request_kwargs(call))
        return call.parse(wire.decode_json(resp.status_code, resp.content))

    def _json(self, resp: httpx.Response) -> dict[str, Any]:
        return wire.decode_json(resp.status_code, resp.content)

    def _error(self, status: int, body: bytes) -> SalvorAPIError:
        return wire.error(status, body)

    def open_client_run(
        self,
        *,
        agent: Optional[str] = None,
        input: Any = None,
        run_id: Optional[str] = None,
        record_prompts: bool = False,
        drive_token: Optional[str] = None,
    ) -> "ClientRunDriver":
        """Open or re-open a client-driven run over this client's connection.

        Returns a :class:`~salvor.client_runs.ClientRunDriver`: the client owns
        the agent loop and streams the events it produces, while the server owns
        the durable log and guards every append. This is the second of Salvor's
        two modes; :meth:`start_run` is the server-driven first. The driver
        shares this client's HTTP pool and auth, so it is closed when this
        client is.

        ``drive_token`` re-opens under a lease this process already holds:
        pass a run's current token back and the re-open returns its recorded
        log under the SAME token rather than raising
        :class:`~salvor.errors.LeaseHeldError`, which is what a bare re-open of
        a run whose lease is still current meets instead. See
        :meth:`salvor.client_runs.ClientRunDriver.open` for the full rule.

        Left unset (the default), this client fills in the last token IT
        remembers for ``run_id``, if any: this client's own earlier
        :meth:`open_client_run` for the same run, or its own :meth:`start_run`
        made no difference here, since it is the presented token, not the
        caller, that a re-open checks. That remembered token is stale-safe
        (see :attr:`_client_run_tokens`), so this is silent and free the first
        time a run id is seen and whenever the remembered lease has already
        lapsed. Pass ``drive_token`` explicitly to override it, including with
        ``""`` or any other value that is not the current lease, which is
        refused exactly as an unset one would be if the lease is still held by
        someone else.
        """
        from .client_runs import ClientRunDriver

        if drive_token is None:
            drive_token = self._client_run_tokens.get(run_id) if run_id else None
        driver = ClientRunDriver._open_over(
            self._http,
            owns_http=False,
            stream_timeout=self._stream_timeout,
            agent=agent,
            input=input,
            run_id=run_id,
            record_prompts=record_prompts,
            drive_token=drive_token,
        )
        self._client_run_tokens[driver.run_id] = driver.drive_token
        return driver
