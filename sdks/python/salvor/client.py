"""A thin synchronous client for the Salvor control plane.

The client holds no durability logic and no run state. It submits definitions
and inputs, reads events, and decodes the error envelope into exceptions. Every
guarantee, exact replay, crash-safe resume, the write-ahead reconciliation
rule, lives in the one Rust process the client talks to.
"""

from __future__ import annotations

import json
import time
from typing import Any, Iterator, Optional, Union

import httpx

from .errors import (
    SalvorAPIError,
    SalvorStreamError,
    decode_error,
)
from .graph import Graph
from .models import (
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


def _document(document: Union[Graph, dict[str, Any]]) -> dict[str, Any]:
    """The wire JSON for a graph document, from either a built
    :class:`~salvor.graph.Graph` or a plain dict of the same fields. A dict is
    passed through untouched, so a document authored by hand (or read from a
    file) needs no builder."""
    return document.to_dict() if isinstance(document, Graph) else document


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
        self.base_url = base_url.rstrip("/")
        self._token = token
        self._max_stream_retries = max_stream_retries
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        self._http = httpx.Client(base_url=self.base_url, headers=headers, timeout=timeout)
        # The event stream needs its own long-lived timeout: the read side of a
        # live tail blocks between events, so the read timeout is disabled while
        # connect/write stay bounded.
        self._stream_timeout = httpx.Timeout(timeout, read=None)

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
        if isinstance(definition, str):
            content = definition.encode("utf-8")
            content_type = "application/toml"
        else:
            content = json.dumps(definition).encode("utf-8")
            content_type = "application/json"
        resp = self._http.post(
            "/v1/agents", content=content, headers={"Content-Type": content_type}
        )
        return self._json(resp)["agent"]

    def list_agents(self) -> list[str]:
        """List the registered agent hashes."""
        resp = self._http.get("/v1/agents")
        return [a["agent"] for a in self._json(resp).get("agents", [])]

    def get_agent(self, agent_hash: str) -> dict[str, Any]:
        """Read one registered definition back: its hash, format, and body."""
        resp = self._http.get(f"/v1/agents/{agent_hash}")
        return self._json(resp)

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
        body: dict[str, Any] = {"agent": agent, "input": input}
        if run_id is not None:
            body["run_id"] = run_id
        if labels is not None:
            body["labels"] = labels
        resp = self._http.post("/v1/runs", json=body)
        return self._json(resp)["run"]

    def list_runs(self) -> list[RunSummary]:
        """List every run with its folded status and counts."""
        resp = self._http.get("/v1/runs")
        return [RunSummary.from_json(r) for r in self._json(resp).get("runs", [])]

    def get_run(self, run_id: str) -> RunState:
        """Get one run's derived state (status, usage, pending call, counts)."""
        resp = self._http.get(f"/v1/runs/{run_id}")
        return RunState.from_json(self._json(resp))

    def replay(self, run_id: str) -> ReplayState:
        """Dry-run replay: the derived state as a pure fold of the log,
        executing nothing. This is what ``salvor replay --dry-run`` prints."""
        resp = self._http.get(f"/v1/runs/{run_id}/replay")
        return ReplayState.from_json(self._json(resp))

    def resume(self, run_id: str, input: Any = None) -> ResumeResult:
        """Continue a run: resume a parked one, or recover a crashed one.

        The server reads the run's state and dispatches: a parked run (suspended
        or budget-exceeded) needs an ``input`` validated against its recorded
        schema; a crashed run recovers with no input (any input is ignored); a
        finished run is reported and left alone. A run that needs reconciliation
        is refused, raising :class:`~salvor.errors.NeedsReconciliationError`;
        use :meth:`resolve` to move past it.
        """
        body = {} if input is None else {"input": input}
        resp = self._http.post(f"/v1/runs/{run_id}/resume", json=body)
        return ResumeResult.from_json(self._json(resp))

    def resolve(self, run_id: str, output: Any) -> RunState:
        """Record a dangling write's completion by hand, the operator side of
        reconciliation.

        After verifying externally what a recorded-but-never-completed write
        did, this records the output it produced, so replay treats the call as
        done and never re-runs it. It records exactly one event and drives
        nothing.
        """
        resp = self._http.post(f"/v1/runs/{run_id}/resolve", json={"output": output})
        obj = self._json(resp)
        # The resolve response nests the status; hand back a RunState view of it
        # so the caller reads the run the same way get_run does.
        return RunState.from_json(
            {
                "run": obj.get("run", run_id),
                "status": obj.get("status", {}),
                "event_count": obj.get("event_count", 0),
            }
        )

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
        resp = self._http.post("/v1/graphs", json=_document(document))
        return GraphSubmitted.from_json(self._json(resp))

    def list_graphs(self) -> list[GraphSummary]:
        """List the stored graphs, each with its hash and shape summary."""
        resp = self._http.get("/v1/graphs")
        return [GraphSummary.from_json(g) for g in self._json(resp).get("graphs", [])]

    def get_graph(self, graph_hash: str) -> StoredGraph:
        """Read one stored document back by hash.

        Raises :class:`~salvor.errors.SalvorAPIError` with code
        ``unknown_graph`` when nothing is stored under it, which is also what a
        hash from before a server restart gets: see :meth:`submit_graph` on the
        in-memory store.
        """
        resp = self._http.get(f"/v1/graphs/{graph_hash}")
        return StoredGraph.from_json(self._json(resp))

    def validate_graph(self, document: Union[Graph, dict[str, Any]]) -> GraphValidation:
        """Validate a document without storing it: :meth:`submit_graph`'s dry
        run, the graph counterpart of :meth:`replay`.

        It answers the question rather than refusing the request, so an invalid
        document comes back with ``valid=False`` and the full error list instead
        of raising. Nothing is ever stored.
        """
        resp = self._http.post("/v1/graphs/validate", json=_document(document))
        return GraphValidation.from_json(self._json(resp))

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
        body: dict[str, Any] = {"graph_hash": graph_hash, "input": input}
        if labels is not None:
            body["labels"] = labels
        resp = self._http.post("/v1/graph-runs", json=body)
        return self._json(resp)["run"]

    def get_run_graph(self, run_id: str) -> GraphProjection:
        """A graph run's per-node projection: which nodes the walk has reached,
        which case each ``branch`` fired, and the node it is inside right now.

        A node the walk has not reached is absent rather than reported as some
        pending state. Refuses with ``not_a_graph_run`` for an ordinary agent
        run, which has no walk to project.
        """
        resp = self._http.get(f"/v1/runs/{run_id}/graph")
        return GraphProjection.from_json(self._json(resp))

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
        body: dict[str, Any] = {"from_node": from_node}
        if acknowledge_writes is not None:
            body["acknowledge_writes"] = acknowledge_writes
        resp = self._http.post(f"/v1/runs/{run_id}/fork", json=body)
        return ForkResult.from_json(self._json(resp))

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
        body: dict[str, Any] = {"from_node": from_node, "dry_run": True}
        if acknowledge_writes is not None:
            body["acknowledge_writes"] = acknowledge_writes
        resp = self._http.post(f"/v1/runs/{run_id}/fork", json=body)
        return ForkPreview.from_json(self._json(resp))

    def list_forks(self, run_id: str) -> ForksIndex:
        """The forks of a run, as the server's own DERIVED index: an origin is
        immutable and never points forward at its children, so this is a scan of
        every run's recorded origin, labelled ``derived`` to say so."""
        resp = self._http.get(f"/v1/runs/{run_id}/forks")
        return ForksIndex.from_json(self._json(resp))

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
        next_seq = from_seq
        last_seen: Optional[int] = None
        attempts = 0
        while True:
            try:
                ended = False
                for kind, obj in self._read_frames(run_id, next_seq):
                    if kind == "event":
                        event = Event.from_envelope(obj)
                        if last_seen is not None and event.seq <= last_seen:
                            continue  # already delivered before a drop; skip it
                        last_seen = event.seq
                        next_seq = event.seq + 1
                        attempts = 0  # forward progress refreshes the retry budget
                        yield event
                    elif kind == "end":
                        stream.end = EndFrame.from_json(obj)
                        ended = True
                        break
                if ended:
                    return
                # The stream closed with no end frame: the connection dropped
                # mid-tail. Fall through to reconnect from the cursor.
            except (httpx.TransportError, httpx.RemoteProtocolError):
                pass  # transient drop; reconnect below
            attempts += 1
            if attempts > self._max_stream_retries:
                raise SalvorStreamError(
                    f"event stream for run {run_id} dropped and did not resume "
                    f"after {self._max_stream_retries} attempts"
                )
            time.sleep(min(0.25 * attempts, 2.0))

    def _read_frames(self, run_id: str, from_seq: int) -> Iterator[tuple[str, dict[str, Any]]]:
        """Open one connection and yield ``(kind, obj)`` per server-sent frame,
        where ``kind`` is ``"event"`` (an envelope) or ``"end"`` (the terminal
        frame). Parses the SSE line protocol directly over fetch streaming."""
        params = {"from_seq": from_seq}
        with self._http.stream(
            "GET",
            f"/v1/runs/{run_id}/events",
            params=params,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                body = resp.read()
                raise self._error(resp.status_code, body)
            event_name: Optional[str] = None
            data_lines: list[str] = []
            for line in resp.iter_lines():
                if line == "":
                    # A blank line dispatches the accumulated frame.
                    if data_lines:
                        payload = json.loads("\n".join(data_lines))
                        yield ("end" if event_name == "end" else "event", payload)
                    event_name = None
                    data_lines = []
                    continue
                if line.startswith(":"):
                    continue  # a comment / keep-alive line
                field, _, value = line.partition(":")
                if value.startswith(" "):
                    value = value[1:]
                if field == "event":
                    event_name = value
                elif field == "data":
                    data_lines.append(value)
                # `id:` lines carry the seq; the seq is also inside the data
                # envelope, so there is nothing extra to track here.

    # -- helpers --------------------------------------------------------------

    def _json(self, resp: httpx.Response) -> dict[str, Any]:
        if resp.status_code // 100 != 2:
            raise self._error(resp.status_code, resp.content)
        if not resp.content:
            return {}
        return resp.json()

    def _error(self, status: int, body: bytes) -> SalvorAPIError:
        return decode_error(status, body)

    def open_client_run(
        self,
        *,
        agent: Optional[str] = None,
        input: Any = None,
        run_id: Optional[str] = None,
        record_prompts: bool = False,
    ) -> "ClientRunDriver":
        """Open or re-open a client-driven run over this client's connection.

        Returns a :class:`~salvor.client_runs.ClientRunDriver`: the client owns
        the agent loop and streams the events it produces, while the server owns
        the durable log and guards every append. This is the second of Salvor's
        two modes; :meth:`start_run` is the server-driven first. The driver
        shares this client's HTTP pool and auth, so it is closed when this
        client is.
        """
        from .client_runs import ClientRunDriver

        return ClientRunDriver._open_over(
            self._http,
            owns_http=False,
            stream_timeout=self._stream_timeout,
            agent=agent,
            input=input,
            run_id=run_id,
            record_prompts=record_prompts,
        )
