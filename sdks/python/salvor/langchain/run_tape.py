"""The tape blocking: one thread's run, driven over the synchronous driver.

Every decision this class makes comes from :class:`salvor.langchain.tape.Tape`,
which is where the cursor, the replay rule and the turn ordering actually live.
What is here is the calling: the requests to the control plane, the lock that
keeps one intent open at a time, and the condition a turn's later ranks wait on.
Read :class:`salvor.langchain.AsyncRunTape` for the same thing awaited.

The waiting is the one real difference between the two. LangChain's synchronous
``ToolNode`` runs a model turn's tool calls on a thread pool, so the ranks that
have to wait are threads, and they wait on a ``threading.Condition`` rather than
an ``asyncio.Condition``. Nothing here starts an event loop: a synchronous agent
stays on the threads LangChain gave it. The one thread this file starts of its
own accord is the heartbeat (:meth:`RunTape._beating`), a daemon that says
"still here" to salvor while a tool body or a live model call runs, because a
lease with no call inside its TTL lapses under a driver that never went
anywhere.
"""

from __future__ import annotations

import logging
import threading
from contextlib import contextmanager
from typing import Any, Callable, Dict, Iterator, Optional, TypeVar

from ..client_runs import ClientRunDriver
from ..errors import SalvorAPIError
from .errors import SalvorMiddlewareError
from .tape import (
    MINIMUM_BEAT_SECONDS,
    Drive,
    ModelAnswer,
    ModelOutcome,
    OpenedCall,
    Tape,
    ToolOutcome,
    TurnPosition,
    beat_interval,
    cannot_reopen,
    dangling_untrusted_call,
    held_by_another_driver,
    lease_lost,
    lease_taken,
    one_driver_error,
    recorded_failure_message,
    recorded_tool_failure,
    start_events,
    still_ours,
    untrusted_tool_raised,
    usage_payload,
)

__all__ = ["RunTape"]

#: Where a lease this tape could not hand back is mentioned. Debug, not warning:
#: a release that fails changes nothing an application can act on (the lease
#: lapses on its own), and it usually happens while a real error is on its way
#: out, where a second line about the lease would only be noise.
LOG = logging.getLogger("salvor.langchain")

T = TypeVar("T")


