"""The errors this middleware raises on its own account."""

from __future__ import annotations

from typing import Any

from ..errors import SalvorError

__all__ = ["SalvorMiddlewareError", "ToolNeedsResolution"]


class SalvorMiddlewareError(SalvorError):
    """Something the middleware itself refuses, as opposed to something the
    control plane refused (which stays a :class:`~salvor.errors.SalvorAPIError`).

    Every message names the thread or the tool it is about and what would fix
    it, because these all surface inside somebody else's agent loop, far from
    this file.
    """


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
    at the provider, and records it with the resolve endpoint. The next invoke
    of the thread meets the resolved completion and replays it.

    The result the tool returned is on :attr:`output`, so the person resolving
    has the value the call produced without having to reconstruct it.
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
            "A person confirms what the call did and records it: `salvor "
            "resolve {run} --output '<json the tool returned>'`, the Inspector's "
            "resolve, or `driver.resolve(output)` on a client run driver. The "
            "next invoke of thread `{thread}` replays that resolved output and "
            "carries on.".format(tool=tool, run=run_id, seq=seq, thread=thread_id)
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
