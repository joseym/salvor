"""One thread's place in one salvor run, decided without any transport.

This module is the rule half of the tape: the cursor that says which log
position a step belongs at, the verdict on whether that step is a replay or a
live call, the order a model turn's parallel tool calls are admitted in, and the
shapes each of those decisions hands back. Nothing here sends a request or waits
for one. :class:`~salvor.langchain.RunTape` performs those decisions over the
synchronous driver and :class:`~salvor.langchain.AsyncRunTape` awaits them over
the asynchronous one, the same way :mod:`salvor._core.driver` serves both
client-driven run drivers. A rule that lived in one of the two tapes would be a
rule the other could drift away from, and drifting here means the same agent
recording its calls at different positions depending on how it was invoked.

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

from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Tuple

from ..models import Event, Usage
from .errors import SalvorMiddlewareError
from .hash import canonical_json

__all__ = [
    "ModelAnswer",
    "ModelOutcome",
    "OpenedCall",
    "Tape",
    "ToolOutcome",
    "TurnPosition",
    "ZERO_USAGE",
    "start_events",
    "usage_payload",
]


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
#: record, and the token counts the run's budgets are held to. A synchronous
#: tape is handed one of these; an asynchronous tape is handed an awaitable of
#: one, which is the only difference between the two.
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
    calls in the model's order rather than in whatever order the event loop or
    the thread pool happens to start them in.
    """

    turn: str
    rank: int
    total: int


def start_events(driver: Any, started: Dict[str, Any]) -> List[Dict[str, Any]]:
    """The events opening a run has to write, which is one or none.

    A fresh run gets its ``RunStarted``, because a client-driven run's first
    event is the client's to write and nothing else can be appended before it. A
    run that already has one is left alone: re-opening returns the recorded log
    and mints a fresh lease, which is all a resuming invoke needs.

    ``driver`` is read for its recorded log and its envelope builder, both of
    which are the same on either driver and neither of which performs any IO.
    """
    if driver.log_envelopes:
        return []
    return [driver.envelope(0, "RunStarted", **started)]


def usage_payload(usage: Usage) -> Dict[str, int]:
    """The token counts as the completion call sends them."""
    return {
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    }


