"""The client-driven run driver: Salvor's second mode.

In the server-driven mode (:class:`salvor.Client`) the server owns the agent
loop and drives it in a background task. In the client-driven mode this module
inverts who owns the loop while keeping who owns the log. The client (this
driver, or a browser folding a run's log in a wasm cursor) owns the loop and
streams the events it produces; the server owns the durable log and, on every
append, re-folds it with the pure append-guard to confirm the incoming event is
the one legal next event.

The generic append carries only the control and deterministic-context events the
client emits itself, which hold no secret and no side effect (``RunStarted``,
``NowObserved``, ``RandomObserved``, ``Suspended``, ``Resumed``,
``SleepStarted``, ``SleepCompleted``, ``BudgetExceeded``, ``RunCompleted``,
``RunFailed``). The side-effecting steps, which the server must perform because
it holds the key or the binary, have their own methods:
:meth:`~ClientRunDriver.model_step` and :meth:`~ClientRunDriver.tool_step`.

A client-driven run may park on a durable timer, and the client is what wakes
it. Nothing on the server waits for the deadline: the wake sweeper leaves every
client-driven run alone, because re-driving one there would be a second writer
racing this driver's lease. So :meth:`~ClientRunDriver.sleep_for` and
:meth:`~ClientRunDriver.sleep_until` record the park, and
:meth:`~ClientRunDriver.await_wake` on a later drive reads the clock and either
stops (still asleep, nothing appended) or closes the pair and carries on. The
methods carry the runtime's names because they carry the runtime's rules.

    from salvor import Client

    with Client("http://127.0.0.1:8080") as client:
        run = client.open_client_run()
        run.append([run.envelope(0, "RunStarted",
                                 agent_def_hash=agent, input=task)])
        result = run.model_step(1, request)
        run.append([run.envelope(3, "RunCompleted", output=answer)])

Unlike :meth:`salvor.Client.start_run`, this driver has no dedicated ``labels``
parameter: the client builds ``RunStarted`` itself, so correlation tags simply
ride in that call's keyword arguments, e.g.
``run.envelope(0, "RunStarted", agent_def_hash=agent, input=task,
labels={"build": "42"})``. The server enforces the same bounds on append (see
``API.md``) as it does for a server-driven start.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, Callable, Iterator, Optional

import httpx

from .errors import SalvorStreamError, decode_error
from .models import Event, Usage


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
    the log.
    """

    seq: int
    idempotency_key: str
    effect: str
    settled: bool
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, obj: dict[str, Any]) -> "ClientToolIntentResult":
        return cls(
            seq=int(obj.get("seq", 0)),
            idempotency_key=obj.get("idempotency_key", ""),
            effect=obj.get("effect", ""),
            settled=bool(obj.get("settled", False)),
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
    instant :meth:`~ClientRunDriver.sleep_until` or
    :meth:`~ClientRunDriver.sleep_for` recorded earlier in the same drive. It is
    ``None`` when this drive set no deadline at all, which is also why such a
    drive always reports still asleep: a wake nobody asked for has not arrived,
    and no clock reading will make it so.
    """

    woken: bool
    wake_at: Optional[datetime] = None


class ModelStepStream:
    """The live ticker of a streaming model step.

    Iterate it to receive each ticker delta the provider emits while the call
    runs: a ``{"type": "text_delta", ...}`` or ``{"type": "thinking_delta",
    ...}`` payload, or a final ``{"type": "usage", ...}``. Iteration stops when
    the assembled completion arrives; :attr:`completion` then holds the
    :class:`ModelStepResult`, byte-identical to what the non-streaming path would
    have recorded. A mid-stream provider failure raises
    :class:`~salvor.errors.SalvorStreamError`.
    """

    def __init__(self) -> None:
        self.completion: Optional[ModelStepResult] = None
        self._gen: Optional[Iterator[dict[str, Any]]] = None

    def __iter__(self) -> "ModelStepStream":
        return self

    def __next__(self) -> dict[str, Any]:
        assert self._gen is not None
        return next(self._gen)


class ClientRunDriver:
    """Drives one client-driven run against a Salvor control plane.

    Open one with :meth:`salvor.Client.open_client_run` or :meth:`open`. The
    driver holds the run id and the current ``drive_token`` (the single-writer
    lease every append presents), plus the log returned when re-opening, so a
    resuming client rebuilds its cursor from :attr:`log_envelopes` without a
    second fetch.
    """

    def __init__(
        self,
        http: httpx.Client,
        *,
        run_id: str,
        drive_token: str,
        log: list[Event],
        owns_http: bool,
        stream_timeout: httpx.Timeout,
    ) -> None:
        self._http = http
        self._owns_http = owns_http
        self._stream_timeout = stream_timeout
        self.run_id = run_id
        self.drive_token = drive_token
        #: The envelopes returned when this run was opened. Empty for a fresh
        #: run; the full recorded log for a re-open, ready to rebuild a cursor.
        self.log_envelopes = log
        #: The clock the durable-timer methods read, returning a timezone-aware
        #: datetime. Replaceable, the way the runtime injects its own clock, so
        #: a test can drive a deadline past without waiting for it.
        self.clock: Callable[[], datetime] = _utc_now
        # The deadline set earlier in THIS drive, live or replayed. The runtime
        # keeps the same one on its context, and for the same reason: what
        # `await_wake` compares against is the instant the log recorded, never
        # a duration or a fresh reading.
        self._sleeping_until: Optional[datetime] = None

    # -- construction ---------------------------------------------------------

    @classmethod
    def open(
        cls,
        base_url: str,
        *,
        agent: Optional[str] = None,
        input: Any = None,
        run_id: Optional[str] = None,
        record_prompts: bool = False,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> "ClientRunDriver":
        """Open a fresh client-driven run, or re-open (resume) an existing one.

        Passing a ``run_id`` this server already holds re-opens it: the recorded
        log comes back on :attr:`log_envelopes` and a fresh lease is minted, so a
        resuming client always holds the current one and the superseded lease
        stops working. Omitting ``run_id`` opens a fresh run the server mints an
        id for.

        ``agent`` and ``input`` are accepted for forward compatibility; this
        surface records them nowhere, because the client appends its own
        ``RunStarted`` (carrying the agent hash and input) as the run's first
        event. ``record_prompts`` is stored against the run and controls whether
        a later :meth:`model_step` records the request body on its intent.

        The driver owns its own HTTP connection; close it with :meth:`close`.
        """
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        http = httpx.Client(base_url=base_url.rstrip("/"), headers=headers, timeout=timeout)
        stream_timeout = httpx.Timeout(timeout, read=None)
        return cls._open_over(
            http,
            owns_http=True,
            stream_timeout=stream_timeout,
            agent=agent,
            input=input,
            run_id=run_id,
            record_prompts=record_prompts,
        )

    @classmethod
    def _open_over(
        cls,
        http: httpx.Client,
        *,
        owns_http: bool,
        stream_timeout: httpx.Timeout,
        agent: Optional[str],
        input: Any,
        run_id: Optional[str],
        record_prompts: bool,
    ) -> "ClientRunDriver":
        body: dict[str, Any] = {"record_prompts": record_prompts}
        if agent is not None:
            body["agent"] = agent
        if input is not None:
            body["input"] = input
        if run_id is not None:
            body["run_id"] = run_id
        resp = http.post("/v1/client-runs", json=body)
        obj = cls._json(resp)
        log = [Event.from_envelope(e) for e in obj.get("log", [])]
        return cls(
            http,
            run_id=obj["run"],
            drive_token=obj["drive_token"],
            log=log,
            owns_http=owns_http,
            stream_timeout=stream_timeout,
        )

    def close(self) -> None:
        """Close the HTTP connection, if this driver owns it."""
        if self._owns_http:
            self._http.close()

    def __enter__(self) -> "ClientRunDriver":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # -- building envelopes ---------------------------------------------------

    def envelope(self, seq: int, kind: str, **payload: Any) -> dict[str, Any]:
        """Build one event-envelope for :meth:`append` at ``seq``.

        A convenience for the control and context events the client emits: it
        wraps ``kind`` and ``payload`` in the pinned envelope shape the log and
        the event stream use, filling ``run_id`` and a fixed ``schema_version``.
        ``recorded_at`` is a client-side placeholder; the server stamps the
        authoritative time when it records the event. For example::

            run.envelope(0, "RunStarted", agent_def_hash=agent, input=task)
            run.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z")
            run.envelope(3, "RunCompleted", output={"done": True})
        """
        return {
            "run_id": self.run_id,
            "seq": seq,
            "schema_version": 1,
            "recorded_at": "1970-01-01T00:00:00Z",
            "event": {"kind": kind, "payload": payload},
        }

    # -- log ------------------------------------------------------------------

    def log(self, from_seq: int = 0) -> list[Event]:
        """Read the recorded log back, for a refreshed client to rebuild its cursor.

        Returns every recorded envelope at or after ``from_seq`` as a typed
        :class:`~salvor.models.Event` list. A client that already holds a prefix
        passes ``from_seq`` to fetch just the tail. The read needs no drive token.
        """
        resp = self._http.get(
            f"/v1/client-runs/{self.run_id}/log", params={"from_seq": from_seq}
        )
        obj = self._json(resp)
        return [Event.from_envelope(e) for e in obj.get("log", [])]

    # -- generic append -------------------------------------------------------

    def append(self, events: list[dict[str, Any]]) -> list[int]:
        """Append control and context events, guarded against the durable log.

        The whole batch is validated before anything is written, so a batch that
        turns illegal appends nothing. Returns the sequence numbers recorded.

        The append is retry-safe: re-appending byte-identical events at
        already-recorded positions is a no-op that still reports those sequence
        numbers and does not grow the log (the case a client hits when it resends
        after a network blip). Different bytes at a recorded position, or an
        event that is not the legal next one, raises
        :class:`~salvor.errors.DivergenceError`; a model or tool event raises a
        ``SalvorAPIError`` with code ``unsupported_event_kind`` (those go through
        :meth:`model_step` and :meth:`tool_step`).
        """
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/events",
            json={"events": events},
            headers=self._lease(),
        )
        return list(self._json(resp).get("appended", []))

    # -- model step -----------------------------------------------------------

    def model_step(self, seq: int, request: Any) -> ModelStepResult:
        """Perform and record a model call the server makes (it holds the key).

        ``seq`` is the log position the client's cursor reserved for the model
        intent; ``request`` is the canonical model request. The server records
        the intent write-ahead, performs the call, records the completion, and
        returns it. Retry identity is ``(seq, request_hash)``: a step already
        completed at ``seq`` with the same request returns the recorded
        completion without calling the provider again (the no-re-pay case), while
        a different request there raises
        :class:`~salvor.errors.DivergenceError`.
        """
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/model-step",
            json={"seq": seq, "request": request},
            headers=self._lease(),
        )
        return ModelStepResult.from_json(self._json(resp))

    def model_step_stream(self, seq: int, request: Any) -> ModelStepStream:
        """Perform a model step with a live ticker, over a server-sent stream.

        Same recording and retry semantics as :meth:`model_step`, but the
        response is a stream: iterate the returned :class:`ModelStepStream` to
        paint each ticker delta as it arrives, then read the assembled completion
        from its :attr:`~ModelStepStream.completion`. The recorded completion is
        byte-identical to the non-streaming path; a step that resolves to a
        replay streams a single completion carrying the recorded one.
        """
        stream = ModelStepStream()
        stream._gen = self._stream_model(seq, request, stream)
        return stream

    def _stream_model(
        self, seq: int, request: Any, stream: ModelStepStream
    ) -> Iterator[dict[str, Any]]:
        headers = self._lease()
        headers["Accept"] = "text/event-stream"
        with self._http.stream(
            "POST",
            f"/v1/client-runs/{self.run_id}/model-step",
            json={"seq": seq, "request": request},
            headers=headers,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                raise decode_error(resp.status_code, resp.read())
            for name, data in _sse_frames(resp.iter_lines()):
                if name == "delta":
                    yield json.loads(data)
                elif name == "complete":
                    stream.completion = ModelStepResult.from_json(json.loads(data))
                    return
                elif name == "error":
                    message = json.loads(data).get("message", "model step failed")
                    raise SalvorStreamError(f"model step for run {self.run_id}: {message}")

    # -- tool step ------------------------------------------------------------

    def tool_step(
        self,
        seq: int,
        tool: str,
        input: Any,
        idempotency_key: Optional[str] = None,
    ) -> Any:
        """Perform and record a tool call the server makes (it holds the binary).

        ``seq`` is the reserved log position; ``tool`` names a tool the server's
        registry holds; ``input`` is recorded on the intent verbatim.
        ``idempotency_key`` is optional; for an idempotent tool draw it from a
        recorded ``RandomObserved`` so it reproduces on replay. The recorded
        effect is the tool's operator-declared one, so a client cannot up- or
        down-grade it. Returns the tool's output.

        Retry follows the effect table: a completed step returns the recorded
        output without re-dispatching; a dangling ``Write`` intent raises
        :class:`~salvor.errors.NeedsReconciliationError` carrying the recorded
        intent, and only :meth:`resolve` may record its completion.
        """
        body: dict[str, Any] = {"seq": seq, "tool": tool, "input": input}
        if idempotency_key is not None:
            body["idempotency_key"] = idempotency_key
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/tool-step",
            json=body,
            headers=self._lease(),
        )
        return self._json(resp).get("output")

    # -- client-performed tool calls -------------------------------------------

    def client_tool_intent(self, seq: int, tool: str, input: Any) -> ClientToolIntentResult:
        """Open a tool call the CLIENT performs, in its own process, with its
        own secrets.

        ``seq`` is the log position the client's cursor reserved for the
        intent; ``tool`` names a tool an operator declared with ``salvor serve
        --client-tool <FILE>`` (never registered over HTTP; see
        :meth:`salvor.Client.list_client_tools` to fetch what is declared);
        ``input`` is checked against the declaration's input schema before
        anything is written.

        The returned ``idempotency_key`` comes FROM the server, not from the
        caller. It is a derived hash of ``(run, seq, tool)``, and the client
        must perform its call under that exact key. This is why: it is what
        stops a retry becoming a second charge, so the party who would
        benefit from a duplicate landing does not get to choose the key that
        lets one through. This is the one place this driver differs from
        :meth:`tool_step` on purpose: there the caller supplies the key,
        because salvor performs the call itself and handing it the key is
        safe; here the client both performs the call and stands to gain from
        a duplicate, so the server derives the key instead of accepting one.

        The returned ``settled`` is ``True`` when the intent at ``seq`` already
        has its completion recorded, ``False`` otherwise. A payments caller
        retrying this call after a dropped response gets back the same key
        either way; ``settled`` is what lets it tell "safe to perform the
        call" from "already done, do not perform it again" without a separate
        log read.

        Raises :class:`~salvor.errors.SalvorAPIError` with code
        ``unknown_tool`` for an undeclared tool, or ``bad_request`` when
        ``input`` fails the declaration's schema; a ``seq`` the log is not
        ready for, or a different event already recorded there, raises
        :class:`~salvor.errors.DivergenceError`.
        """
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/client-tool-intent",
            json={"seq": seq, "tool": tool, "input": input},
            headers=self._lease(),
        )
        return ClientToolIntentResult.from_json(self._json(resp))

    def client_tool_completion(self, seq: int, output: Any) -> None:
        """Report what a client-performed tool call returned.

        ``seq`` must name the pending intent at the end of the log; ``output``
        is checked against the declaration's output schema before it is
        recorded.

        Refused, recording nothing, as a ``SalvorAPIError`` with code
        ``client_completion_refused`` when: the declaration was written with
        ``trust_completion = False``, or it carries no output schema at all.
        Either way there is nothing this call can trust, so settle it by hand
        instead with :meth:`resolve` once you have verified the result
        externally. A reported ``output`` that fails the declared schema is
        ``bad_request``; there the fix is the output, not the call.
        """
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/client-tool-completion",
            json={"seq": seq, "output": output},
            headers=self._lease(),
        )
        self._json(resp)

    # -- durable timers --------------------------------------------------------

    def now(self, seq: int) -> datetime:
        """Observe the clock at ``seq``, recording the reading the first time.

        Returns the recorded reading when ``seq`` already holds a
        ``NowObserved``, so a later drive replays the identical instant, and
        otherwise reads :attr:`clock`, appends it, and returns it. This is the
        one way a client-driven run gets time into its log: a reading taken
        outside the log means nothing to a replay, which has no clock of its
        own to interpret it against.
        """
        recorded = self._event_at(seq)
        if recorded is not None and recorded.kind == "NowObserved":
            return _parse_rfc3339(recorded.payload["now"])
        reading = self.clock()
        self.append([self.envelope(seq, "NowObserved", now=_rfc3339(reading))])
        return reading

    def sleep_until(self, seq: int, wake_at: datetime) -> datetime:
        """Park the run on a durable timer at ``seq``, returning ``wake_at``.

        ``wake_at`` must be derived from recorded data, because a later drive
        presents it again and it has to be the same instant: derive it from an
        observed :meth:`now` (which :meth:`sleep_for` does for you), never from
        a clock read outside the log. A position already holding this exact
        park is a replay: nothing is appended. A position holding a DIFFERENT
        one is submitted anyway, so the server refuses it as the divergence it
        is rather than this driver quietly preferring one of the two instants.

        Follow it with :meth:`await_wake`. Never park between a write tool's
        intent and its completion: the run holds that call's claim for the whole
        sleep, which for a durable timer is hours or weeks.
        """
        recorded = self._event_at(seq)
        if recorded is not None and recorded.kind == "SleepStarted":
            already = _parse_rfc3339(recorded.payload["wake_at"])
            if already == wake_at:
                self._sleeping_until = already
                return already
        self.append([self.envelope(seq, "SleepStarted", wake_at=_rfc3339(wake_at))])
        self._sleeping_until = wake_at
        return wake_at

    def sleep_for(self, seq: int, duration: timedelta) -> datetime:
        """Sleep for ``duration`` from a recorded reading of the clock, and
        return the wake instant it recorded.

        Exactly ``now() + duration``, recorded: the reading goes into the log at
        ``seq`` as a ``NowObserved`` before the park is derived from it, and the
        park lands at ``seq + 1``. So every later drive replays the identical
        reading and derives the identical instant, which is what a duration
        alone can never do. Carries every rule :meth:`sleep_until` does.
        """
        return self.sleep_until(seq + 1, self.now(seq) + duration)

    def await_wake(self, seq: int) -> Waking:
        """Ask whether the sleep is over, closing the pair at ``seq`` if it is.

        The log decides first: a ``SleepCompleted`` already recorded at ``seq``
        means the sleep ended on an earlier drive, so this replays it and
        appends nothing. Otherwise :attr:`clock` decides, against the deadline
        :meth:`sleep_until` or :meth:`sleep_for` recorded earlier in this same
        drive. At or past it the completion is appended and the run carries on;
        before it, nothing is appended and the returned
        :class:`Waking` reports the run still asleep, which is the signal to
        stop driving and come back later.

        Nothing here can wake a run early, and that is deliberate: a driver that
        comes back too soon simply finds it still asleep, exactly as the server
        wakes nothing before its instant.
        """
        recorded = self._event_at(seq)
        if recorded is not None and recorded.kind == "SleepCompleted":
            wake_at = self._sleeping_until
            self._sleeping_until = None
            return Waking(True, wake_at)
        # A drive that set no deadline has none that could have arrived, so it
        # stays asleep, mirroring the runtime's stand-in for the same case.
        if self._sleeping_until is None or self.clock() < self._sleeping_until:
            return Waking(False, self._sleeping_until)
        self.append([self.envelope(seq, "SleepCompleted")])
        wake_at = self._sleeping_until
        self._sleeping_until = None
        return Waking(True, wake_at)

    def _event_at(self, seq: int) -> Optional[Event]:
        """The recorded event at ``seq``, or ``None`` when the log has not
        reached that position yet.

        One log read, deliberately: the durable-timer methods are called once
        per drive apiece, and a driver that has been away for a week cannot
        trust anything it cached before it left.
        """
        tail = self.log(from_seq=seq)
        if tail and tail[0].seq == seq:
            return tail[0]
        return None

    # -- resolve --------------------------------------------------------------

    def resolve(self, output: Any) -> None:
        """Record a dangling write's completion by hand, unsticking the run.

        Legal only when the run's log ends at a dangling ``Write`` intent: it
        correlates ``output`` to that intent and dispatches nothing. After it
        records the completion the run drives again, so re-fetch :meth:`log` and
        the cursor sails past the once-dangling intent. Raises a ``SalvorAPIError``
        with code ``wrong_state`` when there is no dangling write.
        """
        resp = self._http.post(
            f"/v1/client-runs/{self.run_id}/resolve",
            json={"output": output},
            headers=self._lease(),
        )
        self._json(resp)

    # -- helpers --------------------------------------------------------------

    def _lease(self) -> dict[str, str]:
        return {"X-Drive-Token": self.drive_token}

    @staticmethod
    def _json(resp: httpx.Response) -> dict[str, Any]:
        if resp.status_code // 100 != 2:
            raise decode_error(resp.status_code, resp.content)
        if not resp.content:
            return {}
        return resp.json()


def _utc_now() -> datetime:
    """The default clock the durable-timer methods read: the current instant,
    timezone-aware and in UTC, which is the only form the log records."""
    return datetime.now(timezone.utc)


def _rfc3339(instant: datetime) -> str:
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


def _parse_rfc3339(text: str) -> datetime:
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


def _sse_frames(lines: Iterator[str]) -> Iterator[tuple[str, str]]:
    """Parse a server-sent-events line stream into ``(event, data)`` frames.

    The same line protocol :class:`salvor.Client` reads for the event tail:
    ``event:`` and ``data:`` fields accumulate until a blank line dispatches the
    frame. A frame with no ``event:`` field defaults to ``"message"``.
    """
    event_name: Optional[str] = None
    data_lines: list[str] = []
    for line in lines:
        if line == "":
            if data_lines:
                yield (event_name or "message", "\n".join(data_lines))
            event_name = None
            data_lines = []
            continue
        if line.startswith(":"):
            continue
        field_name, _, value = line.partition(":")
        if value.startswith(" "):
            value = value[1:]
        if field_name == "event":
            event_name = value
        elif field_name == "data":
            data_lines.append(value)
    if data_lines:
        yield (event_name or "message", "\n".join(data_lines))
