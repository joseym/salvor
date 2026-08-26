"""The middleware: one line added to an agent somebody already wrote.

Everything else in this package exists to serve the hooks below. They open the
thread's run, hash the model request, record what the model and the tools did,
and hand the recorded answers back on a re-invoke.

There are two of each hook, because there are two ways to drive a LangChain
agent. A middleware built over salvor's synchronous :class:`~salvor.Client`
serves ``agent.invoke`` and ``agent.stream`` through ``before_agent``,
``wrap_model_call``, ``wrap_tool_call`` and ``after_agent``; one built over
:class:`~salvor.AsyncClient` serves ``agent.ainvoke`` and ``agent.astream``
through the ``a``-prefixed four. The client decides which pair does the work,
and driving the agent the other way is refused by name rather than quietly
recording nothing.
"""

from __future__ import annotations

import asyncio
import logging
import threading
from typing import Any, Awaitable, Callable, Dict, List, Optional, Union

from langchain.agents.middleware import AgentMiddleware
from langchain_core.messages import AIMessage, ToolMessage

from ..async_client import AsyncClient
from ..client import Client
from ..errors import SalvorAPIError
from ..models import Event
from .async_run_tape import AsyncRunTape
from .current_call import ToolCallContext, arun_with_tool_call, run_with_tool_call
from .errors import SalvorMiddlewareError, ToolNeedsResolution
from .hash import hash_value, run_id_for_thread
from .messages import (
    as_tool_content,
    canonical_tool_message,
    mark,
    stored_ai_message,
    stored_form,
    tool_output,
    usage_of,
)
from .replay_model import ReplayChatModel
from .request import canonical_request, request_hash
from .run_tape import RunTape
from .tape import (
    Drive,
    ForkInfo,
    ModelAnswer,
    ModelOutcome,
    OpenedCall,
    ToolOutcome,
    TurnPosition,
    held_by_another_driver,
    one_driver_error,
    server_driven_run,
    still_ours,
)

__all__ = ["SalvorMiddleware", "salvor_middleware", "warn_of_fork"]

#: Where a fork nobody asked to hear about is reported. A logger rather than
#: `warnings.warn`: a fork is a runtime event in somebody else's agent loop, not
#: a deprecation, so it belongs in the log an application already collects and
#: already routes. `warnings.warn` would also be shown once per call site per
#: process by the default filter, which would hide the second thread that forked
#: today, and a library that turns into an exception under `-W error` is a
#: library that decides an application's failure policy for it.
LOG = logging.getLogger("salvor.langchain")

#: What a run's `RunStarted` records as its agent definition. The middleware is
#: the definition here: LangGraph owns the graph, and salvor is told only that
#: this run's calls came through this adapter. The string names the adapter
#: rather than the language, and is the one the TypeScript middleware writes,
#: so a thread's run reads the same whichever SDK opened it.
AGENT_DEF = {"middleware": "@salvor-run/client/langchain"}

ThreadIdToRunId = Callable[[str], Union[str, Awaitable[str]]]
AnyClient = Union[Client, AsyncClient]
OnFork = Callable[[ForkInfo], None]


def warn_of_fork(fork: ForkInfo) -> None:
    """Say, once per invoke, that this thread has left what was recorded.

    The default ``on_fork``, over the sentence the fork itself carries, which is
    the sentence the TypeScript middleware warns with too.
    """
    LOG.warning("%s", fork.message)


