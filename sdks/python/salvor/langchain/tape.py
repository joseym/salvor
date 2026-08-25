"""One thread's place in one salvor run: the cursor, the turnstile, and the
rule for deciding whether a step is a replay or a live call.

A LangGraph invoke re-walks the graph from the top every time. This class is
what makes the second walk cheap: it hands each step the log position the first
walk used, asks salvor what is recorded there, and either returns the recorded
answer or performs the call and records it. The positions come from counting,
not from guessing, which is why the turnstile exists: two tool calls in one
model turn would otherwise both try to open an intent at the same place, and
the log's append-guard would refuse the second.

The cursor
----------

``RunStarted`` is seq 0. Every call after it is a pair, intent then completion,
so the cursor moves by two per step: a model call at 1 and 2, a tool call at 3
and 4, the next model call at 5 and 6. The cursor starts at 1 on every invoke,
not at the end of the log, because a re-invoke re-walks the graph from the top
and has to meet the recorded steps in the order they were recorded.

Leaving the recorded path
-------------------------

A re-invoke that asks for something the log does not hold at the cursor has
left the recorded path: a new turn on the thread, an edited prompt, a different
branch. The cursor then jumps to the end of the log and the run carries on
there, so the fork is appended rather than lost. The one case that cannot be
appended to is a log ending at an intent with no completion, and that case is
refused by name: an unfinished call is exactly what a person has to settle
before the run means anything again.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Dict, Optional, Tuple

from ..async_client_runs import AsyncClientRunDriver
from ..models import Event, Usage
from .errors import SalvorMiddlewareError
from .hash import canonical_json

__all__ = ["ModelOutcome", "RunTape", "ToolOutcome", "TurnPosition"]


@dataclass
class ModelOutcome:
    """What a model step turned out to be."""

    seq: int
    replayed: bool
    response: Any
    usage: Usage


@dataclass
class ToolOutcome:
    """What a tool step turned out to be, including the key the server derived."""

    seq: int
    replayed: bool
    output: Any
    effect: str
    idempotency_key: str


ZERO_USAGE = Usage(input_tokens=0, output_tokens=0)

#: What a live model call reports back to the tape: the stored response to
#: record, and the token counts the run's budgets are held to.
ModelAnswer = Tuple[Any, Usage]


@dataclass
class OpenedCall:
    """The position and derived key a tool body is about to run under."""

    seq: int
    idempotency_key: str


@dataclass(frozen=True)
class TurnPosition:
    """Where one tool call sits in the model turn that asked for it.

    ``turn`` identifies the turn (the AI message that listed the calls),
    ``rank`` is the call's index in that message's ``tool_calls``, and ``total``
    is how many calls the turn asked for. The tape uses it to admit a turn's
    calls in the model's order rather than in whatever order the event loop
    happens to start them in.
    """

    turn: str
    rank: int
    total: int


class RunTape:
    """Drives one thread's run for the length of one agent invocation."""

    def __init__(self, driver: AsyncClientRunDriver, record_prompts: bool) -> None:
        self._driver = driver
        #: The run this tape is the cursor into.
        self.run_id = driver.run_id
        self._record_prompts = record_prompts
        #: The recorded log at the moment this invoke opened the run, by seq.
        self._recorded = {
            event.seq: event for event in driver.log_envelopes
        }  # type: Dict[int, Event]
        self._recorded_length = len(driver.log_envelopes)
        #: The next free position; every step takes this one and the one after.
        self._cursor = 1
        #: False once this invoke has asked for something the log does not hold.
        self._replaying = self._recorded_length > 0
        #: The turnstile: one open intent at a time.
        self._gate = asyncio.Lock()
        #: Per-turn admission order, keyed by turn: which rank goes next, and
        #: the events the ranks after it are waiting on.
        self._turns = {}  # type: Dict[str, Dict[str, Any]]

    @classmethod
    async def open(
        cls,
        driver: AsyncClientRunDriver,
        started: Dict[str, Any],
        record_prompts: bool,
    ) -> "RunTape":
        """Open (or re-open) the run behind a thread and take up its cursor.

        A fresh run gets its ``RunStarted`` here, because a client-driven run's
        first event is the client's to write and nothing else can be appended
        before it. A run that already has one is left alone: re-opening returns
        the recorded log and mints a fresh lease, which is all a resuming
        invoke needs.
        """
        if not driver.log_envelopes:
            await driver.append([driver.envelope(0, "RunStarted", **started)])
        return cls(driver, record_prompts)

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
            seq = self._slot(
                lambda event: (
                    event.kind == "ModelCallRequested"
                    and event.payload.get("request_hash") == request_hash
                ),
                "a model call",
            )
            opened = await self._driver.client_model_intent(
                seq, request_hash, body if self._record_prompts else None
            )
            if opened.settled:
                return ModelOutcome(
                    seq=seq,
                    replayed=True,
                    response=opened.response,
                    usage=opened.usage or ZERO_USAGE,
                )
            response, usage = await perform()
            await self._driver.client_model_completion(
                seq,
                response,
                {
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                },
            )
            return ModelOutcome(seq=seq, replayed=False, response=response, usage=usage)

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
        needs it: see :func:`~salvor.langchain.current_tool_call`, which is
        what makes the key reachable there without changing the tool's own
        signature.

        ``position`` says where this call sits in the turn that asked for it,
        and is what makes a parallel turn replayable rather than merely
        serialized. See :meth:`_await_turn`.
        """
        if position is None:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform)
        await self._await_turn(position)
        try:
            async with self._gate:
                return await self._tool_call(tool, tool_input, perform)
        finally:
            self._leave_turn(position)

    async def _tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Awaitable[Any]],
    ) -> ToolOutcome:
        """One tool step, with the turnstile already held."""
        wanted = canonical_json(tool_input)
        seq = self._slot(
            lambda event: (
                event.kind == "ToolCallRequested"
                and event.payload.get("tool") == tool
                and canonical_json(event.payload.get("input")) == wanted
            ),
            "a call to the tool `" + tool + "`",
        )
        opened = await self._driver.client_tool_intent(seq, tool, tool_input)
        if opened.settled:
            return ToolOutcome(
                seq=seq,
                replayed=True,
                output=await self._recorded_output(seq + 1),
                effect=opened.effect,
                idempotency_key=opened.idempotency_key,
            )
        output = await perform(
            OpenedCall(seq=seq, idempotency_key=opened.idempotency_key)
        )
        await self._driver.client_tool_completion(seq, output)
        return ToolOutcome(
            seq=seq,
            replayed=False,
            output=output,
            effect=opened.effect,
            idempotency_key=opened.idempotency_key,
        )

    # -- the turnstile --------------------------------------------------------

    async def _await_turn(self, position: TurnPosition) -> None:
        """Wait until every earlier call in this turn has been recorded.

        LangGraph dispatches a turn's tool calls with one ``asyncio.gather``
        over the model's ``tool_calls`` list, but the hooks do NOT reach this
        class in that list's order: the wrappers LangChain composes around a
        middleware await before they call it, and the tasks come back off the
        loop in whatever order those awaits finish. Measured over five
        identical runs of a three-tool turn, the arrival order was different
        three times. So arrival order cannot decide log positions, or the same
        turn would record its calls at different seqs on different days and a
        resumed invoke would meet a call it was not expecting.

        The model's own order can decide them, and does. Every call carries the
        rank it holds in the AI message that asked for it, and a call waits
        here until the rank before it has finished being recorded. The result
        is the same log, seq for seq, however the loop schedules the turn.

        A step that raises still advances the turn (the caller's ``finally``),
        so a failed call does not strand the ones after it; they meet whatever
        it left in the log and are told about it there. The one thing that
        would strand them is an earlier call in the turn that never reaches
        this middleware at all, which happens only if a middleware listed
        before this one short-circuits that call: such a call is not recorded
        either, so the configuration is already the thing to fix. A call whose
        rank cannot be read is admitted on arrival instead (see
        ``_turn_position`` in ``middleware.py``), and never waits.
        """
        state = self._turns.get(position.turn)
        if state is None:
            state = {"next": 0, "waiting": {}}
            self._turns[position.turn] = state
        while state["next"] != position.rank:
            event = state["waiting"].get(position.rank)
            if event is None:
                event = asyncio.Event()
                state["waiting"][position.rank] = event
            await event.wait()

    def _leave_turn(self, position: TurnPosition) -> None:
        """Hand the turn on to the next rank, and forget a turn that is done."""
        state = self._turns.get(position.turn)
        if state is None:  # pragma: no cover - only if a turn was forgotten early
            return
        state["next"] = position.rank + 1
        event = state["waiting"].pop(position.rank + 1, None)
        if event is not None:
            event.set()
        if state["next"] >= position.total:
            self._turns.pop(position.turn, None)

    # -- the cursor -----------------------------------------------------------

    def _slot(self, matches: Callable[[Event], bool], what: str) -> int:
        """The position for the step about to happen, and two positions consumed.

        While this invoke is still on the recorded path, the recorded event at
        the cursor has to be the step being asked for. When it is not, the
        graph has gone somewhere the log does not describe, and the rest of
        this invoke is appended at the end of the log instead.
        """
        if self._replaying:
            recorded = self._recorded.get(self._cursor)
            if recorded is None:
                self._replaying = False
            elif not matches(recorded):
                self._replaying = False
                tail = self._recorded.get(self._recorded_length - 1)
                if tail is not None and tail.kind in (
                    "ToolCallRequested",
                    "ModelCallRequested",
                ):
                    raise SalvorMiddlewareError(
                        "run {run} asked for {what} at seq {cursor}, but the log "
                        "holds a {kind} there, and its last event (seq {tail_seq}, "
                        "{tail_kind}) is a call that was never completed. Settle "
                        "that call first (`salvor run resolve {run} <output>`, or "
                        "the resolve endpoint) and invoke again.".format(
                            run=self.run_id,
                            what=what,
                            cursor=self._cursor,
                            kind=recorded.kind,
                            tail_seq=tail.seq,
                            tail_kind=tail.kind,
                        )
                    )
                self._cursor = self._recorded_length
        seq = self._cursor
        self._cursor += 2
        return seq

    async def _recorded_output(self, seq: int) -> Any:
        """The output recorded at ``seq``, from this invoke's snapshot of the
        log when it is there and from a fresh read when it is not (a completion
        another drive wrote after this one opened the run)."""
        known = self._recorded.get(seq)
        if known is not None and known.kind == "ToolCallCompleted":
            return known.payload.get("output")
        tail = await self._driver.log(seq)
        completion = tail[0] if tail else None
        if (
            completion is None
            or completion.seq != seq
            or completion.kind != "ToolCallCompleted"
        ):
            raise SalvorMiddlewareError(
                "run {run} reports the tool call at seq {intent} settled, but seq "
                "{seq} holds no completion to replay.".format(
                    run=self.run_id, intent=seq - 1, seq=seq
                )
            )
        return completion.payload.get("output")
