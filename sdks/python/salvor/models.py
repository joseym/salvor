"""Typed views over the JSON the control plane returns.

These dataclasses stay deliberately thin. The event envelope and the derived
state are defined by the server (see ``crates/salvor-replay/src/event.rs`` and
``crates/salvor-server/API.md``); the SDK surfaces the common fields as typed
attributes and keeps the full decoded JSON on ``raw`` so nothing the server
adds later is lost.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional


def _parse_driver(value: Any) -> Optional[str]:
    """``"attached"``/``"none"`` verbatim, else ``None`` -- a terminal run omits
    the field and an older server never sends it, and neither is fabricated into
    a default."""
    return value if value in ("attached", "none") else None


@dataclass
class Usage:
    """Token counts folded from a run's model calls."""

    input_tokens: int
    output_tokens: int

    @classmethod
    def from_json(cls, obj: Optional[dict[str, Any]]) -> Optional["Usage"]:
        if obj is None:
            return None
        return cls(
            input_tokens=int(obj.get("input_tokens", 0)),
            output_tokens=int(obj.get("output_tokens", 0)),
        )


@dataclass
class RunStatus:
    """A run's folded status: always a ``state`` name plus state-specific keys.

    The extra keys depend on the state (a ``completed`` run carries ``output``,
    a ``suspended`` run carries ``reason`` and ``input_schema``, and so on).
    They stay on ``raw``; the convenience properties read the common ones.
    """

    state: str
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "RunStatus":
        return cls(state=obj.get("state", "unknown"), raw=obj)

    @property
    def is_completed(self) -> bool:
        return self.state == "completed"

    @property
    def is_failed(self) -> bool:
        return self.state == "failed"

    @property
    def output(self) -> Any:
        """The final output of a completed run, or ``None``."""
        return self.raw.get("output")

    @property
    def error(self) -> Optional[str]:
        """The failure description of a failed run, or ``None``."""
        return self.raw.get("error")

    @property
    def reason(self) -> Optional[str]:
        """Why a suspended run parked, or ``None``."""
        return self.raw.get("reason")

    @property
    def input_schema(self) -> Optional[dict[str, Any]]:
        """The JSON Schema a suspended run's resume input must satisfy."""
        return self.raw.get("input_schema")


@dataclass
class PendingCall:
    """The step a run is waiting on: a model call or a tool call.

    ``kind`` is ``"model"`` or ``"tool"``. A model pending call carries a
    ``request_hash``; a tool pending call carries ``tool``, ``input``,
    ``effect``, and an optional ``idempotency_key``. All fields stay on
    ``raw``.
    """

    kind: str
    seq: int
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: Optional[dict[str, Any]]) -> Optional["PendingCall"]:
        if obj is None:
            return None
        return cls(kind=obj.get("kind", "unknown"), seq=int(obj.get("seq", 0)), raw=obj)

    @property
    def tool(self) -> Optional[str]:
        return self.raw.get("tool")

    @property
    def effect(self) -> Optional[str]:
        return self.raw.get("effect")

    @property
    def input(self) -> Any:
        return self.raw.get("input")


@dataclass
class RunState:
    """The derived state of one run, as returned by ``GET /v1/runs/{id}``.

    Status is always current because the server folds it from the log on every
    read rather than storing it.
    """

    run: str
    status: RunStatus
    event_count: int
    usage: Optional[Usage] = None
    pending: Optional[PendingCall] = None
    first_recorded_at: Optional[str] = None
    last_recorded_at: Optional[str] = None
    driver: Optional[str] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "RunState":
        return cls(
            run=obj["run"],
            status=RunStatus.from_json(obj.get("status", {})),
            event_count=int(obj.get("event_count", 0)),
            usage=Usage.from_json(obj.get("usage")),
            pending=PendingCall.from_json(obj.get("pending")),
            first_recorded_at=obj.get("first_recorded_at"),
            last_recorded_at=obj.get("last_recorded_at"),
            driver=_parse_driver(obj.get("driver")),
            raw=obj,
        )