class SalvorMiddleware(AgentMiddleware):
    """A LangChain middleware that records this agent's model and tool calls in
    a salvor run, and replays them on a re-invoke of the same thread.

        from langchain.agents import create_agent
        from salvor import Client
        from salvor.langchain import SalvorMiddleware

        agent = create_agent(
            model=model,
            tools=tools,
            middleware=[SalvorMiddleware(Client("http://127.0.0.1:8080"))],
        )

        agent.invoke(
            {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
            {"configurable": {"thread_id": "order-7781"}},
        )

    Pass :class:`~salvor.AsyncClient` instead and the same agent is driven with
    ``await agent.ainvoke(...)`` and ``agent.astream(...)``. Whichever client is
    given, the recording is the same recording: the same positions, the same
    request hashes, the same derived keys, the same log. A run opened by one can
    be resumed by the other.

    The client is what decides, and it decides for the whole invocation. A
    synchronous client under ``ainvoke``, or an asynchronous one under
    ``invoke``, is refused with a sentence naming the client to pass, because a
    middleware that recorded nothing would be worse than one that says what it
    needs. Under the synchronous client nothing here starts an event loop: the
    calls to the control plane are made on whichever thread LangChain is already
    running the agent on. One background thread does get started, and only while
    a tool body or a live model call is running: the heartbeat that keeps the
    run's lease from lapsing under a driver that never went anywhere (see
    :meth:`salvor.langchain.RunTape._beating`). Under the asynchronous client
    that is a task on the loop instead.

    ``wrap_tool_call`` exists only inside ``create_agent``. A hand-built
    ``StateGraph`` calling tools in its own node has no hook for the middleware
    to sit in, so such a graph gets model recording only, and its tool calls
    stay outside the ledger.
    """

    def __init__(
        self,
        client: AnyClient,
        *,
        thread_id_to_run_id: Optional[ThreadIdToRunId] = None,
        record_prompts: bool = False,
        on_fork: Optional[OnFork] = None,
    ) -> None:
        """
        Args:
            client: The control plane every thread's run is opened against, and
                the choice of how the agent is driven. A
                :class:`~salvor.Client` records under ``agent.invoke`` and
                ``agent.stream``; an :class:`~salvor.AsyncClient` records under
                ``agent.ainvoke`` and ``agent.astream``.
            thread_id_to_run_id: The run id for a LangGraph ``thread_id``. The
                default is :func:`~salvor.langchain.run_id_for_thread`: a
                thread id that is already a UUID is used as the run id
                unchanged, anything else is hashed into one. Replace it when
                your thread ids and your run ids are kept in a table
                somewhere. May return an awaitable of the id when the client is
                an :class:`~salvor.AsyncClient`.
            record_prompts: Record each model request's body on its intent, so
                an inspector can show the exact prompt. Off by default,
                because the body carries user data. Replay never reads it: the
                correlation key is the request hash alone.
            on_fork: Told once per invocation, the first time the thread leaves
                what its run recorded. The default writes one warning to the
                ``salvor.langchain`` logger (:func:`warn_of_fork`); pass your
                own to route it, and pass one that does nothing to silence it.
                A fork is never fatal: the invoke carries on, appending to the
                run, and every message it returns from there on says so in
                ``response_metadata["salvor"]["forked"]``.
        """
        super().__init__()
        if not isinstance(client, (Client, AsyncClient)):
            raise SalvorMiddlewareError(
                "SalvorMiddleware records over a salvor client: pass "
                "`Client(...)` to record under `agent.invoke`, or "
                "`AsyncClient(...)` to record under `await agent.ainvoke`. It "
                "was given a {kind}.".format(kind=type(client).__name__),
                code="wrong_client",
            )
        self._client = client
        #: True when the client is asynchronous, which is what decides whether
        #: the awaited hooks or the blocking ones do the work.
        self._awaited = isinstance(client, AsyncClient)
        self._to_run_id = thread_id_to_run_id or run_id_for_thread
        self._record_prompts = record_prompts
        self._on_fork = on_fork if on_fork is not None else warn_of_fork
        #: One tape per live invocation, keyed by run id.
        self._tapes = {}  # type: Dict[str, Any]
        #: In-flight asynchronous opens, so a turn's parallel tool calls share
        #: one open rather than racing each other for the lease.
        self._opening = {}  # type: Dict[str, asyncio.Task]
        #: The same guarantee for the synchronous path, where a turn's parallel
        #: tool calls arrive on a thread pool rather than on one event loop.
        self._open_lock = threading.Lock()
        #: The server's client-tool declarations, by name, read once and kept:
        #: what they say about a tool cannot change under a running server, and
        #: what this middleware needs from them (whether the operator lets a
        #: client close a call) must be known before the tool's result is
        #: reported. ``None`` until the first tool call asks.
        self._declared = None  # type: Optional[Dict[str, Any]]
        self._declared_lock = threading.Lock()

    @property
    def name(self) -> str:
        return "SalvorMiddleware"

    # -- the synchronous hooks -------------------------------------------------

    def before_agent(self, state: Any, runtime: Any) -> None:
        """Take up the thread's run for this invocation.

        Opening here rather than lazily is what makes a second invoke start
        from a clean cursor: even an invocation that failed halfway and never
        reached ``after_agent`` leaves nothing behind that the next one would
        inherit.
        """
        self._blocking_hook()
        self._tapes.pop(self._identify(runtime)["run_id"], None)
        self._tape_for(runtime)
        return None

    def after_agent(self, state: Any, runtime: Any) -> None:
        """Let go of the run: hand the lease back, and forget the cursor.

        The log is the durable part; the cursor is not, and neither is the
        lease. Releasing here is what lets the next process (or the next
        invoke, from anywhere) take this thread up immediately instead of
        being refused `lease_held` for the rest of the lease TTL over a drive
        that is already finished. An invoke that ends by raising never reaches
        this hook at all, which is why the steps release too (see
        :meth:`salvor.langchain.RunTape.step`).
        """
        self._blocking_hook()
        tape = self._tapes.pop(self._identify(runtime)["run_id"], None)
        if tape is not None:
            tape.release()
        return None

    def wrap_model_call(self, request: Any, handler: Any) -> Any:
        """Record the model call, or return the recorded answer.

        The live call is LangChain's: the intent is opened with a hash of the
        request, ``handler`` sends it with whatever provider and key the app
        configured, and the answer is recorded. Salvor never sees the request
        and never holds the key.
        """
        self._blocking_hook()
        tape = self._tape_for(getattr(request, "runtime", None))
        live = {}  # type: Dict[str, Any]

        def perform() -> ModelAnswer:
            response = handler(request)
            live["response"] = response
            return _answer_of(response, tape.run_id)

        # `step` is what hands the lease back when this call is where the
        # invoke dies: a provider error, a refusal from salvor, anything the
        # graph raises through here. `after_agent` never runs after one of
        # those.
        with tape.step():
            outcome = tape.model_call(
                request_hash(request), canonical_request(request), perform
            )
            if not outcome.replayed and "response" in live:
                return _marked_live(live["response"], outcome, tape.run_id)
            # The recorded answer goes back through LangChain's own handler,
            # with a stand-in model in the provider's place, so a streaming
            # caller sees the replayed turn arrive whole instead of seeing
            # nothing at all. See `replay_model.py` for why that indirection is
            # worth having.
            return handler(_replaying(request, outcome, tape.run_id))

    def wrap_tool_call(self, request: Any, handler: Any) -> Any:
        """Record the tool call, or return the recorded result.

        The intent goes in before the tool runs, which is the write-ahead rule:
        a call that was asked for and never reported is visible in the log as
        exactly that, rather than being indistinguishable from a call nobody
        made. The turnstile inside the tape is what lets a model turn ask for
        several tools at once: LangChain runs them on a thread pool, and they
        are recorded one after another, in the order the model listed them,
        with none of them refused.
        """
        self._blocking_hook()
        tape = self._tape_for(getattr(request, "runtime", None))
        name = request.tool_call["name"]
        live = {}  # type: Dict[str, Any]
        # Read before the call, used after it: a tool whose completion the
        # operator does not trust still runs, and what must not happen is
        # finding that out only once the result is being reported.
        trusted = self._trusts_completion(name)

        def perform(opened: OpenedCall) -> Any:
            def body() -> Any:
                live["message"] = _live_tool_message(handler(request), name)
                output = tool_output(live["message"])
                _stop_for_a_person(trusted, tape, opened, name, output)
                return output

            return run_with_tool_call(_context(opened, tape.run_id, name), body)

        # A tool body that raises, and a `ToolNeedsResolution` that stops for a
        # person, both leave the invoke through here, and `after_agent` runs
        # for neither: `step` is what gives the lease back on those paths.
        with tape.step():
            try:
                outcome = tape.tool_call(
                    name, _tool_args(request), perform, _turn_position(request), trusted
                )
            except SalvorAPIError as error:
                undeclared = _undeclared_tool_error(error, name)
                if undeclared is None:
                    raise
                raise undeclared from error

            if not outcome.replayed and "message" in live:
                return mark(live["message"], outcome.marker)
            return _replayed_tool_message(outcome, request, name)

    # -- the awaited hooks ------------------------------------------------------

    async def abefore_agent(self, state: Any, runtime: Any) -> None:
        """:meth:`before_agent`, awaited."""
        self._awaited_hook()
        identity = await self._aidentify(runtime)
        self._tapes.pop(identity["run_id"], None)
        await self._atape_for(runtime)
        return None

    async def aafter_agent(self, state: Any, runtime: Any) -> None:
        """:meth:`after_agent`, awaited: the lease goes back here too."""
        self._awaited_hook()
        identity = await self._aidentify(runtime)
        tape = self._tapes.pop(identity["run_id"], None)
        if tape is not None:
            await tape.release()
        return None

    async def awrap_model_call(self, request: Any, handler: Any) -> Any:
        """:meth:`wrap_model_call`, awaited."""
        self._awaited_hook()
        tape = await self._atape_for(getattr(request, "runtime", None))
        live = {}  # type: Dict[str, Any]

        async def perform() -> ModelAnswer:
            response = await handler(request)
            live["response"] = response
            return _answer_of(response, tape.run_id)

        async with tape.step():
            outcome = await tape.model_call(
                request_hash(request), canonical_request(request), perform
            )
            if not outcome.replayed and "response" in live:
                return _marked_live(live["response"], outcome, tape.run_id)
            return await handler(_replaying(request, outcome, tape.run_id))

    async def awrap_tool_call(self, request: Any, handler: Any) -> Any:
        """:meth:`wrap_tool_call`, awaited. The turn's calls arrive as tasks on
        one event loop here rather than on a thread pool, and are admitted in
        the same order either way."""
        self._awaited_hook()
        tape = await self._atape_for(getattr(request, "runtime", None))
        name = request.tool_call["name"]
        live = {}  # type: Dict[str, Any]
        trusted = await self._atrusts_completion(name)

        async def perform(opened: OpenedCall) -> Any:
            async def body() -> Any:
                live["message"] = _live_tool_message(await handler(request), name)
                output = tool_output(live["message"])
                _stop_for_a_person(trusted, tape, opened, name, output)
                return output

            return await arun_with_tool_call(_context(opened, tape.run_id, name), body)

        async with tape.step():
            try:
                outcome = await tape.tool_call(
                    name, _tool_args(request), perform, _turn_position(request), trusted
                )
            except SalvorAPIError as error:
                undeclared = _undeclared_tool_error(error, name)
                if undeclared is None:
                    raise
                raise undeclared from error

            if not outcome.replayed and "message" in live:
                return mark(live["message"], outcome.marker)
            return _replayed_tool_message(outcome, request, name)

    # -- the run, blocking -------------------------------------------------------

    def _identify(self, runtime: Any) -> Dict[str, str]:
        """The thread this hook is running for, and the run id it maps to."""
        thread_id = _required_thread_id(runtime)
        run_id = self._to_run_id(thread_id)
        if not isinstance(run_id, str):
            raise SalvorMiddlewareError(
                "`thread_id_to_run_id` returned something to await, and this "
                "middleware was given salvor's synchronous `Client`, which has "
                "nothing to await it with. Return the run id itself, or pass an "
                "`AsyncClient` and drive the agent with `ainvoke`.",
                code="wrong_client",
            )
        return {"thread_id": thread_id, "run_id": run_id}

    def _tape_for(self, runtime: Any) -> RunTape:
        """This invocation's tape, opening the run once however many hooks ask.

        A model turn's parallel tool calls reach this from several pool threads
        at the same moment, so the lease is opened under a lock and the rest of
        them find the tape already there.
        """
        identity = self._identify(runtime)
        run_id = identity["run_id"]
        with self._open_lock:
            existing = self._tapes.get(run_id)
            if existing is not None:
                return existing
            try:
                driver = self._open(run_id)
            except SalvorAPIError as error:
                _refusal_of_an_open(identity["thread_id"], run_id, error)
                raise
            # From here the lease is held, and no tape owns it yet: a run this
            # invoke may not drive (a finished thread) or a first append that
            # is refused would otherwise leave the thread locked until the
            # lease lapsed, over an invoke that never started.
            try:
                _refuse_a_finished_run(
                    driver.log_envelopes, identity["thread_id"], run_id
                )
                tape = RunTape.open(
                    driver,
                    _started(identity["thread_id"]),
                    self._drive(identity["thread_id"], run_id),
                )
            except BaseException as error:
                _hand_back(driver, error)
                raise
            self._tapes[run_id] = tape
            return tape

    def _open(self, run_id: str) -> Any:
        """Take up the run's lease, which is also how it is taken back after
        a restart (see :meth:`salvor.langchain.RunTape._guarded`).

        Presents no token of its own: the client this middleware was given
        (:class:`~salvor.Client` or :class:`~salvor.AsyncClient`) remembers
        the last one it saw for this run and fills it in automatically, so
        this middleware's own next invoke of a thread it drove moments ago
        is not refused `lease_held` by a lease it minted itself. See
        :attr:`salvor.Client._client_run_tokens`.
        """
        return self._client.open_client_run(
            run_id=run_id, record_prompts=self._record_prompts
        )

    def _drive(self, thread_id: str, run_id: str) -> Drive:
        """What this invocation drives the run with: the thread every refusal
        names, whether prompts are recorded, how to take the run back, and who
        hears about a fork."""
        return Drive(
            thread_id=thread_id,
            record_prompts=self._record_prompts,
            reopen=lambda: self._open(run_id),
            on_fork=self._on_fork,
        )

    # -- the run, awaited ---------------------------------------------------------

    async def _aidentify(self, runtime: Any) -> Dict[str, str]:
        """:meth:`_identify`, awaiting a mapping that answers with an awaitable."""
        thread_id = _required_thread_id(runtime)
        run_id = self._to_run_id(thread_id)
        if not isinstance(run_id, str):
            run_id = await run_id
        return {"thread_id": thread_id, "run_id": run_id}

    async def _atape_for(self, runtime: Any) -> AsyncRunTape:
        """:meth:`_tape_for`, awaited.

        A model turn's parallel tool calls all reach this at the same moment,
        so the first one to arrive parks its open in ``_opening`` and the rest
        await that same task rather than opening the run again and racing each
        other for the lease.
        """
        identity = await self._aidentify(runtime)
        run_id = identity["run_id"]
        existing = self._tapes.get(run_id)
        if existing is not None:
            return existing
        in_flight = self._opening.get(run_id)
        if in_flight is not None:
            return await in_flight
        started = asyncio.ensure_future(
            self._aopen_tape(run_id, identity["thread_id"])
        )
        self._opening[run_id] = started
        try:
            tape = await started
        finally:
            self._opening.pop(run_id, None)
        self._tapes[run_id] = tape
        return tape

    async def _aopen_tape(self, run_id: str, thread_id: str) -> AsyncRunTape:
        try:
            driver = await self._open(run_id)
        except SalvorAPIError as error:
            _refusal_of_an_open(thread_id, run_id, error)
            raise
        # The lease is held from here and no tape owns it yet; see
        # :meth:`_tape_for` for why it goes back by hand on these two paths.
        try:
            _refuse_a_finished_run(driver.log_envelopes, thread_id, run_id)
            return await AsyncRunTape.open(
                driver, _started(thread_id), self._drive(thread_id, run_id)
            )
        except BaseException as error:
            await _ahand_back(driver, error)
            raise

    # -- what the operator declared -------------------------------------------------

    def _trusts_completion(self, tool: str) -> bool:
        """Whether the operator lets a client close a call to ``tool``.

        The declarations are read once per middleware and kept, because they are
        the server's startup configuration (``salvor serve --client-tool
        <FILE>``) and cannot change while it runs. A tool this server declares
        nothing about is treated as trusted here, so the refusal that reaches
        the application is the one that names the missing declaration, raised
        where it already was: when the intent is opened, before the tool runs.
        """
        if self._declared is None:
            with self._declared_lock:
                if self._declared is None:
                    self._declared = _by_name(self._client.list_client_tools())
        return _trusted(self._declared, tool)

    async def _atrusts_completion(self, tool: str) -> bool:
        """:meth:`_trusts_completion`, awaited.

        Two tool calls of one turn may both find the listing unread and both
        read it. That costs one extra GET and nothing else: the answer is the
        same either way, and a lock held across an await would be a second
        turnstile in a file that already has one.
        """
        if self._declared is None:
            self._declared = _by_name(await self._client.list_client_tools())
        return _trusted(self._declared, tool)

    # -- which way this agent is being driven --------------------------------------

    def _blocking_hook(self) -> None:
        """Refuse a synchronous drive of an asynchronous client."""
        if self._awaited:
            raise SalvorMiddlewareError(
                "SalvorMiddleware was given salvor's asynchronous "
                "`AsyncClient`, so this agent has to be driven asynchronously: "
                "`await agent.ainvoke(...)` or `agent.astream(...)` rather than "
                "`agent.invoke(...)`. To drive it synchronously, give the "
                "middleware salvor's synchronous `Client(...)` instead.",
                code="wrong_client",
            )

    def _awaited_hook(self) -> None:
        """Refuse an asynchronous drive of a synchronous client."""
        if not self._awaited:
            raise SalvorMiddlewareError(
                "SalvorMiddleware was given salvor's synchronous `Client`, so "
                "this agent has to be driven synchronously: `agent.invoke(...)` "
                "or `agent.stream(...)` rather than `await "
                "agent.ainvoke(...)`. To drive it asynchronously, give the "
                "middleware salvor's asynchronous `AsyncClient(...)` instead.",
                code="wrong_client",
            )


