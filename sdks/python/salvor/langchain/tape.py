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

A lease is held until it lapses, not until a newer caller asks for it (see
``API.md``'s drive-token section): re-opening a run whose driver is still
presenting its token within the lease TTL is refused outright, ``409
lease_held``, and a write under a token that is no longer the current one is
``403 invalid_drive_token``. Neither is recoverable by retrying from here: the
first means another driver already holds the run, and under the new rule the
second can now only mean the same thing (a driver that stayed live never loses
`invalid_drive_token` to a race the way it could under "newest caller wins").
Both stop the invoke immediately, by name, before running a tool body for a
step that was never going to be recorded (:func:`held_by_another_driver`,
:func:`one_driver_error`).

A hold ends four ways, and only one of them is the TTL. The middleware releases
it when an invoke ends (``after_agent``, and the step that raised when the
invoke ends by raising: see :meth:`salvor.langchain.RunTape.step`); a heartbeat
keeps it alive while a tool body or a live model call runs, so a driver that
never went anywhere does not lose the run it is inside of
(:meth:`salvor.langchain.RunTape._beating`, :func:`beat_interval`); an operator
resolving a dangling write over HTTP clears it; and a driver that dies without
releasing lets it lapse, which is the safety net rather than the way a drive is
meant to end.

The one refusal worth retrying is ``unknown_run``, which is what a restarted
salvor answers with, because it holds its client-driven leases in memory but
not the log itself. Losing the lease this way once is recoverable and nothing
recorded is lost: the tapes re-open the run (which reads the log fresh,
recognises it as client-driven from its own recorded ``RunStarted``, and mints
this drive a lease of its own), re-read the log and retry the step at the
position it already reserved. Losing it twice in one invoke is two restarts (or
worse) in one invoke, which no retry fixes either, so it is refused by name
(:func:`lease_taken`), and a server that will not hand the run back at all is
refused by :func:`cannot_reopen`.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Optional, Tuple

from ..errors import SalvorAPIError
from ..models import Event, Usage
from .errors import SalvorMiddlewareError, salvor_error
from .hash import canonical_json

__all__ = [
    "BEATS_PER_LEASE",
    "MINIMUM_BEAT_SECONDS",
    "Drive",
    "ForkInfo",
    "ModelAnswer",
    "ModelOutcome",
    "OpenedCall",
    "Tape",
    "ToolOutcome",
    "TurnPosition",
    "ZERO_USAGE",
    "beat_interval",
    "cannot_reopen",
    "dangling_untrusted_call",
    "held_by_another_driver",
    "lease_lost",
    "lease_taken",
    "one_driver_error",
    "recorded_failure_message",
    "recorded_tool_failure",
    "server_driven_run",
    "start_events",
    "still_ours",
    "untrusted_tool_raised",
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


#: The one refusal worth retrying from here: this server no longer remembers
#: the run's lease at all, which is what a restart answers with, because leases
#: live only in process memory and the log they governed does not. A re-open
#: reads that log fresh, recognises the run as client-driven from its own
#: recorded ``RunStarted``, and mints this drive a lease of its own.
RESTARTED = "unknown_run"

#: The refusals that mean another driver holds this run's lease right now: a
#: live lease refused this drive's open outright (``lease_held``), or refused
#: a write because the token it is presenting is no longer the current one
#: (``invalid_drive_token``, which under the held-until-it-lapses rule can now
#: only mean a second driver already took the run over, never a race this
#: drive could have won by asking again). Neither is fixed by re-opening: that
#: would either meet the same refusal again or, worse, hand a tool body's
#: result to a completion nobody is going to record. Both stop the invoke
#: immediately instead.
ONE_DRIVER = ("lease_held", "invalid_drive_token")


def lease_lost(error: Exception) -> bool:
    """Whether ``error`` is salvor saying this drive no longer holds the run,
    for the one reason a re-open from here actually fixes: a restart."""
    return isinstance(error, SalvorAPIError) and error.code == RESTARTED


def held_by_another_driver(error: Exception) -> bool:
    """Whether ``error`` is salvor saying another driver holds this run's
    lease right now, which no retry from here fixes."""
    return isinstance(error, SalvorAPIError) and error.code in ONE_DRIVER


#: The codes that mean this invoke is not the run's driver any more, so there is
#: no lease of its own to hand back: another driver was already holding it
#: (``lease_held``) or has taken it since (``lease_lost``).
NOT_OURS = ("lease_held", "lease_lost")


def still_ours(error: BaseException) -> bool:
    """Whether the lease is still this invoke's to hand back after ``error``.

    An invoke that ends any other way still holds the run and releases it, which
    is what lets the next process take the thread up at once instead of waiting
    out the TTL. The one-driver refusals are the exception: the lease they name
    belongs to somebody else, and a release presenting a token that is not the
    current lease is refused anyway. So they are left alone, rather than
    answered with a call whose only possible outcome is a 403 nobody reads.
    """
    refusal = salvor_error(error)
    return refusal is None or refusal.code not in NOT_OURS


#: How many heartbeats a driver fits inside one lease. Three, so two beats can
#: be lost to a slow network or a busy loop and the lease still stands.
BEATS_PER_LEASE = 3

#: How long a body waits before its first beat, and the shortest any interval
#: after it is allowed to get. Opening a run answers with no TTL at all, so the
#: first beat is the probe that asks for one, and every beat after it is spaced
#: by what the answer said. A tool that returns inside this quarter second
#: never beats at all, which is nearly all of them, and a lease TTL of one
#: second (which a test sets, and nothing else should) still gets its probe
#: comfortably inside the hold. The TypeScript middleware uses the same
#: quarter second for the same reason.
MINIMUM_BEAT_SECONDS = 0.25


def beat_interval(lapses_in_seconds: Any) -> float:
    """How long to wait before the next heartbeat, from what the last one said.

    The server answers every heartbeat with the whole seconds the lease has left
    (see ``API.md``'s heartbeat endpoint), so a driver reads its interval off
    the answer rather than being told the server's configuration some other way.
    A third of it, so two lost beats still leave the lease standing.
    """
    try:
        lapses = float(lapses_in_seconds)
    except (TypeError, ValueError):  # pragma: no cover - a server that says something else
        lapses = 0.0
    return max(lapses / BEATS_PER_LEASE, MINIMUM_BEAT_SECONDS)


def one_driver_error(
    thread_id: str, run_id: str, error: SalvorAPIError
) -> SalvorMiddlewareError:
    """The immediate refusal for ``lease_held`` or ``invalid_drive_token``:
    another driver holds this run right now, so this invoke stops without
    re-opening and without running a tool body for a step that was never
    going to be recorded.

    ``lease_held`` carries ``details.lapses_in_seconds`` (see
    :class:`~salvor.errors.LeaseHeldError`), the whole seconds until the hold
    lapses on its own if that driver goes quiet; it rides along here when the
    error is that kind. ``invalid_drive_token`` carries no such figure, so the
    sentence names the rule instead of a number nobody sent.

    Both sentences are the TypeScript middleware's own, word for word apart
    from the thread and the run (and, for ``lease_held``, the seconds): one
    refusal, one wording, whichever SDK an operator reads it in. TypeScript
    tells the two apart the same way, one sentence for the open-time
    ``LeaseHeldError`` and another (``oneDriverError``) for a mid-invoke
    ``invalid_drive_token``.
    """
    lapses = error.details.get("lapses_in_seconds")
    if lapses is not None:
        message = (
            "thread `{thread}` (run {run}) cannot be opened: another driver "
            "holds its lease right now, and it lapses in {s}s if that driver "
            "goes quiet (or as soon as the run finishes). One driver per "
            "thread at a time. Wait for the lease to lapse and invoke again, "
            "or confirm no other process is already driving this "
            "thread.".format(thread=thread_id, run=run_id, s=lapses)
        )
    else:
        message = (
            "thread `{thread}` (run {run}) is no longer this invoke's to "
            "drive: another driver holds its lease now. One driver per thread "
            "at a time. Invoke a given thread id from one process at a time, "
            "and give work that must run alongside it a thread id of its "
            "own.".format(thread=thread_id, run=run_id)
        )
    return SalvorMiddlewareError(
        message,
        code="lease_held" if error.code == "lease_held" else "lease_lost",
        cause=error,
        lapses_in_seconds=int(lapses) if lapses is not None else None,
    )


def server_driven_run(
    thread_id: str, run_id: str, error: SalvorAPIError
) -> SalvorMiddlewareError:
    """The refusal for a thread whose run id salvor's OTHER mode already
    started.

    ``open_client_run`` knows a client-driven run two ways: this server's lease
    registry, or ``driven_by: "client"`` on the recorded ``RunStarted``. A run
    started with ``start_run`` carries neither, so the open is refused
    ``run_exists``, before any model or tool call. Nothing the middleware can
    do fixes it, but the sentence can say which thread caused it, which the
    server's own (which knows only run ids) cannot.
    """
    return SalvorMiddlewareError(
        "thread `{thread}` maps to run {run}, which salvor's other mode "
        "already started: {reason}. A server-driven run and a client-driven "
        "one cannot share an id, so give this thread an id of its own, or map "
        "it to a different run id with `thread_id_to_run_id`.".format(
            thread=thread_id, run=run_id, reason=error.message
        ),
        code="run_exists",
        cause=error,
    )


def lease_taken(thread_id: str, run_id: str) -> SalvorMiddlewareError:
    """The refusal for ``unknown_run`` met twice inside one invoke: this
    server forgot the run's lease again after already being re-opened once,
    which one straight restart does not do."""
    return SalvorMiddlewareError(
        "run {run} (thread `{thread}`) is being driven from somewhere else: "
        "this invoke lost the run's lease twice, once after taking it back. "
        "Salvor allows one driver per thread at a time, so two app instances "
        "invoking the same thread will go on taking the run from each other "
        "and neither will finish. Invoke a thread from one place, or give the "
        "other task a thread id of its own.".format(thread=thread_id, run=run_id),
        code="lease_lost",
    )


def dangling_untrusted_call(
    thread_id: str, run_id: str, tool: str, seq: int
) -> SalvorMiddlewareError:
    """The refusal for a later invoke that meets an untrusted tool's own
    dangling intent, still unresolved.

    ``trust_completion = false`` exists so the party that performed a write
    does not get to decide it succeeded; running the tool body again on a
    later invoke, before a person has settled the first call, would be exactly
    that. So it never runs a second time: this is the same "never completed"
    refusal a mismatched replay raises in :meth:`Tape._slot`, because that is
    what this is too, a call recorded as requested with nothing this tape may
    treat as its completion.
    """
    return SalvorMiddlewareError(
        "run {run} (thread `{thread}`) met the intent for `{tool}` at seq "
        "{seq} that an earlier invoke left open: its declaration sets "
        "`trust_completion = false`, so that call's result was never reported "
        "and it is a call that was never completed. Settle it first (`POST "
        "/v1/runs/{run}/resolve` on the live server, which clears the run's "
        "lease too; `salvor resolve {run} --store <path to the server's store> "
        "--output '<json "
        "the tool returned>'`, which leaves the lease to lapse on its own; or "
        "`driver.resolve(...)` on a driver holding the run's lease) and invoke "
        "again.".format(run=run_id, thread=thread_id, tool=tool, seq=seq),
        code="open_intent",
    )


#: The reserved key a recorded completion's output carries when the call it
#: closes failed rather than returned: the same sentinel a native tool's
#: exhausted retries write and a reported client failure writes too (see
#: ``salvor_runtime::wire::ERROR_SENTINEL_KEY`` and
#: :meth:`~salvor.client_runs.ClientRunDriver.client_tool_failure`).
ERROR_SENTINEL_KEY = "__salvor_error"


def recorded_failure_message(output: Any) -> Optional[str]:
    """The message a recorded completion's output carries, when that output is
    the failure sentinel; ``None`` for an ordinary result.

    Mirrors the decode rule the sentinel itself is defined by: an object with
    exactly one key, ``__salvor_error``, and nothing else is the sentinel, so
    an ordinary tool result that happens to nest a key by that name somewhere
    inside it is unaffected. This is how a tape meeting a settled intent tells
    "the recorded call failed" from "the recorded call returned this" without
    a second round trip: :attr:`ClientToolIntentResult.output` already carries
    whichever one it is.
    """
    if not isinstance(output, dict) or len(output) != 1:
        return None
    body = output.get(ERROR_SENTINEL_KEY)
    if not isinstance(body, dict):
        return None
    message = body.get("message")
    return message if isinstance(message, str) else None


def recorded_tool_failure(
    thread_id: str, run_id: str, tool: str, seq: int, message: str
) -> SalvorMiddlewareError:
    """The refusal when a later invoke meets a call this run already recorded
    as failed.

    A recorded failure is always a ``write``'s: a trusted ``read`` or
    ``idempotent`` body that raises posts nothing, so the call it raised in is
    performed again on the next invoke rather than settled (see
    :meth:`salvor.langchain.RunTape.tool_call`). A write is the one effect
    class that may have half happened, and so the one worth recording as
    failed for a person to read.

    A recorded failure settles the position exactly as a recorded success
    does: nothing runs again, and the log is not asking to be retried. Handing
    the failure sentinel to the model as though it were the tool's real output
    would be the wrong kind of quiet, so this middleware raises instead,
    naming the message the failure was recorded with. A permanently failing
    input fails the same way on every invoke, because the call is settled, not
    retried: fix whatever the tool keeps failing on and give the thread a new
    turn, or start a new thread.
    """
    return SalvorMiddlewareError(
        "run {run} (thread `{thread}`) already recorded the call to `{tool}` "
        "at seq {seq} as a failure: {message}. That recording settles the "
        "call the same way a recorded success would, so this middleware "
        "raises rather than handing the model a failure it was never meant "
        "to read, and it will raise the same way on every further invoke: a "
        "failed call is not retried. Fix the input this call keeps failing "
        "on and give the thread a new turn, or start a new thread.".format(
            run=run_id, thread=thread_id, tool=tool, seq=seq, message=message
        ),
        code="tool_failed",
    )


def untrusted_tool_raised(
    thread_id: str, run_id: str, tool: str, seq: int, message: str
) -> SalvorMiddlewareError:
    """The refusal when a tool this operator settles by hand raises on this
    very invoke, rather than returning.

    `trust_completion = false` means the client never gets to decide whether
    its own write landed, and a caught exception reported as a failure would
    be exactly that decision made the other way: "it did NOT land," said by
    the party that benefits from being believed. So nothing is posted. The
    call ran once, for real, and what it did is unknown to salvor either way;
    the intent stays open, exactly as recorded, for a person to settle after
    confirming with the provider what actually happened. That is not always
    what the provider shows, so the sentence names the other honest ending
    too: a call that never reached the provider has no output anybody could
    record, and the run is abandoned rather than resolved.

    ``message`` is what the tool itself threw, named early in the sentence so
    a person reading the refusal does not have to go find it on ``__cause__``.
    """
    return SalvorMiddlewareError(
        "the tool `{tool}` threw `{message}` while running under an intent "
        "this middleware may not self-complete: its declaration sets "
        "`trust_completion = false`, so neither a result nor a failure may be "
        "reported on the tool's own say-so. Run {run} (thread `{thread}`) is "
        "stopped at seq {seq} until a person confirms what actually happened "
        "and records it by hand (`POST /v1/runs/{run}/resolve` on the live "
        "server, which clears the run's lease too; `salvor resolve {run} "
        "--store <path to the server's store> --output '<json the call "
        "produced, or an empty object if it produced nothing>'`, which leaves "
        "the lease to lapse on its own; or `driver.resolve(...)` on a driver "
        "holding the run's lease) and invoke again. If the provider shows the "
        "call never happened, or did something this thread cannot carry on "
        "from, there is nothing to record: abandon the run instead (`POST "
        "/v1/runs/{run}/abandon` on the server, or `salvor abandon {run} "
        "--store <path to the server's store>`) and give the next task a new "
        "thread id.".format(
            tool=tool, message=message, run=run_id, thread=thread_id, seq=seq
        ),
        code="open_intent",
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
        ),
        code="reopen_refused",
        cause=refused,
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
        #: How many of this invoke's steps are inside the tape right now, and
        #: whether one of them has already failed. See :meth:`entered`.
        self._live = 0
        self._failing = False
        #: True once the lease has been handed back, so it is handed back once.
        self.released = False
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
                        "that call first (`POST /v1/runs/{run}/resolve` on the "
                        "live server, or `salvor resolve {run} --store <path to "
                        "the server's store> --output '<json>'`) and invoke "
                        "again.".format(
                            run=self.run_id,
                            what=what,
                            cursor=self._cursor,
                            kind=recorded.kind,
                            tail_seq=tail.seq,
                            tail_kind=tail.kind,
                        ),
                        code="open_intent",
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

    # -- when the lease goes back -----------------------------------------------

    def entered(self) -> None:
        """One of this invoke's steps has come inside the tape."""
        self._live += 1

    def left(self, failed: bool) -> bool:
        """One step leaves; ``True`` when the lease should go back with it.

        Answering ``True`` asks for the release; the handing back itself, and
        the once-only flag that goes with it, is :meth:`releasing`.

        An invoke that ends normally hands the lease back from ``after_agent``.
        An invoke that ends by raising never reaches ``after_agent`` at all
        (LangChain skips it), so the step that raised has to do it, and the
        lease would otherwise be held until it lapsed, locking the thread for
        the rest of the TTL over an error the application has already been told
        about.

        The counting is what keeps that safe for a parallel turn. A model turn's
        other tool calls are still live when one of them raises: LangGraph runs
        them anyway, and handing the lease back under them would turn their own
        result into a one-driver refusal about a lease this invoke gave away.
        So the failure is remembered and the lease goes back with the LAST step
        out, whichever one that is.
        """
        self._live -= 1
        if failed:
            self._failing = True
        return self._live <= 0 and self._failing and not self.released

    def releasing(self) -> bool:
        """``True`` once, when nothing has handed this run's lease back yet.

        What ``after_agent`` asks on the ordinary path, so an invoke whose last
        step already released (see :meth:`left`) does not ask the server to
        release a lease it no longer holds.
        """
        if self.released:
            return False
        self.released = True
        return True

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

    # -- an untrusted tool's own dangling intent --------------------------------

    def left_over(self, seq: int) -> bool:
        """Whether ``seq`` already held a ``ToolCallRequested`` in the log this
        tape has read, before the call about to be opened there.

        True only for a dangling intent an earlier invoke left open (this
        invoke asked for the exact same call, at the exact same position, and
        found it already recorded); a call this invoke is opening for the
        first time is not in that reading yet, so this reads false for it. See
        :func:`dangling_untrusted_call`, the refusal this tells apart from a
        genuinely new call.
        """
        recorded = self._recorded.get(seq)
        return recorded is not None and recorded.kind == "ToolCallRequested"

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
