"""The middleware: one line added to an agent somebody already wrote.

Everything else in this package exists to serve the four hooks below. They open
the thread's run, hash the model request, record what the model and the tools
did, and hand the recorded answers back on a re-invoke.
"""

from __future__ import annotations

import asyncio
from typing import Any, Awaitable, Callable, Dict, Optional, Union

from langchain.agents.middleware import AgentMiddleware
from langchain_core.messages import AIMessage, ToolMessage

from ..async_client import AsyncClient
from ..errors import SalvorAPIError
from .current_call import ToolCallContext, run_with_tool_call
from .errors import SalvorMiddlewareError
from .hash import hash_value, run_id_for_thread
from .messages import (
    as_tool_content,
    mark,
    stored_ai_message,
    stored_form,
    tool_output,
    usage_of,
)
from .replay_model import ReplayChatModel
from .request import canonical_request, request_hash
from .tape import OpenedCall, RunTape, TurnPosition

__all__ = ["SalvorMiddleware", "salvor_middleware"]

#: What a run's `RunStarted` records as its agent definition. The middleware is
#: the definition here: LangGraph owns the graph, and salvor is told only that
#: this run's calls came through this adapter. The string names the adapter
#: rather than the language, and is the one the TypeScript middleware writes,
#: so a thread's run reads the same whichever SDK opened it.
AGENT_DEF = {"middleware": "@salvor-run/client/langchain"}

ThreadIdToRunId = Callable[[str], Union[str, Awaitable[str]]]