def salvor_middleware(
    client: AnyClient,
    *,
    thread_id_to_run_id: Optional[ThreadIdToRunId] = None,
    record_prompts: bool = False,
    on_fork: Optional[OnFork] = None,
) -> SalvorMiddleware:
    """Build a :class:`SalvorMiddleware`.

    The function form, for an app that reads better with one; it takes the same
    arguments and means the same thing by each of them, ``Client`` for an agent
    driven with ``invoke`` and ``AsyncClient`` for one driven with ``ainvoke``.
    """
    return SalvorMiddleware(
        client,
        thread_id_to_run_id=thread_id_to_run_id,
        record_prompts=record_prompts,
        on_fork=on_fork,
    )


# -- helpers -------------------------------------------------------------------


def _started(thread_id: str) -> Dict[str, Any]:
    """What a fresh run's ``RunStarted`` records."""
    return {
        "agent_def_hash": hash_value(AGENT_DEF),
        "input": {"thread_id": thread_id},
    }


def _refusal_of_an_open(thread_id: str, run_id: str, error: SalvorAPIError) -> None:
    """Name the two refusals an open has that the thread explains.

    `lease_held` (another driver's current lease refused this open outright)
    becomes the one-driver refusal, naming the thread, the run, and how long
    the hold has left. `run_exists` becomes the other-mode refusal, naming the
    thread whose id collided. Every other refusal bubbles unchanged: this
    middleware did not cause it and cannot fix it by putting a thread id in
    front of it.
    """
    if held_by_another_driver(error):
        raise one_driver_error(thread_id, run_id, error) from error
    if error.code == "run_exists":
        raise server_driven_run(thread_id, run_id, error) from error


