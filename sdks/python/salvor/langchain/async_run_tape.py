"""The tape awaited: one thread's run, driven over the asynchronous driver.

Every decision this class makes comes from :class:`salvor.langchain.tape.Tape`,
which is where the cursor, the replay rule and the turn ordering actually live.
What is here is the awaiting: the requests to the control plane, the lock that
keeps one intent open at a time, and the condition a turn's later ranks wait on.
Read :class:`salvor.langchain.RunTape` for the same thing without the awaits.
"""

from __future__ import annotations

import asyncio
import inspect
import logging
from contextlib import asynccontextmanager
from typing import Any, AsyncIterator, Awaitable, Callable, Dict, Optional, TypeVar

from ..async_client_runs import AsyncClientRunDriver
from ..errors import SalvorAPIError
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
    start_events,
    still_ours,
    usage_payload,
)

__all__ = ["AsyncRunTape"]

#: Where a lease this tape could not hand back is mentioned. See
#: :data:`salvor.langchain.run_tape.LOG`; the reasoning is identical.
LOG = logging.getLogger("salvor.langchain")

T = TypeVar("T")


class AsyncRunTape:
    """Drives one thread's run for the length of one ``ainvoke`` or ``astream``."""

    def __init__(self, driver: AsyncClientRunDriver, drive: Drive) -> None:
        self._driver = driver
        self._drive = drive
        #: The run this tape is the cursor into.
        self.run_id = driver.run_id
        self._tape = Tape(driver.run_id, driver.log_envelopes, drive)
        #: The turnstile: one open intent at a time.
        self._gate = asyncio.Lock()
        #: Where a turn's later ranks wait for the rank before them.
        self._turn = asyncio.Condition()
        #: True once this invoke has taken the run's lease back, which it may do
        #: once. See :func:`salvor.langchain.tape.lease_taken`.
        self._reopened = False

    @classmethod
    async def open(
        cls, driver: AsyncClientRunDriver, started: Dict[str, Any], drive: Drive
    ) -> "AsyncRunTape":
        """Open (or re-open) the run behind a thread and take up its cursor."""
        tape = cls(driver, drive)
        events = start_events(driver, started)
        if events:
            # A fresh run's first event is under the same lease rule as every
            # write after it, so it goes through the same guard.
            await tape._guarded(lambda: tape._driver.append(events))
        return tape

    @property
    def thread_id(self) -> str:
        """The LangGraph thread this run is behind, for whoever has to name it."""
        return self._drive.thread_id

    @property
    def run(self) -> AsyncClientRunDriver:
        """The driver underneath, for a caller that wants the log or the lease."""
        return self._driver

    # -- the lease -------------------------------------------------------------

    @asynccontextmanager
    async def step(self) -> AsyncIterator[None]:
        """:meth:`salvor.langchain.RunTape.step`, awaited: one hook's use of
        this tape, handing the lease back if the invoke dies here.

        No lock guards the counting, because a turn's calls are tasks on one
        event loop and neither :meth:`salvor.langchain.tape.Tape.entered` nor
        :meth:`~salvor.langchain.tape.Tape.left` awaits anything in between.
        """
        self._tape.entered()
        failed = False
        try:
            yield
        except BaseException as error:
            failed = still_ours(error)
            raise
        finally:
            if self._tape.left(failed):
                await self.release()

    async def release(self) -> None:
        """:meth:`salvor.langchain.RunTape.release`, awaited: hand the run's
        lease back, once, and never raise doing it."""
        if not self._tape.releasing():
            return
        try:
            await self._driver.release()
        except Exception as refused:  # noqa: BLE001 - the lease lapses either way
            LOG.debug("salvor: run %s kept its lease: %s", self.run_id, refused)

    @asynccontextmanager
    async def _beating(self) -> AsyncIterator[None]:
        """:meth:`salvor.langchain.RunTape._beating`, awaited: keep the lease
        alive while a body this invoke cannot hurry runs.

        A task on this loop rather than a thread, and cancelled in the
        ``finally``: a long tool body here is one that awaits, so the beats
        happen in the gaps it leaves. A body that blocks the loop instead
        blocks this too, which is the honest behaviour, and the same body would
        block everything else the application is running on that loop.
        """
        beater = asyncio.ensure_future(self._beat())
        try:
            yield
        finally:
            beater.cancel()
            try:
                await beater
            except asyncio.CancelledError:
                pass

    async def _beat(self) -> None:
        """Say "still here" until this task is cancelled, learning the interval
        from the answer.

        The same schedule the synchronous tape keeps (see
        :meth:`salvor.langchain.RunTape._beat`): a probe at a fixed quarter
        second, because opening a run says nothing about the lease TTL, then a
        third of whatever the server last answered.
        """
        interval = MINIMUM_BEAT_SECONDS
        while True:
            await asyncio.sleep(interval)
            try:
                interval = beat_interval(await self._driver.heartbeat())
            except Exception:  # noqa: BLE001 - the step underway reports this
                return

    async def model_call(
        self,
        request_hash: str,
        body: Any,
        perform: Callable[[], Awaitable[ModelAnswer]],
    ) -> ModelOutcome:
        """Record a model call, replaying the recorded answer when there is one.

        ``perform`` is awaited only when salvor says the position is not
        settled, so a re-invoke of a finished thread never reaches the provider
        at all. It is awaited inside the turnstile, which is why a slow model
        call holds the position: the intent is open until the answer is
        recorded, and the log accepts nothing else while it is.
        """
        async with self._gate:
            seq = self._tape.model_slot(request_hash)
            self._announce()
            opened = await self._guarded(
                lambda: self._driver.client_model_intent(
                    seq, request_hash, self._tape.intent_body(body)
                )
            )
            if opened.settled:
                return self._tape.model_replayed(seq, opened)
            async with self._beating():
                response, usage = await perform()
            await self._guarded(
                lambda: self._driver.client_model_completion(
                    seq, response, usage_payload(usage)
                )
            )
            return self._tape.model_performed(seq, response, usage)

    async def tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Awaitable[Any]],
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
        (see ``_stop_for_a_person`` in ``middleware.py``).
        """
        if position is None:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform, trust_completion)
        await self._enter_turn(position)
        try:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform, trust_completion)
        finally:
            await self._leave_turn(position)

    async def _tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Awaitable[Any]],
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
        opened = await self._guarded(
            lambda: self._driver.client_tool_intent(seq, tool, tool_input)
        )
        if opened.settled:
            return self._tape.tool_replayed(
                seq, opened, await self._recorded_output(seq + 1)
            )
        if not trust_completion and left_over:
            raise dangling_untrusted_call(
                self._drive.thread_id, self.run_id, tool, seq
            )
        async with self._beating():
            output = await perform(self._tape.opened_call(seq, opened))
        await self._guarded(
            lambda: self._driver.client_tool_completion(seq, output)
        )
        return self._tape.tool_performed(seq, opened, output)

    async def _recorded_output(self, seq: int) -> Any:
        """The output recorded at ``seq``, from this invoke's snapshot of the
        log when it is there and from a fresh read when it is not."""
        known, output = self._tape.known_output(seq)
        if known:
            return output
        tail = await self._guarded(lambda: self._driver.log(seq))
        return self._tape.output_from_tail(seq, tail)

    def _announce(self) -> None:
        """Tell the application about a fork, the first time there is one.

        Called rather than awaited: ``on_fork`` is an ordinary function in both
        SDKs, so an application writes one callback and it runs the same way
        under either transport.
        """
        info = self._tape.unreported_fork()
        if info is not None and self._drive.on_fork is not None:
            self._drive.on_fork(info)

    async def _guarded(self, step: Callable[[], Awaitable[T]]) -> T:
        """One request to the control plane, through one recoverable loss.

        :meth:`salvor.langchain.RunTape._guarded`, awaited. The rule is the
        same one: ``lease_held`` and ``invalid_drive_token`` mean another
        driver holds this run right now and stop the invoke immediately;
        ``unknown_run`` (a restart) is taken back once, the log read again and
        the step retried at the position it reserved; a second ``unknown_run``
        in one invoke is refused by name too.
        """
        while True:
            try:
                return await step()
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
                await self._take_the_run_back()

    async def _take_the_run_back(self) -> None:
        """Re-open the run, once, and re-read what its log holds now."""
        self._reopened = True
        try:
            driver = self._drive.reopen()  # type: ignore[misc]
            if inspect.isawaitable(driver):
                driver = await driver
        except SalvorAPIError as refused:
            raise cannot_reopen(
                self._drive.thread_id, self.run_id, refused
            ) from refused
        self._driver = driver
        self._tape.reread(driver.log_envelopes)

    async def _enter_turn(self, position: TurnPosition) -> None:
        """Wait until every earlier call in this turn has been recorded."""
        async with self._turn:
            await self._turn.wait_for(lambda: self._tape.admitted(position))

    async def _leave_turn(self, position: TurnPosition) -> None:
        """Let go of the turn, and wake whoever it belongs to next."""
        async with self._turn:
            self._tape.leave(position)
            self._turn.notify_all()