@dataclass
class RunSummary:
    """One row of ``GET /v1/runs``: a run id with its folded status and counts.

    ``usage``, ``step_count``, ``agent_def_hash``, and ``labels`` are
    additive: present whenever the run's log folds (a real ``0`` when a run
    genuinely has no model calls yet), and ``None`` -- never a fabricated
    zero -- only when the server could not read that run's log at all (see
    ``API.md``). ``labels`` follows the same rule one step further: also
    ``None`` when a run recorded no labels at all, or recorded an explicit
    empty set -- the server never sends ``labels: {}``. ``raw`` always
    carries whatever the server actually sent, so a server-side field this
    SDK has not been taught yet is never lost.

    ``driver`` is liveness evidence: ``"attached"`` when a driver is currently
    running the run (a live server task, or a current client-driven lease),
    ``"none"`` when none is, and ``None`` for a terminal run (and from an older
    server). Paired with ``last_recorded_at``, it is how a client derives a
    stalled run -- one that folds to ``running`` yet has no driver and has gone
    quiet.
    """

    run: str
    status: RunStatus
    event_count: int
    first_recorded_at: Optional[str] = None
    last_recorded_at: Optional[str] = None
    usage: Optional[Usage] = None
    step_count: Optional[int] = None
    agent_def_hash: Optional[str] = None
    labels: Optional[dict[str, str]] = None
    driver: Optional[str] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "RunSummary":
        step_count = obj.get("step_count")
        return cls(
            run=obj["run"],
            status=RunStatus.from_json(obj.get("status", {})),
            event_count=int(obj.get("event_count", 0)),
            first_recorded_at=obj.get("first_recorded_at"),
            last_recorded_at=obj.get("last_recorded_at"),
            usage=Usage.from_json(obj.get("usage")),
            step_count=int(step_count) if step_count is not None else None,
            agent_def_hash=obj.get("agent_def_hash"),
            labels=obj.get("labels"),
            driver=_parse_driver(obj.get("driver")),
            raw=obj,
        )


@dataclass
class ReplayState:
    """The dry-run replay projection from ``GET /v1/runs/{id}/replay``: the
    derived state as a pure fold of the log, executing nothing."""

    status: RunStatus
    next_seq: int
    usage: Optional[Usage] = None
    pending: Optional[PendingCall] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ReplayState":
        return cls(
            status=RunStatus.from_json(obj.get("status", {})),
            next_seq=int(obj.get("next_seq", 0)),
            usage=Usage.from_json(obj.get("usage")),
            pending=PendingCall.from_json(obj.get("pending")),
            raw=obj,
        )


@dataclass
class ResumeResult:
    """The result of a resume: ``outcome`` is ``"driving"`` for a run now
    running in the background, or a finished state left alone."""

    run: str
    outcome: str
    status: Optional[RunStatus] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ResumeResult":
        status = obj.get("status")
        return cls(
            run=obj["run"],
            outcome=obj.get("outcome", obj.get("status", "unknown") if isinstance(obj.get("status"), str) else "unknown"),
            status=RunStatus.from_json(status) if isinstance(status, dict) else None,
            raw=obj,
        )