def _refuse_a_finished_run(log: List[Event], thread_id: str, run_id: str) -> None:
    """Refuse an invoke of a thread somebody has already finished, before
    anything tries to append to a closed run."""
    if log and log[-1].kind == "RunCompleted":
        raise SalvorMiddlewareError(
            "thread `{thread}` (run {run}) is finished: `finish_thread` "
            "recorded its `RunCompleted`, and a completed run cannot be "
            "appended to. Give the next task a new thread id.".format(
                thread=thread_id, run=run_id
            ),
            code="thread_finished",
        )


def _required_thread_id(runtime: Any) -> str:
    """The LangGraph thread id this hook is running for, refused when it is not
    a usable one.

    Two refusals, not one, because the two have different fixes. Nothing passed
    at all is ``thread_id_missing``: add the config. Something passed that this
    middleware cannot use as a run id is ``thread_id_invalid``, and the sentence
    says what arrived, because the usual cause is an application whose own ids
    are integers or an empty string standing in for "no thread".
    """
    thread_id = _thread_id(runtime)
    if thread_id is None:
        raise SalvorMiddlewareError(
            "SalvorMiddleware needs a thread id: invoke the agent with "
            '`config={"configurable": {"thread_id": "..."}}`. The thread id '
            "is the run id, so without one there is nothing for a later "
            "invoke to resume.",
            code="thread_id_missing",
        )
    if not isinstance(thread_id, str) or not thread_id:
        raise SalvorMiddlewareError(
            "SalvorMiddleware was given a thread id of {received}, and needs a "
            "non-empty string: `config={{\"configurable\": {{\"thread_id\": "
            '"order-7781"}}}}`. The thread id is the run id (a UUID is used '
            "unchanged, anything else is hashed into one), so an id of another "
            "type has to be spelled as a string by the application that owns "
            "it: `str(order_id)` records the same thread every "
            "time.".format(received=_describe(thread_id)),
            code="thread_id_invalid",
        )
    return thread_id


