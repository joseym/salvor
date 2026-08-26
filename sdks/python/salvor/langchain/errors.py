"""The errors this middleware raises on its own account, and how to catch one.

Every refusal here surfaces inside somebody else's agent loop, far from this
file, so each one carries three things: a sentence naming the thread or the tool
it is about and what would fix it, a stable :attr:`~SalvorMiddlewareError.code`
to match on instead of parsing that sentence, and the underlying error on
``__cause__`` (and on :attr:`~SalvorMiddlewareError.cause`, the name the
TypeScript middleware uses) when there was one.

Catching one is :func:`salvor_error`, which answers with the middleware error
whether it arrived bare or wrapped in something else. As of LangChain 1.3,
``create_agent`` re-raises what a middleware hook raises exactly as it was
raised: an error out of ``before_agent``, ``wrap_model_call``,
``wrap_tool_call`` or ``after_agent`` reaches the caller of ``invoke`` bare, and
so does an exception out of a tool body. Nothing wraps it, and a parallel tool
turn raises one of its failures rather than a group of them. That is not a
promise LangChain makes, though, and an application's own retry or executor may
wrap it later, so :func:`salvor_error` walks ``__cause__``, ``__context__`` and
the members of an exception group rather than trusting the top of the chain.
"""

from __future__ import annotations

from typing import Any, Optional

from ..errors import SalvorError

__all__ = [
    "SalvorMiddlewareError",
    "ToolNeedsResolution",
    "salvor_error",
]


