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
from typing import Any, Awaitable, Callable, Dict, Optional, TypeVar

from ..async_client_runs import AsyncClientRunDriver
from ..errors import SalvorAPIError
from .tape import (
    Drive,
    ModelAnswer,
    ModelOutcome,
    OpenedCall,
    Tape,
    ToolOutcome,
    TurnPosition,
    cannot_reopen,
    lease_lost,
    lease_taken,
    start_events,
    usage_payload,
)

__all__ = ["AsyncRunTape"]

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
        """
        if position is None:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform)
        await self._enter_turn(position)
        try:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform)
        finally:
            await self._leave_turn(position)

    async def _tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Awaitable[Any]],
    ) -> ToolOutcome:
        """One tool step, with the turnstile already held."""
        seq = self._tape.tool_slot(tool, tool_input)
        self._announce()
        opened = await self._guarded(
            lambda: self._driver.client_tool_intent(seq, tool, tool_input)
        )
        if opened.settled:
            return self._tape.tool_replayed(
                seq, opened, await self._recorded_output(seq + 1)
            )
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
        """One request to the control plane, through one lost lease.

        :meth:`salvor.langchain.RunTape._guarded`, awaited. The rule is the
        same one: a lease lost once is taken back, the log is read again and the
        step is retried at the position it reserved; a lease lost twice in one
        invoke is two drivers taking turns and is refused by name.
        """
        while True:
            try:
                return await step()
            except SalvorAPIError as error:
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
