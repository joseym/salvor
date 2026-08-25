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

A fork is not silent. The tape remembers where it happened, every message the
rest of that invoke returns says so in its marker (:meth:`Tape.marker`), and
the application is told once through its ``on_fork`` (see
:class:`salvor.langchain.SalvorMiddleware`). Running off the end of a recorded
log is deliberately not a fork: a log that simply stops is a thread being
carried on, which is what every invoke that adds a turn does. A fork is the log
holding a DIFFERENT step at the position this invoke asked for, which means the
answers recorded below it answer a question nobody is asking any more.

The lease
---------

A drive token belongs to a process, not to a thread of one: another instance of
the same app opening this thread's run takes the lease, and salvor then refuses
this drive's next write with ``invalid_drive_token``. A salvor that restarted
refuses it with ``unknown_run`` instead, because it holds its client-driven
leases in memory; :data:`LEASE_LOST` is both. Losing the lease once is
recoverable and nothing recorded is lost, so the tapes re-open the run (which
returns the recorded state and a fresh lease), re-read the log and retry the
step at the position it already reserved. Losing it twice in one invoke is two
drivers taking turns, which no retry fixes, so it is refused by name
(:func:`lease_taken`), and a server that will not hand the run back at all is
refused by :func:`cannot_reopen`.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Optional, Tuple

from ..errors import SalvorAPIError
from ..models import Event, Usage
from .errors import SalvorMiddlewareError
from .hash import canonical_json

__all__ = [
    "Drive",
    "ForkInfo",
    "ModelAnswer",
    "ModelOutcome",
    "OpenedCall",
    "Tape",
    "ToolOutcome",
    "TurnPosition",
    "ZERO_USAGE",
    "cannot_reopen",
    "lease_lost",
    "lease_taken",
    "start_events",
    "usage_payload",
]


@dataclass(frozen=True)
class ForkInfo:
    """Where an invoke left the recorded path, as the application is told it and
    as every message from there on carries it.

    The same four fields the TypeScript middleware's ``SalvorForkNotice``
    carries, so an application that handles forks in both writes the same
    handler twice.
    """

    #: The log position the tape asked for and the log answered differently at.
    at: int
    #: The LangGraph thread being invoked.
    thread: str
    #: The salvor run behind that thread.
    run: str
    #: The sentence the default handler warns with.
    message: str


@dataclass(frozen=True)
class Drive:
    """What one invoke drives a run with, beyond the connection itself.

    One of these is built per invocation by
    :class:`~salvor.langchain.SalvorMiddleware` and handed to the tape, which is
    why both tapes are given the same four answers however they wait for them.
    """

    #: The LangGraph thread id, which every refusal and every fork names.
    thread_id: str
    #: Whether each model intent records the request body (see
    #: :meth:`Tape.intent_body`).
    record_prompts: bool = False
    #: Takes the run up again after the lease was lost, answering with a fresh
    #: driver (or, under the asynchronous client, an awaitable of one). ``None``
    #: leaves a lost lease to the caller.
    reopen: Optional[Callable[[], Any]] = None
    #: Told once, the first time this invoke leaves the recorded path.
    on_fork: Optional[Callable[[ForkInfo], None]] = None


@dataclass
class ModelOutcome:
    """What a model step turned out to be."""

    seq: int
    replayed: bool
    response: Any
    usage: Usage
    #: What the message this step produces says about itself: see
    #: :meth:`Tape.marker`.
    marker: Dict[str, Any]


@dataclass
class ToolOutcome:
    """What a tool step turned out to be, including the key the server derived."""

    seq: int
    replayed: bool
    output: Any
    effect: str
    idempotency_key: str
    #: What the message this step produces says about itself: see
    #: :meth:`Tape.marker`.
    marker: Dict[str, Any]


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


