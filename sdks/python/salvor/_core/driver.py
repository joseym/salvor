"""The client-driven run's wire shapes and its durable-timer arithmetic.

Two kinds of thing live here. The :class:`~salvor._core.wire.Call` builders are
the driver's half of the protocol: open, the guarded generic append, the two
server-performed steps, the client-performed pair, and resolve. The rest is the
part that is not protocol at all but rule: what a recorded event at a position
means, when a park is a replay and when it is a divergence, and what a clock
reading is allowed to conclude. Those are pure functions over the log and this
drive's state, so both drivers reach the same verdict from the same evidence.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Callable, Optional

from ..models import Event, Usage
from .wire import Call, discard, identity


# -- results ------------------------------------------------------------------


@dataclass
class ModelStepResult:
    """The completion of a server-performed model step.

    ``response`` is the provider's ``MessageResponse`` as decoded JSON; ``usage``
    is the token counts folded from it. The full decoded body stays on ``raw``.
    The client feeds ``response`` and the recomputed request hash back to its
    cursor, which advances over the two now-recorded events.
    """

    response: Any
    usage: Optional[Usage] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ModelStepResult":
        return cls(
            response=obj.get("response"),
            usage=Usage.from_json(obj.get("usage")),
            raw=obj,
        )


@dataclass
class ClientToolIntentResult:
    """The receipt from opening a client-performed tool call: the position,
    the DERIVED idempotency key the client must perform under, the
    operator-declared effect the intent was recorded with, and whether this
    position's completion is already recorded.

    ``settled`` is ``True`` when the intent at ``seq`` already has its
    completion recorded, ``False`` otherwise. A payments caller retrying
    ``client_tool_intent`` after a dropped response gets back the same key
    either way; ``settled`` is what lets it tell "safe to perform the call"
    from "already done, do not perform it again" without separately reading
    the log. ``output`` rides along whenever ``settled`` is ``True`` -- the
    recorded completion's output, a normal result or the ``__salvor_error``
    failure sentinel alike -- so a caller that only needs to know what a
    settled call produced never has to read the log a second time to find
    out; it is ``None`` while the call is still open.
    """

    seq: int
    idempotency_key: str
    effect: str
    settled: bool
    output: Any = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ClientToolIntentResult":
        return cls(
            seq=int(obj.get("seq", 0)),
            idempotency_key=obj.get("idempotency_key", ""),
            effect=obj.get("effect", ""),
            settled=bool(obj.get("settled", False)),
            output=obj.get("output"),
            raw=obj,
        )


@dataclass
class ClientModelIntentResult:
    """The receipt from opening a model call the client performs: the
    position, whether this position's completion is already recorded, and,
    when it is, the recorded response and its usage.

    ``settled`` is ``True`` when the intent at ``seq`` already has its
    completion recorded, ``False`` otherwise. It is what a caller retrying
    :meth:`~salvor.client_runs.ClientRunDriver.client_model_intent` after a
    dropped response uses to tell "the call still has to be made" from
    "already recorded, return this instead" without a separate log read;
    ``response`` and ``usage`` carry the recorded answer only when ``settled``
    is ``True``.
    """

    seq: int
    settled: bool
    response: Any = None
    usage: Optional[Usage] = None
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ClientModelIntentResult":
        return cls(
            seq=int(obj.get("seq", 0)),
            settled=bool(obj.get("settled", False)),
            response=obj.get("response"),
            usage=Usage.from_json(obj.get("usage")),
            raw=obj,
        )


@dataclass
class Waking:
    """What a check on a durable timer found.

    ``woken`` is ``True`` when the sleep is over: either the log already held
    the ``SleepCompleted`` (a replay) or the deadline had passed and this call
    recorded it. ``False`` means the run is still asleep and nothing was
    appended; stop driving and come back later.

    ``wake_at`` is the deadline this drive measured against, which is the
    instant :meth:`~salvor.client_runs.ClientRunDriver.sleep_until` or
    :meth:`~salvor.client_runs.ClientRunDriver.sleep_for` recorded earlier in
    the same drive. It is ``None`` when this drive set no deadline at all, which
    is also why such a drive always reports still asleep: a wake nobody asked
    for has not arrived, and no clock reading will make it so.
    """

    woken: bool
    wake_at: Optional[datetime] = None


@dataclass
class OpenedRun:
    """What opening (or re-opening) a client-driven run hands back: the run's
    id, the fresh single-writer lease, and the log recorded so far."""

    run_id: str
    drive_token: str
    log: list[Event]


# -- the wire ------------------------------------------------------------------


def lease(drive_token: str) -> dict[str, str]:
    """The single-writer lease header every writing call presents."""
    return {"X-Drive-Token": drive_token}


def open_run(
    agent: Optional[str],
    input: Any,
    run_id: Optional[str],
    record_prompts: bool,
    drive_token: Optional[str] = None,
) -> Call:
    """Open a fresh run, or re-open one this server holds.

    ``drive_token`` is the held lease's own token, presented on a re-open:
    salvor returns the SAME token back rather than minting a fresh one, so a
    client rebuilding its cursor is not made to give up the lease it holds
    (see ``API.md``'s drive-token section). Omit it to re-open without
    presenting one, which is refused with ``409 lease_held`` while another
    driver's lease on the run is still current, and succeeds with a fresh
    lease otherwise (an unheld run, a lapsed lease, or a run this server only
    knows from its log after a restart).
    """
    body: dict[str, Any] = {"record_prompts": record_prompts}
    if agent is not None:
        body["agent"] = agent
    if input is not None:
        body["input"] = input
    if run_id is not None:
        body["run_id"] = run_id

    def parse(obj: dict[str, Any]) -> OpenedRun:
        return OpenedRun(
            run_id=obj["run"],
            drive_token=obj["drive_token"],
            log=[Event.from_envelope(e) for e in obj.get("log", [])],
        )

    headers = lease(drive_token) if drive_token is not None else None
    return Call("POST", "/v1/client-runs", parse=parse, json_body=body, headers=headers)


def release(run_id: str, drive_token: str) -> Call:
    """Hand the drive-token lease back, so the next open takes the run at once.

    Answers ``True`` when a lease was given back and ``False`` when there was
    none to give: released already, lapsed, or a run this server never opened.
    Finding nothing is not an error, because the caller's goal (a run nobody
    holds) is already true. A lease that stands and is not this caller's is
    ``403 invalid_drive_token``, a missing token included, and nothing is
    dropped.

    Lapsing is the safety net, not how a drive ends: without this call a
    short-lived process locks out the process after it for up to the lease TTL
    for nothing. Only the lease goes; the log stays readable and the run stays
    client-driven, so the next open adopts it as it would after a restart.
    """
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/release",
        parse=lambda obj: bool(obj.get("released", False)),
        headers=lease(drive_token),
    )


def heartbeat(run_id: str, drive_token: str) -> Call:
    """Say "still here" without driving the run, and hear how long the lease has.

    Answers the whole seconds until the lease lapses if this driver goes quiet
    from now (rounded up, never below ``1``), so a driver picks its interval
    from the answer rather than being told the server's configuration some
    other way. For the driver that makes no drive call for longer than the TTL
    because it is inside one long body: a tool that takes minutes, a model call
    it is streaming to its own screen.
    """
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/heartbeat",
        parse=lambda obj: int(obj.get("lapses_in_seconds", 0)),
        headers=lease(drive_token),
    )


def read_log(run_id: str, from_seq: int) -> Call:
    return Call(
        "GET",
        f"/v1/client-runs/{run_id}/log",
        parse=lambda obj: [Event.from_envelope(e) for e in obj.get("log", [])],
        params={"from_seq": from_seq},
    )


def append(run_id: str, drive_token: str, events: list[dict[str, Any]]) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/events",
        parse=lambda obj: list(obj.get("appended", [])),
        json_body={"events": events},
        headers=lease(drive_token),
    )


def model_step(run_id: str, drive_token: str, seq: int, request: Any) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/model-step",
        parse=ModelStepResult.from_json,
        json_body={"seq": seq, "request": request},
        headers=lease(drive_token),
    )


def model_step_stream(run_id: str, drive_token: str, seq: int, request: Any) -> Call:
    """The same call as :func:`model_step`, asking for the live ticker. Only the
    ``Accept`` header differs, which is why a streaming retry of a completed
    step lands on the same recorded completion.

    The body is a frame stream rather than a JSON document, so ``parse`` is
    never used; the reader takes the request and reads the frames itself with
    :func:`~salvor._core.sse.model_step_frame`.
    """
    headers = lease(drive_token)
    headers["Accept"] = "text/event-stream"
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/model-step",
        parse=identity,
        json_body={"seq": seq, "request": request},
        headers=headers,
    )


def tool_step(
    run_id: str,
    drive_token: str,
    seq: int,
    tool: str,
    input: Any,
    idempotency_key: Optional[str],
) -> Call:
    body: dict[str, Any] = {"seq": seq, "tool": tool, "input": input}
    if idempotency_key is not None:
        body["idempotency_key"] = idempotency_key
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/tool-step",
        parse=lambda obj: obj.get("output"),
        json_body=body,
        headers=lease(drive_token),
    )


def client_tool_intent(
    run_id: str, drive_token: str, seq: int, tool: str, input: Any
) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/client-tool-intent",
        parse=ClientToolIntentResult.from_json,
        json_body={"seq": seq, "tool": tool, "input": input},
        headers=lease(drive_token),
    )


def client_tool_completion(
    run_id: str, drive_token: str, seq: int, output: Any
) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/client-tool-completion",
        parse=discard,
        json_body={"seq": seq, "output": output},
        headers=lease(drive_token),
    )


def client_tool_failure(
    run_id: str, drive_token: str, seq: int, message: str, kind: str = "handler"
) -> Call:
    """Report that a client-performed tool call failed rather than returned.

    Hits the same endpoint as :func:`client_tool_completion`, on the ``error``
    shape rather than the ``output`` one: the server records the same
    ``__salvor_error`` sentinel completion a native tool's exhausted retries
    write, so the call is settled -- closed, and replayed as the failure
    rather than performed again -- exactly as a reported output settles it.
    ``kind`` is one of ``"invalid_input"``, ``"handler"`` or
    ``"output_serialization"``, the dispatch layer that failed; a tool body
    that ran and raised is ``"handler"``, the default. A declaration written
    ``trust_completion = false`` refuses this the same way it refuses a
    reported output.
    """
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/client-tool-completion",
        parse=discard,
        json_body={"seq": seq, "error": {"message": message, "kind": kind}},
        headers=lease(drive_token),
    )


def client_model_intent(
    run_id: str,
    drive_token: str,
    seq: int,
    request_hash: str,
    request_body: Any = None,
) -> Call:
    body: dict[str, Any] = {"seq": seq, "request_hash": request_hash}
    if request_body is not None:
        body["request_body"] = request_body
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/client-model-intent",
        parse=ClientModelIntentResult.from_json,
        json_body=body,
        headers=lease(drive_token),
    )


def client_model_completion(
    run_id: str, drive_token: str, seq: int, response: Any, usage: dict[str, int]
) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/client-model-completion",
        parse=discard,
        json_body={"seq": seq, "response": response, "usage": usage},
        headers=lease(drive_token),
    )


def resolve(run_id: str, drive_token: str, output: Any) -> Call:
    return Call(
        "POST",
        f"/v1/client-runs/{run_id}/resolve",
        parse=discard,
        json_body={"output": output},
        headers=lease(drive_token),
    )


# -- envelopes -----------------------------------------------------------------


def envelope(run_id: str, seq: int, kind: str, **payload: Any) -> dict[str, Any]:
    """Build one event-envelope for the generic append at ``seq``.

    It wraps ``kind`` and ``payload`` in the pinned envelope shape the log and
    the event stream use, filling ``run_id`` and a fixed ``schema_version``.
    ``recorded_at`` is a client-side placeholder; the server stamps the
    authoritative time when it records the event.
    """
    return {
        "run_id": run_id,
        "seq": seq,
        "schema_version": 1,
        "recorded_at": "1970-01-01T00:00:00Z",
        "event": {"kind": kind, "payload": payload},
    }


# -- the durable-timer rules ----------------------------------------------------


@dataclass
class TimerStep:
    """One durable-timer verdict, reached from the log alone.

    ``events`` is what the driver must append, empty when the log already holds
    the answer; ``sleeping_until`` is the deadline this drive carries afterwards,
    which the driver stores only once the append has landed. A rule that decides
    nothing new returns the deadline it was handed, unchanged.
    """

    result: Any
    events: list[dict[str, Any]]
    sleeping_until: Optional[datetime]


def event_at(tail: list[Event], seq: int) -> Optional[Event]:
    """The recorded event at ``seq`` in a log tail read from ``seq``, or
    ``None`` when the log has not reached that position yet."""
    if tail and tail[0].seq == seq:
        return tail[0]
    return None


def now_step(
    run_id: str,
    seq: int,
    recorded: Optional[Event],
    clock: Callable[[], datetime],
    sleeping_until: Optional[datetime],
) -> TimerStep:
    """Observe the clock at ``seq``, recording the reading the first time.

    A ``NowObserved`` already at ``seq`` is the answer, so a later drive replays
    the identical instant; otherwise the clock is read once and the reading goes
    into the log. A reading taken outside the log means nothing to a replay,
    which has no clock of its own to interpret it against.
    """
    if recorded is not None and recorded.kind == "NowObserved":
        return TimerStep(parse_rfc3339(recorded.payload["now"]), [], sleeping_until)
    reading = clock()
    return TimerStep(
        reading,
        [envelope(run_id, seq, "NowObserved", now=rfc3339(reading))],
        sleeping_until,
    )


def sleep_step(
    run_id: str,
    seq: int,
    wake_at: datetime,
    recorded: Optional[Event],
) -> TimerStep:
    """Park the run on a durable timer at ``seq``.

    A position already holding this exact park is a replay: nothing is appended.
    A position holding a DIFFERENT one is submitted anyway, so the server refuses
    it as the divergence it is rather than a driver quietly preferring one of the
    two instants.
    """
    if recorded is not None and recorded.kind == "SleepStarted":
        already = parse_rfc3339(recorded.payload["wake_at"])
        if already == wake_at:
            return TimerStep(already, [], already)
    return TimerStep(
        wake_at,
        [envelope(run_id, seq, "SleepStarted", wake_at=rfc3339(wake_at))],
        wake_at,
    )


def wake_step(
    run_id: str,
    seq: int,
    recorded: Optional[Event],
    clock: Callable[[], datetime],
    sleeping_until: Optional[datetime],
) -> TimerStep:
    """Ask whether the sleep is over, closing the pair at ``seq`` if it is.

    The log decides first: a ``SleepCompleted`` already recorded at ``seq`` means
    the sleep ended on an earlier drive, so this replays it and appends nothing.
    Otherwise the clock decides, against the deadline this drive recorded
    earlier. A drive that set no deadline has none that could have arrived, so it
    stays asleep. Nothing here can wake a run early.
    """
    if recorded is not None and recorded.kind == "SleepCompleted":
        return TimerStep(Waking(True, sleeping_until), [], None)
    if sleeping_until is None or clock() < sleeping_until:
        return TimerStep(Waking(False, sleeping_until), [], sleeping_until)
    return TimerStep(
        Waking(True, sleeping_until),
        [envelope(run_id, seq, "SleepCompleted")],
        None,
    )


# -- instants -------------------------------------------------------------------


def utc_now() -> datetime:
    """The default clock the durable-timer methods read: the current instant,
    timezone-aware and in UTC, which is the only form the log records."""
    return datetime.now(timezone.utc)


def rfc3339(instant: datetime) -> str:
    """Format an instant the way every recorded timestamp on the wire is
    formatted: UTC, RFC 3339, with the ``Z`` suffix.

    A naive datetime is refused rather than assumed to be UTC. Guessing an
    offset here would put an instant in the log that is wrong by hours, and a
    recorded instant is the one thing a later drive cannot re-derive.
    """
    if instant.tzinfo is None:
        raise ValueError(
            "a recorded instant must be timezone-aware; pass a UTC datetime "
            "(datetime.now(timezone.utc)) rather than a naive one"
        )
    return instant.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def parse_rfc3339(text: str) -> datetime:
    """Decode a recorded RFC 3339 instant.

    Tolerates the two things the server writes that Python's own parser has not
    always taken: the ``Z`` suffix, and more fractional digits than microsecond
    precision holds. Extra digits are truncated rather than rejected, because a
    reading this driver recorded at microsecond precision comes back at the
    precision the store kept it.
    """
    value = text.strip()
    if value.endswith(("Z", "z")):
        value = value[:-1] + "+00:00"
    head, dot, tail = value.partition(".")
    if dot:
        digits = ""
        for char in tail:
            if not char.isdigit():
                break
            digits += char
        value = f"{head}.{digits[:6].ljust(6, '0')}{tail[len(digits):]}"
    return datetime.fromisoformat(value)