class SalvorMiddleware(AgentMiddleware):
    """A LangChain middleware that records this agent's model and tool calls in
    a salvor run, and replays them on a re-invoke of the same thread.

        from langchain.agents import create_agent
        from salvor import AsyncClient
        from salvor.langchain import SalvorMiddleware

        agent = create_agent(
            model=model,
            tools=tools,
            middleware=[SalvorMiddleware(AsyncClient("http://127.0.0.1:8080"))],
        )

        await agent.ainvoke(
            {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
            {"configurable": {"thread_id": "order-7781"}},
        )

    Asynchronous, deliberately. Every call this middleware makes to the control
    plane is a request, and the hooks it sits in have an awaited form
    (``awrap_model_call``, ``awrap_tool_call``), so the whole path is one
    coroutine and no thread or nested event loop is involved anywhere. Drive
    the agent with ``ainvoke`` and ``astream``; the synchronous hooks are
    implemented only to refuse ``invoke`` by name, because a middleware that
    quietly recorded nothing would be worse than one that says what it needs.

    ``wrap_tool_call`` exists only inside ``create_agent``. A hand-built
    ``StateGraph`` calling tools in its own node has no hook for the middleware
    to sit in, so such a graph gets model recording only, and its tool calls
    stay outside the ledger.
    """

    def __init__(
        self,
        client: AsyncClient,
        *,
        thread_id_to_run_id: Optional[ThreadIdToRunId] = None,
        record_prompts: bool = False,
    ) -> None:
        """
        Args:
            client: The control plane every thread's run is opened against.
            thread_id_to_run_id: The run id for a LangGraph ``thread_id``. The
                default is :func:`~salvor.langchain.run_id_for_thread`: a
                thread id that is already a UUID is used as the run id
                unchanged, anything else is hashed into one. Replace it when
                your thread ids and your run ids are kept in a table
                somewhere. May return the id or an awaitable of it.
            record_prompts: Record each model request's body on its intent, so
                an inspector can show the exact prompt. Off by default,
                because the body carries user data. Replay never reads it: the
                correlation key is the request hash alone.
        """
        super().__init__()
        self._client = client
        self._to_run_id = thread_id_to_run_id or run_id_for_thread
        self._record_prompts = record_prompts
        #: One tape per live invocation, keyed by run id.
        self._tapes = {}  # type: Dict[str, RunTape]
        #: In-flight opens, so a turn's parallel tool calls share one open.
        self._opening = {}  # type: Dict[str, asyncio.Task]

    @property
    def name(self) -> str:
        return "SalvorMiddleware"

    # -- the run --------------------------------------------------------------

    async def _identify(self, runtime: Any) -> Dict[str, str]:
        """The thread this hook is running for, and the run id it maps to."""
        thread_id = _thread_id(runtime)
        if not isinstance(thread_id, str) or not thread_id:
            raise SalvorMiddlewareError(
                "SalvorMiddleware needs a thread id: invoke the agent with "
                '`config={"configurable": {"thread_id": "..."}}`. The thread id '
                "is the run id, so without one there is nothing for a later "
                "invoke to resume."
            )
        run_id = self._to_run_id(thread_id)
        if not isinstance(run_id, str):
            run_id = await run_id
        return {"thread_id": thread_id, "run_id": run_id}

    async def _open_tape(self, run_id: str, thread_id: str) -> RunTape:
        driver = await self._client.open_client_run(
            run_id=run_id, record_prompts=self._record_prompts
        )
        log = driver.log_envelopes
        if log and log[-1].kind == "RunCompleted":
            raise SalvorMiddlewareError(
                "thread `{thread}` (run {run}) is finished: `finish_thread` "
                "recorded its `RunCompleted`, and a completed run cannot be "
                "appended to. Give the next task a new thread id.".format(
                    thread=thread_id, run=run_id
                )
            )
        return await RunTape.open(
            driver,
            {"agent_def_hash": hash_value(AGENT_DEF), "input": {"thread_id": thread_id}},
            self._record_prompts,
        )

    async def _tape_for(self, runtime: Any) -> RunTape:
        """This invocation's tape, opening the run once however many hooks ask.

        A model turn's parallel tool calls all reach this at the same moment,
        so the first one to arrive parks its open in ``_opening`` and the rest
        await that same task rather than opening the run again and racing each
        other for the lease.
        """
        identity = await self._identify(runtime)
        run_id = identity["run_id"]
        existing = self._tapes.get(run_id)
        if existing is not None:
            return existing
        in_flight = self._opening.get(run_id)
        if in_flight is not None:
            return await in_flight
        started = asyncio.ensure_future(
            self._open_tape(run_id, identity["thread_id"])
        )
        self._opening[run_id] = started
        try:
            tape = await started
        finally:
            self._opening.pop(run_id, None)
        self._tapes[run_id] = tape
        return tape

    # -- the hooks ------------------------------------------------------------

    async def abefore_agent(self, state: Any, runtime: Any) -> None:
        """Take up the thread's run for this invocation.

        Opening here rather than lazily is what makes a second invoke start
        from a clean cursor: even an invocation that failed halfway and never
        reached ``after_agent`` leaves nothing behind that the next one would
        inherit.
        """
        identity = await self._identify(runtime)
        self._tapes.pop(identity["run_id"], None)
        await self._tape_for(runtime)
        return None

    async def aafter_agent(self, state: Any, runtime: Any) -> None:
        """Let go of the run. The log is the durable part; the cursor is not."""
        identity = await self._identify(runtime)
        self._tapes.pop(identity["run_id"], None)
        return None

    async def awrap_model_call(self, request: Any, handler: Any) -> Any:
        """Record the model call, or return the recorded answer.

        The live call is LangChain's: the intent is opened with a hash of the
        request, ``handler`` sends it with whatever provider and key the app
        configured, and the answer is recorded. Salvor never sees the request
        and never holds the key.
        """
        tape = await self._tape_for(getattr(request, "runtime", None))
        wanted = request_hash(request)
        live = {}  # type: Dict[str, Any]

        async def perform():
            response = await handler(request)
            answer = _ai_message_of(response, tape.run_id)
            live["response"] = response
            return stored_form(answer), usage_of(answer)

        outcome = await tape.model_call(wanted, canonical_request(request), perform)
        if not outcome.replayed and "response" in live:
            return live["response"]
        # The recorded answer goes back through LangChain's own handler, with a
        # stand-in model in the provider's place, so a streaming caller sees the
        # replayed turn arrive whole instead of seeing nothing at all. See
        # `replay_model.py` for why that indirection is worth having.
        recorded = mark(
            stored_ai_message(outcome.response, tape.run_id), outcome.seq, tape.run_id
        )
        return await handler(request.override(model=ReplayChatModel(recorded)))

    async def awrap_tool_call(self, request: Any, handler: Any) -> Any:
        """Record the tool call, or return the recorded result.

        The intent goes in before the tool runs, which is the write-ahead rule:
        a call that was asked for and never reported is visible in the log as
        exactly that, rather than being indistinguishable from a call nobody
        made. The turnstile inside the tape is what lets a model turn ask for
        several tools at once: they are recorded one after another, in the
        order the model listed them, and none of them is refused.
        """
        tape = await self._tape_for(getattr(request, "runtime", None))
        name = request.tool_call["name"]
        args = request.tool_call.get("args") or {}
        live = {}  # type: Dict[str, Any]

        async def perform(opened: OpenedCall) -> Any:
            context = ToolCallContext(
                key=opened.idempotency_key,
                seq=opened.seq,
                run_id=tape.run_id,
                tool=name,
            )

            async def body() -> Any:
                result = await handler(request)
                if not isinstance(result, ToolMessage):
                    raise SalvorMiddlewareError(
                        "the tool `{tool}` returned a LangGraph Command rather "
                        "than a tool message. A Command is graph control flow, "
                        "not a recorded result, so this middleware cannot put "
                        "it in the log. Return a value or a ToolMessage from "
                        "tools you want recorded.".format(tool=name)
                    )
                live["message"] = result
                return tool_output(result)

            return await run_with_tool_call(context, body)

        try:
            outcome = await tape.tool_call(name, args, perform, _turn_position(request))
        except SalvorAPIError as error:
            undeclared = _undeclared_tool_error(error, name)
            if undeclared is None:
                raise
            raise undeclared from error

        if not outcome.replayed and "message" in live:
            return live["message"]
        content = outcome.output
        return mark(
            ToolMessage(
                content=as_tool_content(content),
                tool_call_id=request.tool_call.get("id") or "",
                name=name,
                # A recorded completion is, by construction, a call that
                # reported a result: salvor refuses to record one any other way.
                status="success",
            ),
            outcome.seq,
            tape.run_id,
        )

    # -- the synchronous hooks -------------------------------------------------

    def before_agent(self, state: Any, runtime: Any) -> None:
        raise _needs_async()

    def wrap_model_call(self, request: Any, handler: Any) -> Any:
        raise _needs_async()

    def wrap_tool_call(self, request: Any, handler: Any) -> Any:
        raise _needs_async()