def _describe(thread_id: Any) -> str:
    """What arrived where a thread id was wanted, for the refusal to name."""
    if isinstance(thread_id, str):
        return "an empty string"
    return "{kind} ({value!r})".format(kind=type(thread_id).__name__, value=thread_id)


def _thread_id(runtime: Any) -> Any:
    """The LangGraph thread id, from wherever this hook can reach it, as it was
    passed rather than as this middleware wishes it had been.

    A tool call's runtime carries the ``RunnableConfig`` itself; a model call's
    does not, so the config is read from the ambient one LangGraph sets around
    every node it runs. ``None`` means no thread id was passed at all, which is
    a different refusal from one of the wrong type (see
    :func:`_required_thread_id`), so the value comes back unfiltered.
    """
    config = getattr(runtime, "config", None)
    if not isinstance(config, dict):
        try:
            from langgraph.config import get_config

            config = get_config()
        except Exception:
            config = None
    if not isinstance(config, dict):
        return None
    configurable = config.get("configurable")
    if not isinstance(configurable, dict):
        return None
    return configurable.get("thread_id")


def _hand_back(driver: Any, error: BaseException) -> None:
    """Give back a lease no tape ever took up, on the way out of ``error``.

    The one-driver refusals are left alone: the lease they name is another
    driver's, and this one has none to hand back. Nothing raised here reaches
    the caller, because the error already on its way out is the one worth
    seeing.
    """
    if not still_ours(error):
        return
    try:
        driver.release()
    except Exception as refused:  # noqa: BLE001 - the lease lapses either way
        LOG.debug("salvor: run %s kept its lease: %s", driver.run_id, refused)