@dataclass
class Event:
    """One recorded event, decoded from the pinned envelope wire JSON.

    The same bytes arrive as a stream frame's ``data`` and as a log row, so one
    decoder serves both. ``kind`` names what happened (``RunStarted``,
    ``ModelCallCompleted``, ``ToolCallRequested``, ``RunCompleted``, ...) and
    ``payload`` holds its fields. The helper properties read the ones the
    common kinds carry.
    """

    run_id: str
    seq: int
    schema_version: int
    recorded_at: str
    kind: str
    payload: dict[str, Any]

    @classmethod
    def from_envelope(cls, obj: dict[str, Any]) -> "Event":
        event = obj.get("event", {})
        return cls(
            run_id=obj["run_id"],
            seq=int(obj["seq"]),
            schema_version=int(obj.get("schema_version", 1)),
            recorded_at=obj.get("recorded_at", ""),
            kind=event.get("kind", "Unknown"),
            payload=event.get("payload", {}) or {},
        )

    @property
    def usage(self) -> Optional[Usage]:
        """Token usage on a ``ModelCallCompleted`` event, else ``None``."""
        return Usage.from_json(self.payload.get("usage"))

    @property
    def output(self) -> Any:
        """The output on a ``RunCompleted`` or ``ToolCallCompleted`` event."""
        return self.payload.get("output")

    @property
    def error(self) -> Optional[str]:
        """The error on a ``RunFailed`` event, else ``None``."""
        return self.payload.get("error")

    @property
    def tool(self) -> Optional[str]:
        """The tool name on a ``ToolCall*`` event, else ``None``."""
        return self.payload.get("tool")


@dataclass
class EndFrame:
    """The terminal ``event: end`` frame that closes a stream.

    ``status`` is the run's resting status. ``detached`` is ``True`` when the
    run was mid-step but no driver is running it in this server process, so a
    fresh stream (opened after recovering the run) tails the continuation.
    """

    status: Optional[RunStatus]
    detached: bool = False
    error: Optional[str] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "EndFrame":
        status = obj.get("status")
        return cls(
            status=RunStatus.from_json(status) if isinstance(status, dict) else None,
            detached=bool(obj.get("detached", False)),
            error=obj.get("error"),
            raw=obj,
        )


@dataclass
class GraphShape:
    """A graph document's shape: how many nodes and edges it holds, and the ends
    of its walk. Sent per row by ``GET /v1/graphs``, and by
    ``POST /v1/graphs/validate`` for a document that validates."""

    node_count: int
    edge_count: int
    entry_nodes: list[str] = field(default_factory=list)
    terminal_nodes: list[str] = field(default_factory=list)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphShape":
        return cls(
            node_count=int(obj.get("node_count", 0)),
            edge_count=int(obj.get("edge_count", 0)),
            entry_nodes=list(obj.get("entry_nodes", [])),
            terminal_nodes=list(obj.get("terminal_nodes", [])),
        )


@dataclass
class GraphSubmitted:
    """The receipt from ``POST /v1/graphs``: the document's content hash, and
    whether this call is what stored it.

    ``created`` is ``False`` when the identical document was already stored, so
    re-submitting is idempotent rather than a conflict: the same document always
    hashes to the same id.
    """

    graph: str
    created: bool
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphSubmitted":
        return cls(graph=obj["graph"], created=bool(obj.get("created", False)), raw=obj)


@dataclass
class GraphSummary:
    """One row of ``GET /v1/graphs``: a stored document's hash and its shape."""

    graph: str
    shape: GraphShape
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphSummary":
        return cls(graph=obj["graph"], shape=GraphShape.from_json(obj), raw=obj)


@dataclass
class StoredGraph:
    """One stored document read back by hash, from ``GET /v1/graphs/{hash}``.

    ``document`` is the document JSON verbatim -- the same bytes that were
    submitted, which is why they hash to ``graph``.
    """

    graph: str
    document: dict[str, Any]
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "StoredGraph":
        return cls(graph=obj["graph"], document=obj.get("document", {}), raw=obj)


@dataclass
class GraphValidationError:
    """One validation failure, node- or edge-precise.

    ``code`` is the stable token (``dangling_edge``, ``duplicate_id``,
    ``node_name_too_long``; a document that does not even parse is the single
    ``malformed_document``). ``node`` or ``edge`` names where the failure is,
    and at most one of the two is ever present.
    """

    code: str
    message: str
    node: Optional[str] = None
    edge: Optional[dict[str, Any]] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphValidationError":
        return cls(
            code=obj.get("code", "unknown"),
            message=obj.get("message", ""),
            node=obj.get("node"),
            edge=obj.get("edge"),
            raw=obj,
        )


