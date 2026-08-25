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

Every rule this driver applies, the wire shapes and the durable-timer
arithmetic alike, lives in :mod:`salvor._core.driver`, which knows nothing about
sockets. :class:`salvor.AsyncClientRunDriver` is this same surface awaited, over
the same core. What differs between them is the sending; nothing else.
"""

from __future__ import annotations

from datetime import datetime, timedelta
from typing import Any, Callable, Iterator, Optional

import httpx

from ._core import api, driver as rules, wire
from ._core.driver import (
    ClientToolIntentResult,
    ModelStepResult,
    Waking,
    utc_now as _utc_now,
)
from ._core.sse import frames as _sse_frames, model_step_frame
from ._core.wire import Call
from .models import Event

__all__ = [
    "ClientRunDriver",
    "ClientToolIntentResult",
    "ModelStepResult",
    "ModelStepStream",
    "Waking",
]


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
        base, headers = api.connection(base_url, token)
        http = httpx.Client(base_url=base, headers=headers, timeout=timeout)
        return cls._open_over(
            http,
            owns_http=True,
            stream_timeout=httpx.Timeout(timeout, read=None),
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
        call = rules.open_run(agent, input, run_id, record_prompts)
        resp = http.request(call.method, call.path, **wire.request_kwargs(call))
        opened = call.parse(wire.decode_json(resp.status_code, resp.content))
        return cls(
            http,
            run_id=opened.run_id,
            drive_token=opened.drive_token,
            log=opened.log,
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
        return rules.envelope(self.run_id, seq, kind, **payload)

    # -- log ------------------------------------------------------------------

    def log(self, from_seq: int = 0) -> list[Event]:
        """Read the recorded log back, for a refreshed client to rebuild its cursor.

        Returns every recorded envelope at or after ``from_seq`` as a typed
        :class:`~salvor.models.Event` list. A client that already holds a prefix
        passes ``from_seq`` to fetch just the tail. The read needs no drive token.
        """
        return self._send(rules.read_log(self.run_id, from_seq))

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
        return self._send(rules.append(self.run_id, self.drive_token, events))

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
        return self._send(
            rules.model_step(self.run_id, self.drive_token, seq, request)
        )

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
        call = rules.model_step_stream(self.run_id, self.drive_token, seq, request)
        with self._http.stream(
            call.method,
            call.path,
            json=call.json_body,
            headers=call.headers,
            timeout=self._stream_timeout,
        ) as resp:
            if resp.status_code != 200:
                raise wire.error(resp.status_code, resp.read())
            for frame in _sse_frames(resp.iter_lines()):
                what, value = model_step_frame(self.run_id, frame)
                if what == "delta":
                    yield value
                elif what == "complete":
                    stream.completion = value
                    return

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
        return self._send(
            rules.tool_step(
                self.run_id, self.drive_token, seq, tool, input, idempotency_key
            )
        )

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
        return self._send(
            rules.client_tool_intent(self.run_id, self.drive_token, seq, tool, input)
        )

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
        self._send(
            rules.client_tool_completion(self.run_id, self.drive_token, seq, output)
        )

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
        return self._timed(
            rules.now_step(
                self.run_id, seq, self._event_at(seq), self.clock, self._sleeping_until
            )
        )

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
        return self._timed(
            rules.sleep_step(self.run_id, seq, wake_at, self._event_at(seq))
        )

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
        return self._timed(
            rules.wake_step(
                self.run_id, seq, self._event_at(seq), self.clock, self._sleeping_until
            )
        )

    def _timed(self, step: rules.TimerStep) -> Any:
        """Carry out one durable-timer verdict: append what it asks for, then
        adopt the deadline it leaves behind. The deadline moves only once the
        append has landed, so a refused park leaves this drive as it was."""
        if step.events:
            self.append(step.events)
        self._sleeping_until = step.sleeping_until
        return step.result

    def _event_at(self, seq: int) -> Optional[Event]:
        """The recorded event at ``seq``, or ``None`` when the log has not
        reached that position yet.

        One log read, deliberately: the durable-timer methods are called once
        per drive apiece, and a driver that has been away for a week cannot
        trust anything it cached before it left.
        """
        return rules.event_at(self.log(from_seq=seq), seq)

    # -- resolve --------------------------------------------------------------

    def resolve(self, output: Any) -> None:
        """Record a dangling write's completion by hand, unsticking the run.

        Legal only when the run's log ends at a dangling ``Write`` intent: it
        correlates ``output`` to that intent and dispatches nothing. After it
        records the completion the run drives again, so re-fetch :meth:`log` and
        the cursor sails past the once-dangling intent. Raises a ``SalvorAPIError``
        with code ``wrong_state`` when there is no dangling write.
        """
        self._send(rules.resolve(self.run_id, self.drive_token, output))

    # -- helpers --------------------------------------------------------------

    def _lease(self) -> dict[str, str]:
        return rules.lease(self.drive_token)

    def _send(self, call: Call) -> Any:
        """Perform one described call: send it, decode the answer, parse it."""
        resp = self._http.request(call.method, call.path, **wire.request_kwargs(call))
        return call.parse(wire.decode_json(resp.status_code, resp.content))

    @staticmethod
    def _json(resp: httpx.Response) -> dict[str, Any]:
        return wire.decode_json(resp.status_code, resp.content)