async def _ahand_back(driver: Any, error: BaseException) -> None:
    """:func:`_hand_back`, awaited."""
    if not still_ours(error):
        return
    try:
        await driver.release()
    except Exception as refused:  # noqa: BLE001 - the lease lapses either way
        LOG.debug("salvor: run %s kept its lease: %s", driver.run_id, refused)


def _tool_args(request: Any) -> Dict[str, Any]:
    """The arguments the model produced for this call."""
    return request.tool_call.get("args") or {}


def _context(opened: OpenedCall, run_id: str, tool: str) -> ToolCallContext:
    """What the tool body about to run will read from ``current_tool_call()``."""
    return ToolCallContext(
        key=opened.idempotency_key, seq=opened.seq, run_id=run_id, tool=tool
    )


def _turn_position(request: Any) -> Optional[TurnPosition]:
    """Where this tool call sits in the model turn that asked for it.

    Read from the state rather than from arrival order, because arrival order
    is not the model's order in Python (see
    :meth:`salvor.langchain.tape.Tape.admitted`). The AI message that listed
    this call is found by its call id, and the rank is the call's index in that
    message's ``tool_calls``.

    ``None`` when the position cannot be read: a call with no id, a state this
    middleware cannot walk, or an id no recorded turn claims. The tape then
    admits the call on arrival, which is the best a call whose turn is unknown
    can be given.
    """
    call_id = request.tool_call.get("id")
    if not call_id:
        return None
    for message in reversed(_messages_of(request.state)):
        calls = getattr(message, "tool_calls", None)
        if not calls:
            continue
        ids = [call.get("id") for call in calls]
        if call_id not in ids:
            continue
        turn = getattr(message, "id", None) or "|".join(str(one) for one in ids)
        return TurnPosition(turn=str(turn), rank=ids.index(call_id), total=len(ids))
    return None