def salvor_middleware(
    client: AsyncClient,
    *,
    thread_id_to_run_id: Optional[ThreadIdToRunId] = None,
    record_prompts: bool = False,
) -> SalvorMiddleware:
    """Build a :class:`SalvorMiddleware`.

    The function form, for an app that reads better with one; it takes the same
    arguments and means the same thing by each of them.
    """
    return SalvorMiddleware(
        client,
        thread_id_to_run_id=thread_id_to_run_id,
        record_prompts=record_prompts,
    )


# -- helpers -------------------------------------------------------------------


def _needs_async() -> SalvorMiddlewareError:
    return SalvorMiddlewareError(
        "SalvorMiddleware records over an asynchronous salvor client, so the "
        "agent has to be driven asynchronously: use `await agent.ainvoke(...)` "
        "or `agent.astream(...)` rather than `agent.invoke(...)`. Salvor's "
        "synchronous `Client` is unaffected; it is this adapter that is async."
    )


def _thread_id(runtime: Any) -> Optional[str]:
    """The LangGraph thread id, from wherever this hook can reach it.

    A tool call's runtime carries the ``RunnableConfig`` itself; a model call's
    does not, so the config is read from the ambient one LangGraph sets around
    every node it runs.
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
    thread_id = configurable.get("thread_id")
    return thread_id if isinstance(thread_id, str) else None


def _turn_position(request: Any) -> Optional[TurnPosition]:
    """Where this tool call sits in the model turn that asked for it.

    Read from the state rather than from arrival order, because arrival order
    is not the model's order in Python (see :meth:`RunTape._await_turn`). The
    AI message that listed this call is found by its call id, and the rank is
    the call's index in that message's ``tool_calls``.

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


def _ai_message_of(response: Any, run: str) -> AIMessage:
    """The AI message a model call produced, from whichever shape the handler
    returned it in.

    LangChain's async handler answers with a ``ModelResponse`` whose ``result``
    is usually one ``AIMessage`` and occasionally that message plus a tool
    message carrying structured output. The answer this middleware records is
    the AI message: that is what a later invoke has to hand back.
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
        "middleware.".format(run=run)
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
        "examples/client-tools/refund-card.toml.".format(tool=tool)
    )