def _fork_sentence(thread_id: str, run_id: str, at: int) -> str:
    """What a fork is worth saying out loud.

    A fork is not an error: the run carries on, appended past its recorded
    history, and everything from there is performed for real. It is worth saying
    all the same, because the usual cause is something an operator can fix and
    would otherwise never see: a tool whose result is not the same twice, or a
    graph that branches on the clock. The middleware cannot tell which, so it
    names the position and the two things to look at. The TypeScript middleware
    says the same sentence.
    """
    return (
        "salvor: thread `{thread}` (run {run}) left its recorded path at seq "
        "{at}. Nothing from there replays: every model call and every tool call "
        "for the rest of this invoke is being performed and recorded afresh, "
        "and the messages carry `response_metadata[\"salvor\"][\"forked\"]` "
        "saying so. If this thread was meant to resume, look for a tool whose "
        "result differs between invokes, or a graph that branches on the clock "
        "or on randomness.".format(thread=thread_id, run=run_id, at=at)
    )


#: The refusals that mean this drive is no longer the run's driver. The first
#: is another process having re-opened the run and taken the lease; the second
#: is a salvor that no longer knows the run at all, which is what a restarted
#: server answers, because it holds its client-driven leases in memory.
LEASE_LOST = ("invalid_drive_token", "unknown_run")


def lease_lost(error: Exception) -> bool:
    """Whether ``error`` is salvor saying this drive no longer holds the run."""
    return isinstance(error, SalvorAPIError) and error.code in LEASE_LOST


def lease_taken(thread_id: str, run_id: str) -> SalvorMiddlewareError:
    """The refusal for a lease lost twice inside one invoke."""
    return SalvorMiddlewareError(
        "run {run} (thread `{thread}`) is being driven from somewhere else: "
        "this invoke lost the run's lease twice, once after taking it back. "
        "Salvor allows one driver per thread at a time, so two app instances "
        "invoking the same thread will go on taking the run from each other "
        "and neither will finish. Invoke a thread from one place, or give the "
        "other task a thread id of its own.".format(thread=thread_id, run=run_id)
    )


def cannot_reopen(
    thread_id: str, run_id: str, refused: SalvorAPIError
) -> SalvorMiddlewareError:
    """The refusal for a run this server will not hand back at all.

    A salvor server keeps its client-driven leases in memory, so a server that
    restarted does not recognise a run it opened before and refuses to adopt one
    that already has recorded history. Nothing is lost: the log is on disk and
    reads back. What cannot happen is this invoke carrying on, and saying so is
    better than retrying into the same refusal.
    """
    return SalvorMiddlewareError(
        "run {run} (thread `{thread}`) lost its lease and could not be taken "
        "up again: {reason}. The recorded log is intact and still readable; "
        "what is gone is this server's lease on the run, which is what a "
        "restarted salvor loses. Drive the thread against the server that "
        "opened it, or start the next task on a new thread id.".format(
            thread=thread_id, run=run_id, reason=refused.message
        )
    )


