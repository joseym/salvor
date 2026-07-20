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