@dataclass
class GraphValidation:
    """The answer from ``POST /v1/graphs/validate``.

    It answers the question rather than refusing the request: an invalid
    document comes back ``valid=False`` with the complete error list instead of
    raising, and nothing is stored either way. ``graph`` and ``shape`` are
    present only when the document is valid; ``errors`` is empty then.
    """

    valid: bool
    graph: Optional[str] = None
    shape: Optional[GraphShape] = None
    errors: list[GraphValidationError] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphValidation":
        summary = obj.get("summary")
        return cls(
            valid=bool(obj.get("valid", False)),
            graph=obj.get("graph"),
            shape=GraphShape.from_json(summary) if isinstance(summary, dict) else None,
            errors=[GraphValidationError.from_json(e) for e in obj.get("errors", [])],
            raw=obj,
        )


@dataclass
class GraphNodeProgress:
    """One node's progress in a graph run's walk.

    ``state`` is ``entered``, ``exited``, or ``skipped``; a skipped node carries
    the ``reason``. ``branch_case`` is the case a ``branch`` fired, present only
    once decided. A ``map`` node's fan-out is on ``raw`` verbatim. A node the
    walk has not reached is absent from the projection entirely, which is
    distinct from a ``skipped`` one.
    """

    node: str
    state: str
    reason: Optional[str] = None
    branch_case: Optional[str] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphNodeProgress":
        return cls(
            node=obj["node"],
            state=obj.get("state", "unknown"),
            reason=obj.get("reason"),
            branch_case=obj.get("branch_case"),
            raw=obj,
        )


@dataclass
class ForkOrigin:
    """The origin a forked run records permanently on its seq-0
    ``GraphRunStarted``: where it came from, how much of the origin's log it
    opens with, and which recorded writes the operator accepted may re-fire."""

    run_id: str
    through_seq: int
    from_node: str
    graph_hash: str
    acknowledged_writes: list[int] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: Optional[dict[str, Any]]) -> Optional["ForkOrigin"]:
        if not isinstance(obj, dict):
            return None
        return cls(
            run_id=obj["run_id"],
            through_seq=int(obj.get("through_seq", 0)),
            from_node=obj["from_node"],
            graph_hash=obj["graph_hash"],
            acknowledged_writes=[int(s) for s in obj.get("acknowledged_writes", [])],
            raw=obj,
        )


@dataclass
class GraphProjection:
    """A graph run's per-node projection, from ``GET /v1/runs/{id}/graph``:
    which nodes the walk has reached, and the graph it is walking.

    ``current_node`` is present only while a node is entered and not yet exited;
    ``forked_from`` only on a run that was forked.
    """

    graph_hash: str
    current_node: Optional[str] = None
    nodes: list[GraphNodeProgress] = field(default_factory=list)
    forked_from: Optional[ForkOrigin] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "GraphProjection":
        return cls(
            graph_hash=obj["graph_hash"],
            current_node=obj.get("current_node"),
            nodes=[GraphNodeProgress.from_json(n) for n in obj.get("nodes", [])],
            forked_from=ForkOrigin.from_json(obj.get("forked_from")),
            raw=obj,
        )


@dataclass
class ForkResult:
    """The receipt from a real fork: the new child run, and the origin it
    records."""

    run: str
    status: str
    forked_from: Optional[ForkOrigin] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ForkResult":
        return cls(
            run=obj["run"],
            status=obj.get("status", "unknown"),
            forked_from=ForkOrigin.from_json(obj.get("forked_from")),
            raw=obj,
        )