class Tape:
    """The cursor and the turnstile for one thread's run, as pure decisions.

    One of these belongs to one tape, which belongs to one agent invocation. It
    holds the log as it stood when the run was opened, the position the next
    step takes, whether this invoke is still on the recorded path, and how far
    each live model turn has got through its tool calls. Every method either
    answers a question about that state or advances it; none of them waits for
    anything, which is what lets both tapes share them.
    """

    def __init__(self, run_id: str, log: List[Event], drive: Drive) -> None:
        #: The run this tape is the cursor into.
        self.run_id = run_id
        #: What this invoke is driving it with: the thread, the prompt-recording
        #: choice, how to take the run up again, and who to tell about a fork.
        self.drive = drive
        #: The next free position; every step takes this one and the one after.
        self._cursor = 1
        #: Where this invoke left the recorded path, once it has.
        self.fork = None  # type: Optional[ForkInfo]
        #: The same, until the application has been told about it.
        self._unreported = None  # type: Optional[ForkInfo]
        #: Per-turn admission order, keyed by turn: which rank goes next.
        self._turns = {}  # type: Dict[str, int]
        self._take_up(log)

    def _take_up(self, log: List[Event]) -> None:
        """Take up a reading of the log, leaving the cursor where it is."""
        #: The recorded log as this tape last read it, by seq.
        self._recorded = {event.seq: event for event in log}  # type: Dict[int, Event]
        self._recorded_length = len(log)
        #: False once this invoke has asked for something the log does not hold,
        #: and false forever after a fork: a re-read cannot put an invoke back
        #: on a path it has already left.
        self._replaying = self._recorded_length > 0 and self.fork is None

    def reread(self, log: List[Event]) -> None:
        """Take up the log as it stands now, after the run was re-opened.

        The cursor does not move: the step that lost the lease is about to be
        retried at the position it already reserved, and what it meets there is
        decided by what the log holds now, which may include work another driver
        recorded in between.
        """
        self._take_up(log)

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
                self._forked_at(self._cursor)
                self._cursor = self._recorded_length
        seq = self._cursor
        self._cursor += 2
        return seq

    def _forked_at(self, seq: int) -> None:
        """Remember, once, that this invoke left the recorded path here.

        Only the log holding a different step at ``seq`` gets here. A log that
        simply stops is not a fork (see the module docs), and neither is a
        refusal: an invoke that is turned away by an unsettled call never left
        the path, it was stopped on it.
        """
        if self.fork is not None:
            return
        self.fork = ForkInfo(
            at=seq,
            thread=self.drive.thread_id,
            run=self.run_id,
            message=_fork_sentence(self.drive.thread_id, self.run_id, seq),
        )
        self._unreported = self.fork

    def unreported_fork(self) -> Optional[ForkInfo]:
        """The fork the application has not been told about, and never twice.

        The telling is a side effect, so it belongs to whichever tape owns the
        transport rather than to these rules; what belongs here is that it
        happens exactly once per invoke.
        """
        info, self._unreported = self._unreported, None
        return info

    # -- what a message says about itself ---------------------------------------

    def marker(self, seq: int, replayed: bool) -> Dict[str, Any]:
        """What the message a step produces carries in
        ``response_metadata["salvor"]``.

        Three shapes, and every message this middleware returns carries one of
        them, so a reader never has to read anything into a message that carries
        none:

        * ``{"replayed": True, "seq": n, "run": ...}``: this answer came out of
          the log, and nothing was paid for or performed;
        * ``{"live": True, "seq": n, "run": ...}``: this answer was paid for or
          performed on this invoke and recorded at that position;
        * ``{"forked": {"at": n, "thread": ..., "run": ...}}``: this invoke has
          left the recorded path, at seq ``n``, and everything it returns from
          there on is being appended to the run rather than replayed from it.

        The fork shape wins over the other two, because it is the one a reader
        acts on: which position an appended answer landed at matters less than
        the fact that the thread is no longer following what was recorded.
        """
        if self.fork is not None:
            return {
                "forked": {
                    "at": self.fork.at,
                    "thread": self.fork.thread,
                    "run": self.fork.run,
                }
            }
        if replayed:
            return {"replayed": True, "seq": seq, "run": self.run_id}
        return {"live": True, "seq": seq, "run": self.run_id}

    # -- what an intent carries ------------------------------------------------

    def intent_body(self, body: Any) -> Any:
        """The request body the model intent records, which is none of it unless
        the application asked for it: the body carries user data, and replay
        never reads it, because the correlation key is the request hash alone."""
        return body if self.drive.record_prompts else None

    # -- the shapes a step hands back ------------------------------------------

    def model_replayed(self, seq: int, opened: Any) -> ModelOutcome:
        """The recorded answer salvor handed back when the position was settled."""
        return ModelOutcome(
            seq=seq,
            replayed=True,
            response=opened.response,
            usage=opened.usage or ZERO_USAGE,
            marker=self.marker(seq, replayed=True),
        )

    def model_performed(self, seq: int, response: Any, usage: Usage) -> ModelOutcome:
        """The answer this invoke paid a provider for and recorded."""
        return ModelOutcome(
            seq=seq,
            replayed=False,
            response=response,
            usage=usage,
            marker=self.marker(seq, replayed=False),
        )

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
            marker=self.marker(seq, replayed=True),
        )

    def tool_performed(self, seq: int, opened: Any, output: Any) -> ToolOutcome:
        """The result this invoke's tool body produced and recorded."""
        return ToolOutcome(
            seq=seq,
            replayed=False,
            output=output,
            effect=opened.effect,
            idempotency_key=opened.idempotency_key,
            marker=self.marker(seq, replayed=False),
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