def _messages_of(state: Any) -> list:
    """The conversation on an agent state, however that state is shaped."""
    if isinstance(state, dict):
        messages = state.get("messages")
    elif isinstance(state, list):
        messages = state
    else:
        messages = getattr(state, "messages", None)
    return list(messages) if messages else []


def _answer_of(response: Any, run: str) -> ModelAnswer:
    """What a live model call reports back to the tape: the stored form of the
    AI message it produced, and the token counts it cost."""
    answer = _ai_message_of(response, run)
    return stored_form(answer), usage_of(answer)


def _replaying(request: Any, outcome: ModelOutcome, run: str) -> Any:
    """The same model request with the recorded answer in the provider's place."""
    recorded = mark(stored_ai_message(outcome.response, run), outcome.marker)
    return request.override(model=ReplayChatModel(recorded))


def _marked_live(response: Any, outcome: ModelOutcome, run: str) -> Any:
    """The live response, with its AI message saying it was live.

    Marking the message this middleware just recorded is what makes the absence
    of a marker mean nothing at all: every answer a salvor-recorded agent hands
    back says where it came from, so a reader never has to guess whether an
    unmarked message was live or came from a build without the middleware. The
    marker rides on ``response_metadata``, which no request hash reads, so it
    cannot change what the next model call hashes to.
    """
    mark(_ai_message_of(response, run), outcome.marker)
    return response


def _live_tool_message(result: Any, tool: str) -> ToolMessage:
    """The tool message a handler returned, spelt the way the log will spell it.

    A JSON result is rewritten into its canonical form here, on the live path,
    because the replayed message is built from the recorded value and would
    otherwise carry different bytes for the same result. See
    :func:`~salvor.langchain.messages.canonical_tool_message`.
    """
    return canonical_tool_message(_tool_message_of(result, tool))