@dataclass
class RecordedWrite:
    """One recorded write in the segment a fork would re-walk.

    A write with an ``idempotency_key`` is not a hazard (the provider collapses
    the duplicate); one without needs acknowledging by ``seq`` before the fork
    may proceed. The server sends an explicit ``null`` key for a write that has
    none, which is absence rather than a value.
    """

    seq: int
    tool: str
    input: Any = None
    idempotency_key: Optional[str] = None
    recorded_at: Optional[str] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "RecordedWrite":
        return cls(
            seq=int(obj.get("seq", 0)),
            tool=obj.get("tool", ""),
            input=obj.get("input"),
            idempotency_key=obj.get("idempotency_key"),
            recorded_at=obj.get("recorded_at"),
            raw=obj,
        )


@dataclass
class ForkPreview:
    """A fork's dry-run preview, from ``POST /v1/runs/{id}/fork`` with
    ``dry_run: true``. Nothing is created.

    ``unacknowledged_writes`` is exactly the set of ``seq`` values a real fork
    would refuse over, and ``would_proceed`` says whether the fork as previewed
    could go ahead.
    """

    origin: str
    from_node: str
    through_seq: int
    graph_hash: str
    prefix_event_count: int
    writes: list[RecordedWrite] = field(default_factory=list)
    unacknowledged_writes: list[int] = field(default_factory=list)
    would_proceed: bool = False
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ForkPreview":
        return cls(
            origin=obj["origin"],
            from_node=obj["from_node"],
            through_seq=int(obj.get("through_seq", 0)),
            graph_hash=obj["graph_hash"],
            prefix_event_count=int(obj.get("prefix_event_count", 0)),
            writes=[RecordedWrite.from_json(w) for w in obj.get("writes", [])],
            unacknowledged_writes=[int(s) for s in obj.get("unacknowledged_writes", [])],
            would_proceed=bool(obj.get("would_proceed", False)),
            raw=obj,
        )


@dataclass
class ForkEntry:
    """One row of the forks index: a child run and the boundary it forked at."""

    run: str
    from_node: str
    through_seq: int
    acknowledged_writes: list[int] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ForkEntry":
        return cls(
            run=obj["run"],
            from_node=obj["from_node"],
            through_seq=int(obj.get("through_seq", 0)),
            acknowledged_writes=[int(s) for s in obj.get("acknowledged_writes", [])],
            raw=obj,
        )


@dataclass
class ClientToolDecl:
    """One operator-declared tool the CLIENT performs, from
    ``GET /v1/client-tools``.

    ``input_schema`` is the same JSON Schema a client-tool intent's ``input``
    is checked against server-side, which is also, verbatim, the parameter
    schema to hand the model as this tool's function definition: one schema,
    not a copy a client keeps in step by hand. ``output_schema`` is ``None``
    unless the declaration carries one; a tool declared without one cannot be
    self-completed by a client (see
    :meth:`~salvor.client_runs.ClientRunDriver.client_tool_completion`).
    ``trust_completion`` is ``True`` unless the operator declared otherwise.
    """

    name: str
    effect: str
    input_schema: Any
    output_schema: Optional[Any] = None
    trust_completion: bool = True
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ClientToolDecl":
        return cls(
            name=obj["name"],
            effect=obj.get("effect", "read"),
            input_schema=obj.get("input_schema"),
            output_schema=obj.get("output_schema"),
            trust_completion=bool(obj.get("trust_completion", True)),
            raw=obj,
        )


@dataclass
class ForksIndex:
    """The forks of a run, from ``GET /v1/runs/{id}/forks``.

    ``derived`` is the server saying so out loud: an origin is immutable and
    never points forward at its children, so this index is a scan of every run's
    recorded origin rather than a fact the origin holds.
    """

    run: str
    derived: bool = False
    forks: list[ForkEntry] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ForksIndex":
        return cls(
            run=obj["run"],
            derived=bool(obj.get("derived", False)),
            forks=[ForkEntry.from_json(f) for f in obj.get("forks", [])],
            raw=obj,
        )