class Tape:
    """The cursor and the turnstile for one thread's run, as pure decisions.

    One of these belongs to one tape, which belongs to one agent invocation. It
    holds the log as it stood when the run was opened, the position the next
    step takes, whether this invoke is still on the recorded path, and how far
    each live model turn has got through its tool calls. Every method either
    answers a question about that state or advances it; none of them waits for
    anything, which is what lets both tapes share them.
    """

    def __init__(
        self, run_id: str, log: List[Event], record_prompts: bool
    ) -> None:
        #: The run this tape is the cursor into.
        self.run_id = run_id
        self._record_prompts = record_prompts
        #: The recorded log at the moment this invoke opened the run, by seq.
        self._recorded = {event.seq: event for event in log}  # type: Dict[int, Event]
        self._recorded_length = len(log)
        #: The next free position; every step takes this one and the one after.
        self._cursor = 1
        #: False once this invoke has asked for something the log does not hold.
        self._replaying = self._recorded_length > 0
        #: Per-turn admission order, keyed by turn: which rank goes next.
        self._turns = {}  # type: Dict[str, int]

    # -- the cursor -----------------------------------------------------------

    def model_slot(self, request_hash: str) -> int:
        """The position for the model call about to happen.

        The recorded step at the cursor has to be a model call asking the same
        question, which is what the request hash is: change the messages, the
        model or its settings and the hash changes with them, because the
        question is no longer the one the recorded answer was an answer to.
        """
        return self._slot(
            lambda event: (
                event.kind == "ModelCallRequested"
                and event.payload.get("request_hash") == request_hash
            ),
            "a model call",
        )

    def tool_slot(self, tool: str, tool_input: Any) -> int:
        """The position for the tool call about to happen.

        Matched on the tool's name and the arguments the model produced, canonicalized
        the same way the recorded input was, so a call that differs only in key
        order is the same call and a call that differs in a value is not.
        """
        wanted = canonical_json(tool_input)
        return self._slot(
            lambda event: (
                event.kind == "ToolCallRequested"
                and event.payload.get("tool") == tool
                and canonical_json(event.payload.get("input")) == wanted
            ),
            "a call to the tool `" + tool + "`",
        )

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

    # -- what an intent carries ------------------------------------------------

    def intent_body(self, body: Any) -> Any:
        """The request body the model intent records, which is none of it unless
        the application asked for it: the body carries user data, and replay
        never reads it, because the correlation key is the request hash alone."""
        return body if self._record_prompts else None

    # -- the shapes a step hands back ------------------------------------------

    def model_replayed(self, seq: int, opened: Any) -> ModelOutcome:
        """The recorded answer salvor handed back when the position was settled."""
        return ModelOutcome(
            seq=seq,
            replayed=True,
            response=opened.response,
            usage=opened.usage or ZERO_USAGE,
        )

    def model_performed(self, seq: int, response: Any, usage: Usage) -> ModelOutcome:
        """The answer this invoke paid a provider for and recorded."""
        return ModelOutcome(seq=seq, replayed=False, response=response, usage=usage)

    def opened_call(self, seq: int, opened: Any) -> OpenedCall:
        """What a tool body is told about the call it is running inside of."""
        return OpenedCall(seq=seq, idempotency_key=opened.idempotency_key)

    def tool_replayed(self, seq: int, opened: Any, output: Any) -> ToolOutcome:
        """The recorded result of a tool call this invoke did not run."""
        return ToolOutcome(
            seq=seq,
            replayed=True,
            output=output,
            effect=opened.effect,
            idempotency_key=opened.idempotency_key,
        )

    def tool_performed(self, seq: int, opened: Any, output: Any) -> ToolOutcome:
        """The result this invoke's tool body produced and recorded."""
        return ToolOutcome(
            seq=seq,
            replayed=False,
            output=output,
            effect=opened.effect,
            idempotency_key=opened.idempotency_key,
        )

    # -- reading a recorded completion back -------------------------------------

    def known_output(self, seq: int) -> Tuple[bool, Any]:
        """The output recorded at ``seq`` in this invoke's own snapshot of the
        log, and whether the snapshot held one at all.

        ``(False, None)`` means the completion landed after this invoke opened
        the run, which is another drive's doing and takes a fresh read to see.
        """
        known = self._recorded.get(seq)
        if known is not None and known.kind == "ToolCallCompleted":
            return True, known.payload.get("output")
        return False, None

    def output_from_tail(self, seq: int, tail: List[Event]) -> Any:
        """The output at ``seq`` in a log tail read from ``seq``.

        Salvor said this call was settled, so a tail with no completion at that
        position is a contradiction rather than a missing value, and is refused
        as one instead of replayed as ``None``.
        """
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

    # -- the turnstile -----------------------------------------------------------

    def admitted(self, position: TurnPosition) -> bool:
        """Whether this call's turn is this call's to take yet.

        LangChain dispatches a model turn's tool calls all at once, with one
        ``asyncio.gather`` under ``ainvoke`` and one thread pool under
        ``invoke``, and in neither case do the hooks reach this middleware in
        the list's order. Under asyncio the wrappers LangChain composes around a
        middleware await before they call it, and the tasks come back off the
        loop in whatever order those awaits finish; measured over five identical
        runs of a three-tool turn, the arrival order was different three times.
        Under threads the pool decides. So arrival order cannot decide log
        positions, or the same turn would record its calls at different seqs on
        different days and a resumed invoke would meet a call it was not
        expecting.

        The model's own order can decide them, and does. Every call carries the
        rank it holds in the AI message that asked for it, and a call is
        admitted only once the rank before it has finished being recorded. The
        tape that owns this state waits on whatever its transport waits with, an
        ``asyncio.Condition`` or a ``threading.Condition``, and the result is
        the same log, seq for seq, however the turn was scheduled.

        A step that raises still advances the turn (its tape's ``finally``), so
        a failed call does not strand the ones after it; they meet whatever it
        left in the log and are told about it there. The one thing that would
        strand them is an earlier call in the turn that never reaches this
        middleware at all, which happens only if a middleware listed before this
        one short-circuits that call: such a call is not recorded either, so the
        configuration is already the thing to fix. A call whose rank cannot be
        read is admitted on arrival instead (see ``_turn_position`` in
        ``middleware.py``), and never waits.
        """
        return self._turns.get(position.turn, 0) == position.rank

    def leave(self, position: TurnPosition) -> None:
        """Hand the turn on to the next rank, and forget a turn that is done."""
        following = position.rank + 1
        if following >= position.total:
            self._turns.pop(position.turn, None)
        else:
            self._turns[position.turn] = following