def _by_name(declarations: Any) -> Dict[str, Any]:
    """The server's client-tool declarations, by tool name."""
    return {declaration.name: declaration for declaration in declarations or []}


def _trusted(declared: Dict[str, Any], tool: str) -> bool:
    """Whether ``tool``'s declaration lets a client report its own result."""
    declaration = declared.get(tool)
    return declaration is None or bool(
        getattr(declaration, "trust_completion", False)
    )


def _stop_for_a_person(
    trusted: bool, tape: Any, opened: OpenedCall, tool: str, output: Any
) -> None:
    """Stop after a call the operator settles by hand, before reporting it.

    The tool has run by now, which is right: performing the call is the
    application's business and the intent is recorded ahead of it either way.
    What this middleware must not do is report the result, because salvor
    refuses a completion for a tool declared ``trust_completion = false`` and
    the refusal would arrive as a bare ``403`` in the middle of a graph, after
    the write. The log is left saying exactly what happened: the call was asked
    for, and nobody has confirmed what it did.
    """
    if trusted:
        return
    raise ToolNeedsResolution(
        run_id=tape.run_id,
        thread_id=tape.thread_id,
        seq=opened.seq,
        tool=tool,
        output=output,
        key=opened.idempotency_key,
    )


def _tool_message_of(result: Any, tool: str) -> ToolMessage:
    """The tool message a handler returned, refusing graph control flow."""
    if not isinstance(result, ToolMessage):
        raise SalvorMiddlewareError(
            "the tool `{tool}` returned a LangGraph Command rather than a tool "
            "message. A Command is graph control flow, not a recorded result, "
            "so this middleware cannot put it in the log. Return a value or a "
            "ToolMessage from tools you want recorded.".format(tool=tool),
            code="tool_returned_command",
        )
    return result


def _replayed_tool_message(
    outcome: ToolOutcome, request: Any, tool: str
) -> ToolMessage:
    """The recorded result of a tool call, as the message LangGraph expects.

    The content is the canonical spelling of the recorded value, which is
    exactly what the live message carried, so the model call after it hashes to
    the position the log already holds.
    """
    return mark(
        ToolMessage(
            content=as_tool_content(outcome.output),
            tool_call_id=request.tool_call.get("id") or "",
            name=tool,
            # A recorded completion is, by construction, a call that reported a
            # result: salvor refuses to record one any other way.
            status="success",
        ),
        outcome.marker,
    )


def _ai_message_of(response: Any, run: str) -> AIMessage:
    """The AI message a model call produced, from whichever shape the handler
    returned it in.

    LangChain's handler answers with a ``ModelResponse`` whose ``result`` is
    usually one ``AIMessage`` and occasionally that message plus a tool message
    carrying structured output. The answer this middleware records is the AI
    message: that is what a later invoke has to hand back.
    """
    if isinstance(response, AIMessage):
        return response
    result = getattr(response, "result", None)
    if result is None:
        model_response = getattr(response, "model_response", None)
        result = getattr(model_response, "result", None)
    for message in result or []:
        if isinstance(message, AIMessage):
            return message
    raise SalvorMiddlewareError(
        "run {run} performed a model call that produced no AI message, so "
        "there is nothing to record at this position. A model whose handler "
        "answers with something else cannot be recorded by this "
        "middleware.".format(run=run),
        code="unreadable_record",
    )


def _undeclared_tool_error(
    error: SalvorAPIError, tool: str
) -> Optional[SalvorMiddlewareError]:
    """Turn the server's ``unknown_tool`` refusal into the sentence that fixes
    it, or ``None`` when the refusal was about something else.

    The middleware cannot declare the tool itself, and should not want to: a
    declaration fixes whether a call is a write, and code that performs the
    write must not be the code that decides that.
    """
    if error.code != "unknown_tool":
        return None
    return SalvorMiddlewareError(
        "the tool `{tool}` has no client-tool declaration on this salvor "
        "server, so its call cannot be recorded. Write a declaration for it: a "
        'TOML file with `name = "{tool}"`, an `effect` (`read`, `idempotent` '
        "or `write`), an `[input_schema]` matching the tool's parameters, and, "
        "so the middleware may record what the tool returned, "
        "`trust_completion = true` with an `[output_schema]`. Then start the "
        "server with `salvor serve --client-tool <FILE>`. See "
        "examples/client-tools/refund-card.toml.".format(tool=tool),
        code="tool_undeclared",
    )
