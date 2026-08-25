"""The tape blocking: one thread's run, driven over the synchronous driver.

Every decision this class makes comes from :class:`salvor.langchain.tape.Tape`,
which is where the cursor, the replay rule and the turn ordering actually live.
What is here is the calling: the requests to the control plane, the lock that
keeps one intent open at a time, and the condition a turn's later ranks wait on.
Read :class:`salvor.langchain.AsyncRunTape` for the same thing awaited.

The waiting is the one real difference between the two. LangChain's synchronous
``ToolNode`` runs a model turn's tool calls on a thread pool, so the ranks that
have to wait are threads, and they wait on a ``threading.Condition`` rather than
an ``asyncio.Condition``. Nothing here starts a thread of its own and nothing
here starts an event loop: a synchronous agent stays on the threads LangChain
gave it.
"""

from __future__ import annotations

import threading
from typing import Any, Callable, Dict, Optional

from ..client_runs import ClientRunDriver
from .tape import (
    ModelAnswer,
    ModelOutcome,
    OpenedCall,
    Tape,
    ToolOutcome,
    TurnPosition,
    start_events,
    usage_payload,
)

__all__ = ["RunTape"]


class RunTape:
    """Drives one thread's run for the length of one ``invoke`` or ``stream``."""

    def __init__(self, driver: ClientRunDriver, record_prompts: bool) -> None:
        self._driver = driver
        #: The run this tape is the cursor into.
        self.run_id = driver.run_id
        self._tape = Tape(driver.run_id, driver.log_envelopes, record_prompts)
        #: The turnstile: one open intent at a time.
        self._gate = threading.Lock()
        #: Where a turn's later ranks wait for the rank before them.
        self._turn = threading.Condition()

    @classmethod
    def open(
        cls,
        driver: ClientRunDriver,
        started: Dict[str, Any],
        record_prompts: bool,
    ) -> "RunTape":
        """Open (or re-open) the run behind a thread and take up its cursor."""
        events = start_events(driver, started)
        if events:
            driver.append(events)
        return cls(driver, record_prompts)

    @property
    def run(self) -> ClientRunDriver:
        """The driver underneath, for a caller that wants the log or the lease."""
        return self._driver

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
            opened = self._driver.client_model_intent(
                seq, request_hash, self._tape.intent_body(body)
            )
            if opened.settled:
                return self._tape.model_replayed(seq, opened)
            response, usage = perform()
            self._driver.client_model_completion(seq, response, usage_payload(usage))
            return self._tape.model_performed(seq, response, usage)

    def tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Any],
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
            with self._gate:
                return self._tool_call(tool, tool_input, perform)
        self._enter_turn(position)
        try:
            with self._gate:
                return self._tool_call(tool, tool_input, perform)
        finally:
            self._leave_turn(position)

    def _tool_call(
        self,
        tool: str,
        tool_input: Any,
        perform: Callable[[OpenedCall], Any],
    ) -> ToolOutcome:
        """One tool step, with the turnstile already held."""
        seq = self._tape.tool_slot(tool, tool_input)
        opened = self._driver.client_tool_intent(seq, tool, tool_input)
        if opened.settled:
            return self._tape.tool_replayed(
                seq, opened, self._recorded_output(seq + 1)
            )
        output = perform(self._tape.opened_call(seq, opened))
        self._driver.client_tool_completion(seq, output)
        return self._tape.tool_performed(seq, opened, output)

    def _recorded_output(self, seq: int) -> Any:
        """The output recorded at ``seq``, from this invoke's snapshot of the
        log when it is there and from a fresh read when it is not."""
        known, output = self._tape.known_output(seq)
        if known:
            return output
        return self._tape.output_from_tail(seq, self._driver.log(seq))

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