class RunTape:
    """Drives one thread's run for the length of one ``invoke`` or ``stream``."""

    def __init__(self, driver: ClientRunDriver, drive: Drive) -> None:
        self._driver = driver
        self._drive = drive
        #: The run this tape is the cursor into.
        self.run_id = driver.run_id
        self._tape = Tape(driver.run_id, driver.log_envelopes, drive)
        #: The turnstile: one open intent at a time.
        self._gate = threading.Lock()
        #: Where a turn's later ranks wait for the rank before them.
        self._turn = threading.Condition()
        #: True once this invoke has taken the run's lease back, which it may do
        #: once. See :func:`salvor.langchain.tape.lease_taken`.
        self._reopened = False
        #: Guards the live-step count, which a parallel turn's pool threads
        #: change at the same moment.
        self._counting = threading.Lock()

    @classmethod
    def open(
        cls, driver: ClientRunDriver, started: Dict[str, Any], drive: Drive
    ) -> "RunTape":
        """Open (or re-open) the run behind a thread and take up its cursor."""
        tape = cls(driver, drive)
        events = start_events(driver, started)
        if events:
            # A fresh run's first event is under the same lease rule as every
            # write after it, so it goes through the same guard.
            tape._guarded(lambda: tape._driver.append(events))
        return tape

    @property
    def thread_id(self) -> str:
        """The LangGraph thread this run is behind, for whoever has to name it."""
        return self._drive.thread_id

    @property
    def run(self) -> ClientRunDriver:
        """The driver underneath, for a caller that wants the log or the lease."""
        return self._driver

    # -- the lease -------------------------------------------------------------

    @contextmanager
    def step(self) -> Iterator[None]:
        """One hook's use of this tape, handing the lease back if it dies here.

        An invoke that ends normally releases from ``after_agent``. An invoke
        that ends by raising never reaches ``after_agent``, so the lease has to
        go back from the step that raised, or the thread stays locked for the
        rest of the lease TTL over an error the application already knows
        about. The one-driver refusals are left alone: the lease they name is
        somebody else's (see :func:`~salvor.langchain.tape.still_ours`).

        A parallel turn's other calls may still be live when one of them
        raises, so the release goes with the last step out rather than the first
        (see :meth:`salvor.langchain.tape.Tape.left`).
        """
        with self._counting:
            self._tape.entered()
        failed = False
        try:
            yield
        except BaseException as error:
            failed = still_ours(error)
            raise
        finally:
            with self._counting:
                hand_back = self._tape.left(failed)
            if hand_back:
                self.release()

    def release(self) -> None:
        """Hand the run's lease back, once, so the next invoke takes the thread
        up at once instead of waiting out the TTL.

        Never raises: a release that fails leaves the lease to lapse the way it
        would have anyway, and this is called on the way out of an invoke that
        may already be carrying an error worth more than this one.
        """
        with self._counting:
            going = self._tape.releasing()
        if not going:
            return
        try:
            self._driver.release()
        except Exception as refused:  # noqa: BLE001 - the lease lapses either way
            LOG.debug("salvor: run %s kept its lease: %s", self.run_id, refused)

    @contextmanager
    def _beating(self) -> Iterator[None]:
        """Keep the lease alive while a body this invoke cannot hurry runs.

        A tool that takes minutes, or a model call the application is streaming
        itself, makes no call to salvor while it works, and a lease with no
        driving call inside its TTL lapses. So a daemon thread says "still
        here" (``POST /v1/client-runs/{id}/heartbeat``) on the interval the
        server's own answer names, and is stopped in the ``finally`` below.

        The thread is a daemon and is not joined: a beat already in flight when
        the body finishes lands on a run this invoke either still holds (a
        no-op) or has just released (a refusal the beater swallows), and
        neither is worth making the tool call wait for.
        """
        stop = threading.Event()
        beater = threading.Thread(
            target=self._beat, args=(stop,), name="salvor-heartbeat", daemon=True
        )
        beater.start()
        try:
            yield
        finally:
            stop.set()

    def _beat(self, stop: threading.Event) -> None:
        """Say "still here" until the body is done, learning the interval from
        the answer.

        The first beat is a probe: opening a run says nothing about the lease
        TTL, and the heartbeat's own answer is where that number comes from, so
        the first one waits a fixed quarter second and every one after it waits
        a third of what the server last said. A body shorter than that probe
        beats not at all, which is nearly every tool call. A refusal means the
        lease is gone or the server is, and the step underway will say so, so
        this just stops.
        """
        interval = MINIMUM_BEAT_SECONDS
        while not stop.wait(interval):
            try:
                interval = beat_interval(self._driver.heartbeat())
            except Exception:  # noqa: BLE001 - the step underway reports this
                return

    def model_call(
        self,
        request_hash: str,
        body: Any,
        perform: Callable[[], ModelAnswer],
    ) -> ModelOutcome:
        """Record a model call, replaying the recorded answer when there is one.

        ``perform`` is called only when salvor says the position is not settled,
        so a re-invoke of a finished thread never reaches the provider at all.
        It is called inside the turnstile, which is why a slow model call holds
        the position: the intent is open until the answer is recorded, and the
        log accepts nothing else while it is.
        """
        with self._gate:
            seq = self._tape.model_slot(request_hash)
            self._announce()
            opened = self._guarded(
                lambda: self._driver.client_model_intent(
                    seq, request_hash, self._tape.intent_body(body)
                )
            )
            if opened.settled:
                return self._tape.model_replayed(seq, opened)
            with self._beating():
                response, usage = perform()
            self._guarded(
                lambda: self._driver.client_model_completion(
                    seq, response, usage_payload(usage)
                )
            )
            return self._tape.model_performed(seq, response, usage)

    def tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Any],
        position: Optional[TurnPosition] = None,
        trust_completion: bool = True,
    ) -> ToolOutcome:
        """Record a tool call, replaying the recorded output when there is one.

        The effect class and the idempotency key both come back from the
        server, derived from the operator's declaration and from
        ``(run, seq, tool)``. The middleware never chooses either, which is the
        whole point of the client-tool surface: the party that performs a write
        does not get to pick the key that would let a duplicate through.

        ``perform`` is handed that same key (with the seq it landed at) before
        it runs, not after, because the tool body it eventually calls is what
        needs it: see :func:`~salvor.langchain.current_tool_call`.

        ``position`` says where this call sits in the turn that asked for it,
        and is what makes a parallel turn replayable rather than merely
        serialized. See :meth:`salvor.langchain.tape.Tape.admitted`.

        ``trust_completion`` is the operator's own word for this tool. When it
        is ``False`` and this position's intent is a dangling one an earlier
        invoke left open, ``perform`` never runs: see
        :func:`~salvor.langchain.tape.dangling_untrusted_call`. A tool whose
        intent this invoke is opening for the first time still runs; whether
        its result is then reported is the caller's call inside ``perform``
        (see ``_stop_for_a_person`` in ``middleware.py``). If ``perform``
        raises an ``Exception`` that is not this middleware's own
        :class:`~salvor.langchain.errors.SalvorMiddlewareError`, the raise
        itself is reported as the call's failure (for a trusted tool) or left
        unposted and refused by name (for an untrusted one): see
        :func:`~salvor.langchain.tape.recorded_tool_failure` and
        :func:`~salvor.langchain.tape.untrusted_tool_raised`. A
        ``BaseException`` that is not an ``Exception`` -- ``KeyboardInterrupt``,
        ``SystemExit``, ``GeneratorExit``, ``asyncio.CancelledError`` -- is the
        process leaving, not the call failing, so it is never caught here: it
        propagates untouched and the intent stays exactly as recorded, open,
        the same dangling-write case a real crash leaves.
        """
        if position is None:
            with self._gate:
                return self._tool_call(tool, tool_input, perform, trust_completion)
        self._enter_turn(position)
        try:
            with self._gate:
                return self._tool_call(tool, tool_input, perform, trust_completion)
        finally:
            self._leave_turn(position)

    def _tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Any],
        trust_completion: bool,
    ) -> ToolOutcome:
        """One tool step, with the turnstile already held."""
        seq = self._tape.tool_slot(tool, tool_input)
        self._announce()
        # Read from this invoke's own reading of the log, before the intent
        # call below can add to it: true only when this exact position already
        # held this tool's intent, which is what tells a dangling untrusted
        # call apart from one this invoke is opening for the first time.
        left_over = self._tape.left_over(seq)
        opened = self._guarded(
            lambda: self._driver.client_tool_intent(seq, tool, tool_input)
        )
        if opened.settled:
            failure = recorded_failure_message(opened.output)
            if failure is not None:
                raise recorded_tool_failure(
                    self._drive.thread_id, self.run_id, tool, seq, failure
                )
            return self._tape.tool_replayed(seq, opened, opened.output)
        if not trust_completion and left_over:
            raise dangling_untrusted_call(
                self._drive.thread_id, self.run_id, tool, seq
            )
        with self._beating():
            try:
                output = perform(self._tape.opened_call(seq, opened))
            except Exception as error:
                if isinstance(error, SalvorMiddlewareError):
                    raise
                if not trust_completion:
                    raise untrusted_tool_raised(
                        self._drive.thread_id, self.run_id, tool, seq, str(error)
                    ) from error
                self._guarded(
                    lambda: self._driver.client_tool_failure(seq, str(error))
                )
                raise
        self._guarded(lambda: self._driver.client_tool_completion(seq, output))
        return self._tape.tool_performed(seq, opened, output)

    def _announce(self) -> None:
        """Tell the application about a fork, the first time there is one."""
        info = self._tape.unreported_fork()
        if info is not None and self._drive.on_fork is not None:
            self._drive.on_fork(info)

    def _guarded(self, step: Callable[[], T]) -> T:
        """One request to the control plane, through one recoverable loss.

        ``lease_held`` (on an open) and ``invalid_drive_token`` (on a write)
        both mean another driver holds this run right now, which no retry from
        here fixes: they stop the invoke immediately, by name
        (:func:`~salvor.langchain.tape.one_driver_error`), before running a
        tool body for a step that was never going to be recorded.

        ``unknown_run`` is the one refusal worth retrying: a restarted salvor
        forgot the run's lease but not its log, so taking the run up again
        returns the recorded state and a fresh lease, the log is read again in
        case another driver wrote something in between, and the step is
        retried at the position it already reserved, where it either meets its
        own work already recorded or is still expected. Losing it twice in one
        invoke is two restarts (or worse) in one invoke, which no retry
        settles either, so it is refused by name too.
        """
        while True:
            try:
                return step()
            except SalvorAPIError as error:
                if held_by_another_driver(error):
                    raise one_driver_error(
                        self._drive.thread_id, self.run_id, error
                    ) from error
                if not lease_lost(error):
                    raise
                if self._reopened or self._drive.reopen is None:
                    raise lease_taken(
                        self._drive.thread_id, self.run_id
                    ) from error
                self._take_the_run_back()

    def _take_the_run_back(self) -> None:
        """Re-open the run, once, and re-read what its log holds now."""
        self._reopened = True
        try:
            driver = self._drive.reopen()  # type: ignore[misc]
        except SalvorAPIError as refused:
            raise cannot_reopen(
                self._drive.thread_id, self.run_id, refused
            ) from refused
        self._driver = driver
        self._tape.reread(driver.log_envelopes)

    def _enter_turn(self, position: TurnPosition) -> None:
        """Wait until every earlier call in this turn has been recorded.

        A rank waiting here occupies the pool thread LangChain dispatched it on,
        and a small pool is still safe: ``ToolNode`` submits a turn's calls in
        the order the model listed them, and a pool hands work out in the order
        it was submitted, so the rank that releases the others is always started
        before the ranks waiting on it. What would strand them is a call that
        never arrives at all, which is a middleware ahead of this one
        short-circuiting it, and such a call is not recorded either.
        """
        with self._turn:
            self._turn.wait_for(lambda: self._tape.admitted(position))

    def _leave_turn(self, position: TurnPosition) -> None:
        """Let go of the turn, and wake whoever it belongs to next."""
        with self._turn:
            self._tape.leave(position)
            self._turn.notify_all()