class SalvorMiddlewareError(SalvorError):
    """Something the middleware itself refuses, as opposed to something the
    control plane refused (which stays a :class:`~salvor.errors.SalvorAPIError`).

    Attributes:
        code: The stable token for what was refused, matched on instead of the
            sentence. One of:

            ``lease_held``
                Another driver holds this thread's run right now. Carries
                :attr:`lapses_in_seconds`, the whole seconds until that hold
                lapses if that driver goes quiet; wait it out and invoke again.
            ``lease_lost``
                This invoke no longer holds the run: its token is not the
                current lease any more, or the lease went twice in one invoke.
                Another instance is driving the same thread.
            ``reopen_refused``
                The run's lease was lost and the server would not hand the run
                back at all. The log is intact; this server is not the one to
                drive it from.
            ``thread_finished``
                The thread's run is closed: ``finish_thread`` recorded its
                ``RunCompleted``, and a completed run takes no more events.
            ``thread_id_missing``
                The invoke passed no ``thread_id`` at all.
            ``thread_id_invalid``
                It passed one that is not a non-empty string.
            ``tool_undeclared``
                The tool has no client-tool declaration on this server, so its
                call cannot be recorded.
            ``tool_needs_resolution``
                The tool ran and its operator settles such a call by hand; see
                :class:`ToolNeedsResolution`.
            ``open_intent``
                The log holds a call recorded as requested and never completed.
                Settle it and invoke again.
            ``run_exists``
                The thread maps to a run id salvor's other mode already
                started. A server-driven run and a client-driven one cannot
                share an id.
            ``thread_never_invoked``
                ``finish_thread`` was asked to close a thread whose run holds
                no events at all, so there is nothing to close.
            ``tool_returned_command``
                A tool answered with a LangGraph ``Command``, which is graph
                control flow rather than a result there is anything to record.
            ``unreadable_record``
                A model answer is not there or does not read back: a call that
                produced no AI message, or a recorded response this middleware
                cannot read as the message LangChain returned.
            ``wrong_client``
                The middleware was given the wrong client for the way the agent
                is being driven (or something that is not a salvor client). The
                one code with no twin in the TypeScript SDK, which has a single
                client and so cannot be given the wrong one.
            ``bad_request``
                The control plane's own refusal, unwrapped rather than
                translated: a client-reported tool output failed the
                operator's declared ``output_schema``. :attr:`cause` is the
                :class:`~salvor.errors.SalvorAPIError` this code came from,
                and its message says which field and why.
            ``client_completion_refused``
                The control plane's own refusal: a reported ``require_equal``
                field differed from the value the intent recorded, or the
                tool's declaration has no ``output_schema`` to check a
                completion against at all. (A tool declared
                ``trust_completion = false`` never reaches the server this
                way: this middleware stops for a person first, as
                ``tool_needs_resolution``.)
            *anything else*
                Any other code a driving call inside a hook meets is not
                translated either, and is not left to escape bare: the
                server's own ``code`` and sentence ride along unchanged, with
                the thread and the run named and :attr:`cause` set to the
                :class:`~salvor.errors.SalvorAPIError` itself. Match it the
                way you would match ``SalvorAPIError.code`` outside a hook.
        cause: The error underneath this one, when there was one. The same
            object as ``__cause__``; the second name is what the TypeScript
            middleware calls it, so a handler written against one SDK reads the
            same against the other.
        lapses_in_seconds: On ``lease_held``, the whole seconds until the
            holding driver's lease lapses if it goes quiet. ``None`` on every
            other code.
    """

    def __init__(
        self,
        message: str,
        *,
        code: str,
        cause: Optional[BaseException] = None,
        lapses_in_seconds: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        #: The stable token for what was refused.
        self.code = code
        #: The sentence, without having to call ``str()``.
        self.message = message
        #: How long the holding driver's lease has left, on ``lease_held``.
        self.lapses_in_seconds = lapses_in_seconds
        if cause is not None:
            self.__cause__ = cause

    @property
    def cause(self) -> Optional[BaseException]:
        """The error underneath this one: ``__cause__`` under the name the
        TypeScript middleware uses, so ``raise ... from`` and this attribute can
        never disagree about what it was."""
        return self.__cause__

    @cause.setter
    def cause(self, error: Optional[BaseException]) -> None:
        self.__cause__ = error


class ToolNeedsResolution(SalvorMiddlewareError):
    """Raised when a tool ran, returned a result, and its operator will not let
    the client close the call.

    A client-tool declaration carries ``trust_completion``, and it is ``false``
    unless the operator opted in: silence gets the safe direction. For such a
    tool the middleware performs the call (the work is the application's to do)
    and then stops, because reporting the result would be the party that
    performed the write also deciding the write succeeded. Salvor refuses such a
    completion by design, so the alternative to stopping here is a raw ``403``
    tearing through the graph after the money moved.

    What is left behind is exactly what the log should say: the call's intent,
    recorded, with no completion. A person confirms what the call actually did,
    at the provider, and records it with a resolve. The next invoke of the
    thread meets the resolved completion and replays it.

    The result the tool returned is on :attr:`output`, so the person resolving
    has the value the call produced without having to reconstruct it. Its
    :attr:`~SalvorMiddlewareError.code` is ``tool_needs_resolution``.
    """

    def __init__(
        self,
        run_id: str,
        thread_id: str,
        seq: int,
        tool: str,
        output: Any,
        key: str,
    ) -> None:
        super().__init__(
            "the tool `{tool}` ran and returned a result, but its client-tool "
            "declaration says `trust_completion = false`: this operator settles "
            "a call to it by hand, and salvor refuses a completion reported by "
            "the client that performed it. Run {run} holds the call's intent at "
            "seq {seq}, and what the tool returned is on this error's `.output`. "
            "A person confirms what the call did and records it, three ways: "
            "`POST /v1/runs/{run}/resolve` on the live server with "
            "`{{\"output\": <json the tool returned>}}`, which also clears the "
            "run's lease so the thread can be invoked again at once; `salvor "
            "resolve {run} --store <path to the server's store> --output '<json "
            "the tool returned>'` on the command line, which writes the store "
            "directly and cannot reach a live server's memory, so the lease it "
            "leaves behind lapses on its own instead (this middleware never "
            "knows the store path; use the one `salvor serve --store` was "
            "given); or `driver.resolve(output)` on a client run driver holding "
            "the run's own lease. The next invoke of thread `{thread}` replays "
            "that resolved output and carries on.".format(
                tool=tool, run=run_id, seq=seq, thread=thread_id
            ),
            code="tool_needs_resolution",
        )
        #: The run holding the call's recorded intent.
        self.run_id = run_id
        #: The LangGraph thread behind that run.
        self.thread_id = thread_id
        #: The log position that intent landed at; the completion a person
        #: records goes at ``seq + 1``.
        self.seq = seq
        #: The tool that ran.
        self.tool = tool
        #: What it returned, for the person who resolves the call.
        self.output = output
        #: The idempotency key salvor derived for this call, which is the key
        #: the tool's own provider was handed and the one to look the call up by
        #: when confirming what it did.
        self.key = key


def salvor_error(error: BaseException) -> Optional[SalvorMiddlewareError]:
    """The middleware error inside ``error``, however it got there.

    Answers the :class:`SalvorMiddlewareError` itself when the invoke raised one
    bare (which is what LangChain does today), the one underneath when
    something wrapped it, and ``None`` when this error has nothing to do with
    salvor. Written this way so an application catches every refusal in one
    place and switches on ``.code``::

        from salvor.langchain import salvor_error

        try:
            agent.invoke(ask, {"configurable": {"thread_id": thread}})
        except Exception as error:
            refusal = salvor_error(error)
            if refusal is None:
                raise
            if refusal.code == "lease_held":
                time.sleep(refusal.lapses_in_seconds)
                ...

    The search is breadth-first from ``error`` itself, following ``__cause__``
    ahead of ``__context__`` at each step (an explicitly chained error is the
    one that was meant) and stepping into the members of an exception group.
    Cycles and self-references are visited once.
    """
    seen = set()
    pending = [error]
    while pending:
        current = pending.pop(0)
        if current is None or id(current) in seen:
            continue
        seen.add(id(current))
        if isinstance(current, SalvorMiddlewareError):
            return current
        pending.append(getattr(current, "__cause__", None))
        pending.append(getattr(current, "__context__", None))
        # `ExceptionGroup` is 3.11 and later, so this reads the attribute the
        # group carries rather than naming a type that may not exist here.
        members = getattr(current, "exceptions", None)
        if isinstance(members, (list, tuple)):
            pending.extend(
                member for member in members if isinstance(member, BaseException)
            )
    return None
