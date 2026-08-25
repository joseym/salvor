"""Proves the LangChain middleware against the real ``salvor serve``, with a
scripted model and no provider key anywhere.

Every case here drives an ordinary ``create_agent`` app. Nothing in the app
knows about salvor: the graph, the tools and the model are what a team would
already have written, and the middleware is the one line added to them. What the
cases check is what that line buys. A first invoke pays for the model calls and
runs the tools; a second invoke of the same thread pays for none of it, executes
none of it, and returns the same final message. A crash between a tool's intent
and its completion leaves the log saying exactly that, and the next invoke picks
the call up where it stopped, under the same derived key.

Every case is written once and run twice: once through ``agent.invoke`` over
salvor's synchronous ``Client``, and once through ``agent.ainvoke`` over its
``AsyncClient``. That is the parity this file exists to assert. The two
middlewares share their rules (``salvor/langchain/tape.py``) and differ only in
what they wait with, and a shared rule is only worth having if something notices
when one of the two stops obeying it. A scenario reaches its client and its
agent through :func:`call`, which awaits a coroutine and hands a plain value
straight back, so a body reads the same whichever transport is underneath it.
The synchronous class runs its scenarios with no event loop at all
(:func:`without_a_loop`), which is what proves the synchronous path never
reaches for one.

The model is a small ``BaseChatModel`` scripted turn by turn rather than one of
the fakes in ``langchain_core.language_models.fake_chat_models``. Those cannot
script a multi-turn tool-calling agent, and a fake whose ``bind_tools`` rebuilds
itself drops any counter attached to it, which is precisely the thing these
cases have to count. Both facts are checked by this file's own script: no key,
no network, one counter that survives binding. The tools carry a synchronous
body and an asynchronous one, because a tool a real app ships is called both
ways too.

Two cases pin values shared with the TypeScript suite: the run id a known thread
id maps to, and the hash of a known canonical request. Both were produced by
running ``sdks/typescript/dist/langchain/hash.js`` and are asserted here
verbatim, because a thread has to mean the same run and a request the same key
whichever SDK is driving.

The suite skips when LangChain is not installed, and the cases that need a
control plane skip when ``target/debug/salvor`` is not built. Run it with

    .venv/bin/python -m unittest tests.test_langchain
"""

from __future__ import annotations

import asyncio
import inspect
import json
import socket
import subprocess
import threading
import time
import unittest
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    import httpx
except ImportError:  # pragma: no cover - the SDK's one dependency
    raise unittest.SkipTest(
        "httpx is not installed; the LangChain middleware tests need the SDK's "
        "one dependency (pip install -e sdks/python)"
    ) from None

try:
    from langchain.agents import create_agent
    from langchain_core.language_models.chat_models import BaseChatModel
    from langchain_core.messages import AIMessage, BaseMessage, ToolMessage
    from langchain_core.outputs import ChatGeneration, ChatResult
    from langchain_core.tools import StructuredTool
except ImportError:  # pragma: no cover - depends on what is installed
    raise unittest.SkipTest(
        "LangChain is not installed; install the extra to run these "
        "(pip install -e 'sdks/python[langchain]')"
    ) from None

from salvor import AsyncClient, Client
from salvor.langchain import (
    SalvorMiddleware,
    SalvorMiddlewareError,
    canonical_request,
    current_tool_call,
    finish_thread,
    hash_value,
    is_uuid,
    request_hash,
    run_id_for_thread,
)

REPO_ROOT = Path(__file__).resolve().parents[3]
SALVOR = REPO_ROOT / "target" / "debug" / "salvor"
DECLS = [
    Path(__file__).resolve().parent / "client-tools" / "lookup-order.toml",
    Path(__file__).resolve().parent / "client-tools" / "stamp-ledger.toml",
]


# -- driving either transport ---------------------------------------------------


async def call(method, *args, **kwargs):
    """Call one method, whichever transport it belongs to.

    A coroutine is awaited; a plain return value is handed back as it is. This
    is what lets a scenario be written once: ``await call(client.get_run, id)``
    reads the same against both clients, and the difference stays here.
    """
    result = method(*args, **kwargs)
    if inspect.isawaitable(result):
        return await result
    return result


async def collect(stream):
    """Collect a message stream to a list, sync iterator or async iterator."""
    if hasattr(stream, "__aiter__"):
        return [item async for item in stream]
    return list(stream)


def without_a_loop(coro):
    """Run a scenario that never actually suspends, with no event loop at all.

    The synchronous middleware records over a synchronous client, so a
    synchronous scenario reaches the end of itself in one step: nothing on the
    path awaits anything that yields. Stepping the coroutine by hand rather than
    handing it to ``asyncio.run`` is what makes that a claim the suite checks
    rather than one it assumes. A scenario that did suspend would have found an
    event loop under the synchronous path, and there is not supposed to be one.
    """
    try:
        coro.send(None)
    except StopIteration as done:
        return done.value
    coro.close()
    raise AssertionError(
        "the synchronous scenario suspended, so something on the invoke path "
        "waited on an event loop the synchronous middleware must not need"
    )


# -- the scripted model --------------------------------------------------------


class ScriptedModel(BaseChatModel):
    """A model that answers turn by turn from a script and counts how often it
    was actually asked.

    The turn is chosen from the history (how many AI messages the conversation
    already holds) rather than from the counter, so a replayed invoke that skips
    the model entirely still lines up with the script.
    """

    turns: List[Dict[str, Any]]
    #: Shared by reference across every model this one binds itself into, so a
    #: bound copy still counts into the same place. Annotated `Any` on purpose:
    #: pydantic copies a `dict` field as it validates it, and a copied counter
    #: is a counter that stops counting the moment the agent binds its tools.
    calls: Any
    #: Read by the canonical-request vector; a real provider would have these.
    model: str = "vector-model"
    temperature: int = 0

    @property
    def _llm_type(self) -> str:
        return "scripted-fake"

    def bind_tools(self, tools: Any, **kwargs: Any) -> "ScriptedModel":
        return ScriptedModel(turns=self.turns, calls=self.calls)

    def _answer(self, messages: List[BaseMessage]) -> ChatResult:
        index = min(
            len([m for m in messages if m.type == "ai"]), len(self.turns) - 1
        )
        turn = self.turns[index]
        self.calls["count"] += 1
        message = AIMessage(
            content=turn["content"],
            id="scripted-{index}".format(index=index),
            tool_calls=[
                dict(call, type="tool_call") for call in turn.get("tool_calls") or []
            ],
            usage_metadata={"input_tokens": 11, "output_tokens": 5, "total_tokens": 16},
        )
        return ChatResult(generations=[ChatGeneration(message=message)])

    def _generate(
        self,
        messages: List[BaseMessage],
        stop: Optional[List[str]] = None,
        run_manager: Any = None,
        **kwargs: Any,
    ) -> ChatResult:
        return self._answer(messages)

    async def _agenerate(
        self,
        messages: List[BaseMessage],
        stop: Optional[List[str]] = None,
        run_manager: Any = None,
        **kwargs: Any,
    ) -> ChatResult:
        return self._answer(messages)


# -- the tools -----------------------------------------------------------------

#: How often each tool body actually ran, and how many ran at once. Guarded,
#: because the synchronous agent runs a turn's tool calls on a thread pool.
ran = {"lookup": 0, "stamp": 0, "concurrent": 0, "peak_concurrent": 0}
counting = threading.Lock()
#: Set to make the next `stamp_ledger` body raise, standing in for a crash.
stamp_crashes = {"on": False}
#: What `current_tool_call()` reported inside each `lookup_order` body that ran,
#: with the thread it ran on: `call` is the last one, `calls` is all of them.
captured = {"call": None, "calls": []}  # type: Dict[str, Any]


def enter() -> None:
    with counting:
        ran["concurrent"] += 1
        ran["peak_concurrent"] = max(ran["peak_concurrent"], ran["concurrent"])


def leave() -> None:
    with counting:
        ran["concurrent"] -= 1


def count(what: str) -> None:
    with counting:
        ran[what] += 1


def capture_call(order_id: str) -> None:
    """What this tool body was told about the call it is running inside of, and
    which thread told it."""
    context = current_tool_call()
    with counting:
        captured["call"] = context
        captured["calls"].append(
            {
                "order_id": order_id,
                "thread": threading.get_ident(),
                "key": getattr(context, "key", None),
                "seq": getattr(context, "seq", None),
                "tool": getattr(context, "tool", None),
            }
        )


def lookup_body(order_id: str) -> Dict[str, Any]:
    """Look up an order that has already been placed."""
    enter()
    try:
        capture_call(order_id)
        time.sleep(0.015)
        count("lookup")
        return {"order_id": order_id, "status": "paid", "total_cents": 4200}
    finally:
        leave()


async def alookup_body(order_id: str) -> Dict[str, Any]:
    enter()
    try:
        capture_call(order_id)
        await asyncio.sleep(0.015)
        count("lookup")
        return {"order_id": order_id, "status": "paid", "total_cents": 4200}
    finally:
        leave()


def stamp_body(order_id: str, note: str) -> Dict[str, Any]:
    """Write one line into the order's ledger."""
    enter()
    try:
        count("stamp")
        if stamp_crashes["on"]:
            raise RuntimeError("the ledger writer died mid-call")
        return {"order_id": order_id, "entry_id": "entry-{n}".format(n=len(note))}
    finally:
        leave()


async def astamp_body(order_id: str, note: str) -> Dict[str, Any]:
    return stamp_body(order_id, note)


def email_body(to: str) -> Dict[str, Any]:
    """Send an email. Deliberately never declared to salvor."""
    return {"sent": True}


async def aemail_body(to: str) -> Dict[str, Any]:
    return {"sent": True}


def both_ways(func: Any, coroutine: Any, name: str) -> StructuredTool:
    """One tool with both bodies, the way an app that is driven both ways ships
    its tools: LangChain calls the coroutine under ``ainvoke`` and the plain
    function under ``invoke``, and both count into the same place."""
    return StructuredTool.from_function(
        func=func, coroutine=coroutine, name=name, description=func.__doc__
    )


lookup_order = both_ways(lookup_body, alookup_body, "lookup_order")
stamp_ledger = both_ways(stamp_body, astamp_body, "stamp_ledger")
send_email = both_ways(email_body, aemail_body, "send_email")


def reset() -> None:
    ran.update({"lookup": 0, "stamp": 0, "concurrent": 0, "peak_concurrent": 0})
    stamp_crashes["on"] = False
    captured["call"] = None
    captured["calls"] = []


# -- the server ----------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_until_up(base: str) -> bool:
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            response = httpx.get("{base}/v1/client-tools".format(base=base), timeout=0.5)
            if response.status_code == 200:
                return True
        except httpx.HTTPError:
            pass
        time.sleep(0.1)
    return False


ONE_TOOL_SCRIPT = [
    {
        "content": "looking that up",
        "tool_calls": [
            {"name": "lookup_order", "args": {"order_id": "ORD-7781"}, "id": "call-1"}
        ],
    },
    {"content": "Order ORD-7781 is paid, 4200 cents."},
]

THREE_TOOL_SCRIPT = [
    {
        "content": "looking all three up",
        "tool_calls": [
            {"name": "lookup_order", "args": {"order_id": "ORD-A"}, "id": "call-a"},
            {"name": "lookup_order", "args": {"order_id": "ORD-B"}, "id": "call-b"},
            {"name": "lookup_order", "args": {"order_id": "ORD-C"}, "id": "call-c"},
        ],
    },
    {"content": "All three are paid."},
]

ASK = {"messages": [{"role": "user", "content": "how is ORD-7781?"}]}


class MiddlewareScenarios:
    """The whole middleware surface against the real control-plane binary,
    written once. Two subclasses bind the two ways of driving an agent.

    A mixin rather than a ``TestCase`` on purpose: unittest would otherwise
    collect and run the bodies a third time with no transport bound.
    """

    #: Bound by each subclass: the client, the client that is wrong for it, and
    #: the names of the two agent entry points that go with it.
    CLIENT: type = Client
    WRONG_CLIENT: type = AsyncClient
    INVOKE = "invoke"
    STREAM = "stream"
    #: The client the refusal has to name when the wrong one was passed.
    NAMES_CLIENT = "`Client(...)`"

    proc: subprocess.Popen
    base: str

    @classmethod
    def setUpClass(cls) -> None:
        if not SALVOR.exists():
            raise unittest.SkipTest(
                "build the binary first (cargo build): {path}".format(path=SALVOR)
            )
        port = free_port()
        cls.base = "http://127.0.0.1:{port}".format(port=port)
        store = "/tmp/salvor-py-langchain-{port}.db".format(port=port)
        Path(store).unlink(missing_ok=True)
        declarations = []  # type: List[str]
        for path in DECLS:
            declarations += ["--client-tool", str(path)]
        cls.proc = subprocess.Popen(
            [str(SALVOR), "--store", store, "serve", "--bind", "127.0.0.1:{port}".format(port=port)]
            + declarations,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={"PATH": "/usr/bin:/bin"},
        )
        if not wait_until_up(cls.base):
            cls.tearDownClass()
            raise unittest.SkipTest("salvor serve did not come up")

    @classmethod
    def tearDownClass(cls) -> None:
        proc = getattr(cls, "proc", None)
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:  # pragma: no cover
                proc.kill()

    def setUp(self) -> None:
        reset()

    # -- harness --------------------------------------------------------------

    def drive(self, body: Any) -> Any:
        """Run one scenario body with this class's client, closed either way."""
        raise NotImplementedError

    def dispose(self, client: Any) -> None:
        """Close a client of the kind this class does NOT drive with."""
        raise NotImplementedError

    async def invoke(self, agent: Any, message_in: Any, config: Any) -> Any:
        """This class's way of running an agent to completion."""
        return await call(getattr(agent, self.INVOKE), message_in, config)

    def messages_of(self, agent: Any, message_in: Any, config: Any) -> Any:
        """This class's way of streaming an agent's messages."""
        return getattr(agent, self.STREAM)(message_in, config, stream_mode="messages")

    def agent_for(self, turns: List[Dict[str, Any]], client: Any, tools: Any = None):
        model = ScriptedModel(turns=turns, calls={"count": 0})
        agent = create_agent(
            model=model,
            tools=list(tools if tools is not None else [lookup_order, stamp_ledger]),
            middleware=[SalvorMiddleware(client)],
        )
        return agent, model

    async def kinds_of(self, client: Any, thread_id: str) -> List[str]:
        run = await call(client.open_client_run, run_id=run_id_for_thread(thread_id))
        return [event.kind for event in run.log_envelopes]

    async def intents_of(self, client: Any, thread_id: str) -> List[Any]:
        run = await call(client.open_client_run, run_id=run_id_for_thread(thread_id))
        return [
            event for event in run.log_envelopes if event.kind == "ToolCallRequested"
        ]

    @staticmethod
    def text_of(message: BaseMessage) -> str:
        content = message.content
        return content if isinstance(content, str) else json.dumps(content)

    @staticmethod
    def thread(config_thread: str) -> Dict[str, Any]:
        return {"configurable": {"thread_id": config_thread}}

    # -- (a) and (b): record a run, then replay it ----------------------------

    def test_a_run_records_one_model_call_and_one_tool_call_then_replays_both(self) -> None:
        thread_id = "thread-record-and-replay"

        async def body(client: Any) -> None:
            # (a) The first invoke pays for everything, and the log says so.
            agent, model = self.agent_for(ONE_TOOL_SCRIPT, client)
            answer = await self.invoke(agent, ASK, self.thread(thread_id))
            self.assertEqual(model.calls["count"], 2, "the tool turn and the answer")
            self.assertEqual(ran["lookup"], 1, "the tool body ran once")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                ],
            )
            final = self.text_of(answer["messages"][-1])
            self.assertEqual(final, "Order ORD-7781 is paid, 4200 cents.")

            # (b) The second invoke of the same thread pays for nothing at all.
            reset()
            again_agent, again_model = self.agent_for(ONE_TOOL_SCRIPT, client)
            again = await self.invoke(again_agent, ASK, self.thread(thread_id))
            self.assertEqual(again_model.calls["count"], 0, "zero model calls on replay")
            self.assertEqual(ran["lookup"], 0, "zero tool executions on replay")
            replayed = again["messages"][-1]
            self.assertEqual(self.text_of(replayed), final, "the same final message")
            marker = replayed.response_metadata["salvor"]
            self.assertIs(marker["replayed"], True)
            self.assertEqual(marker["seq"], 5, "the second model call sat at seq 5")
            self.assertEqual(marker["run"], run_id_for_thread(thread_id))
            self.assertEqual(
                len(await self.kinds_of(client, thread_id)), 7, "the replay wrote nothing"
            )

        self.drive(body)

    # -- (c) a crash between a tool's intent and its completion ---------------

    def test_a_crash_mid_write_leaves_a_dangling_intent_the_next_invoke_picks_up(self) -> None:
        thread_id = "thread-crash-mid-write"
        script = [
            {
                "content": "stamping the ledger",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": "ORD-9001", "note": "seen"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "Stamped ORD-9001."},
        ]
        ask = {"messages": [{"role": "user", "content": "stamp ORD-9001"}]}

        async def body(client: Any) -> None:
            # The tool dies after salvor recorded the intent and before anything
            # could report a result, which is the shape of every real mid-write
            # crash.
            stamp_crashes["on"] = True
            crashed, _ = self.agent_for(script, client)
            with self.assertRaises(RuntimeError) as caught:
                await self.invoke(crashed, ask, self.thread(thread_id))
            self.assertIn("ledger writer died", str(caught.exception))
            self.assertEqual(ran["stamp"], 1, "the tool body ran once and raised")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                ],
                "the log ends at the intent: a write asked for and never reported",
            )

            # The next invoke replays the model call for free, meets the
            # dangling intent, performs the call once more under the same
            # derived key, and closes it.
            reset()
            recovered, model = self.agent_for(script, client)
            answer = await self.invoke(recovered, ask, self.thread(thread_id))
            self.assertEqual(model.calls["count"], 1, "only the answer turn was live")
            self.assertEqual(ran["stamp"], 1, "the unfinished write ran once more")
            self.assertEqual(self.text_of(answer["messages"][-1]), "Stamped ORD-9001.")

            kinds = await self.kinds_of(client, thread_id)
            self.assertEqual(
                kinds,
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                ],
            )
            self.assertEqual(kinds.count("ToolCallRequested"), 1, "exactly one intent")
            self.assertEqual(kinds.count("ToolCallCompleted"), 1, "exactly one completion")

        self.drive(body)

    # -- (d) two tool calls in one model turn ---------------------------------

    def test_two_parallel_tool_calls_are_serialised_by_the_turnstile(self) -> None:
        thread_id = "thread-parallel-tools"
        script = [
            {
                "content": "looking both up",
                "tool_calls": [
                    {"name": "lookup_order", "args": {"order_id": "ORD-1"}, "id": "call-a"},
                    {"name": "lookup_order", "args": {"order_id": "ORD-2"}, "id": "call-b"},
                ],
            },
            {"content": "Both orders are paid."},
        ]
        ask = {"messages": [{"role": "user", "content": "check ORD-1 and ORD-2"}]}

        async def body(client: Any) -> None:
            agent, model = self.agent_for(script, client)
            answer = await self.invoke(agent, ask, self.thread(thread_id))
            self.assertEqual(model.calls["count"], 2)
            self.assertEqual(ran["lookup"], 2, "both tool calls executed")
            self.assertEqual(
                ran["peak_concurrent"], 1, "never two at once: the turnstile held the second"
            )
            self.assertEqual(self.text_of(answer["messages"][-1]), "Both orders are paid.")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                ],
            )

            # The order the model asked for is the order the log recorded, which
            # is what makes the pair replayable rather than merely serialized.
            intents = await self.intents_of(client, thread_id)
            self.assertEqual(
                [intent.payload["input"]["order_id"] for intent in intents],
                ["ORD-1", "ORD-2"],
            )

            # And a replay of the whole turn touches neither model nor tools.
            reset()
            second, second_model = self.agent_for(script, client)
            await self.invoke(second, ask, self.thread(thread_id))
            self.assertEqual(second_model.calls["count"], 0)
            self.assertEqual(ran["lookup"], 0)

        self.drive(body)

    def test_a_turn_records_its_tool_calls_in_the_model_order_every_time(self) -> None:
        """The ordering the turnstile exists for, run enough times to catch the
        turn being scheduled differently.

        Neither transport reaches the middleware in the model's order. Under
        ``ainvoke`` the same three-tool turn arrived in three different orders
        across five runs of a bare probe middleware; under ``invoke`` a thread
        pool decides. So the recorded order is taken from the AI message rather
        than from arrival, and this is the case that says so.
        """
        ask = {"messages": [{"role": "user", "content": "check all three"}]}

        async def body(client: Any) -> None:
            for attempt in range(5):
                reset()
                thread_id = "thread-order-{n}".format(n=attempt)
                agent, _ = self.agent_for(THREE_TOOL_SCRIPT, client)
                await self.invoke(agent, ask, self.thread(thread_id))
                self.assertEqual(ran["lookup"], 3)
                self.assertEqual(ran["peak_concurrent"], 1, "one at a time")
                intents = await self.intents_of(client, thread_id)
                self.assertEqual(
                    [intent.payload["input"]["order_id"] for intent in intents],
                    ["ORD-A", "ORD-B", "ORD-C"],
                )

        self.drive(body)

    # -- (e) a replayed answer under streaming --------------------------------

    def test_a_replayed_answer_streams_as_one_whole_chunk_marked_replayed(self) -> None:
        thread_id = "thread-streaming-replay"

        async def body(client: Any) -> None:
            first, first_model = self.agent_for(ONE_TOOL_SCRIPT, client)
            await self.invoke(first, ASK, self.thread(thread_id))
            self.assertEqual(first_model.calls["count"], 2)

            reset()
            second, second_model = self.agent_for(ONE_TOOL_SCRIPT, client)
            streamed = await collect(
                self.messages_of(second, ASK, self.thread(thread_id))
            )
            chunks = [
                message for message, _metadata in streamed if message.type == "ai"
            ]

            self.assertEqual(second_model.calls["count"], 0, "the stream paid for nothing")
            self.assertEqual(ran["lookup"], 0)
            self.assertEqual(
                len(chunks), 2, "one whole chunk per recorded model call, never re-tokenised"
            )
            for chunk in chunks:
                marker = chunk.response_metadata.get("salvor")
                self.assertIsNotNone(marker, "every replayed answer says it was replayed")
                self.assertIs(marker["replayed"], True)
                self.assertIsInstance(marker["seq"], int)
            self.assertEqual(
                [chunk.response_metadata["salvor"]["seq"] for chunk in chunks], [1, 5]
            )
            self.assertEqual(self.text_of(chunks[1]), "Order ORD-7781 is paid, 4200 cents.")

        self.drive(body)

    # -- (f) a tool nobody declared -------------------------------------------

    def test_a_tool_with_no_client_tool_declaration_is_refused_by_name(self) -> None:
        script = [
            {
                "content": "emailing",
                "tool_calls": [
                    {"name": "send_email", "args": {"to": "ops@example.com"}, "id": "call-mail"}
                ],
            },
            {"content": "Sent."},
        ]

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(
                script, client, tools=[lookup_order, stamp_ledger, send_email]
            )
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "email ops"}]},
                    self.thread("thread-undeclared-tool"),
                )
            text = str(caught.exception)
            self.assertIn("send_email", text, "the error names the tool")
            self.assertIn("client-tool declaration", text, "and the declaration it needs")
            self.assertIn("--client-tool", text, "and how to load it")

        self.drive(body)

    # -- leaving the recorded path --------------------------------------------

    def test_an_invoke_off_the_recorded_path_appends_instead_of_replaying(self) -> None:
        thread_id = "thread-second-question"

        async def body(client: Any) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await self.invoke(first, ASK, self.thread(thread_id))
            self.assertEqual(len(await self.kinds_of(client, thread_id)), 7)

            # A different question down the same thread is a different first
            # model call, so nothing at the recorded positions applies. The run
            # carries on at the end of its log rather than pretending the old
            # answers are still answers.
            reset()
            second, model = self.agent_for(
                [{"content": "ORD-9999 is not one of ours."}], client
            )
            answer = await self.invoke(
                second,
                {"messages": [{"role": "user", "content": "how is ORD-9999?"}]},
                self.thread(thread_id),
            )
            self.assertEqual(model.calls["count"], 1, "the new question was asked for real")
            self.assertEqual(
                self.text_of(answer["messages"][-1]), "ORD-9999 is not one of ours."
            )
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[7:],
                ["ModelCallRequested", "ModelCallCompleted"],
            )

        self.drive(body)

    # -- (g) finish_thread closes a thread's run ------------------------------

    def test_finish_thread_completes_the_run_and_a_further_invoke_is_refused(self) -> None:
        thread_id = "thread-finish"

        async def body(client: Any) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            answer = await self.invoke(first, ASK, self.thread(thread_id))
            final = self.text_of(answer["messages"][-1])

            run_id = run_id_for_thread(thread_id)
            finished = await call(finish_thread, client, thread_id)
            self.assertEqual(finished.run_id, run_id)
            self.assertEqual((await self.kinds_of(client, thread_id))[-1], "RunCompleted")

            state = await call(client.get_run, run_id)
            self.assertEqual(state.status.state, "completed")
            self.assertEqual(
                state.status.raw.get("output"),
                final,
                "the default output is the last AI message",
            )

            # A further invoke on the finished thread is refused, clearly,
            # rather than failing somewhere inside the append.
            reset()
            second, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(second, ASK, self.thread(thread_id))
            text = str(caught.exception)
            self.assertIn("thread-finish", text, "the error names the thread")
            self.assertIn("finish", text.lower(), "and says it is finished")

        self.drive(body)

    # -- (h) current_tool_call() inside a tool body ---------------------------

    def test_a_tool_body_reads_the_key_the_intent_recorded_on_both_invokes(self) -> None:
        thread_id = "thread-current-tool-call"

        async def body(client: Any) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await self.invoke(first, ASK, self.thread(thread_id))
            context = captured["call"]
            self.assertIsNotNone(context, "the tool body read a current call")
            self.assertEqual(context.tool, "lookup_order")
            self.assertEqual(context.run_id, run_id_for_thread(thread_id))

            intent = (await self.intents_of(client, thread_id))[0]
            self.assertEqual(context.seq, intent.seq, "the seq matches the recorded intent")
            self.assertEqual(
                context.key,
                intent.payload["idempotency_key"],
                "the key is the one salvor recorded on the intent",
            )

            # A replayed invoke never runs the tool body, so nothing new is
            # captured, but the log's own recorded key is unchanged.
            reset()
            second, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await self.invoke(second, ASK, self.thread(thread_id))
            self.assertIsNone(captured["call"], "the replay never ran the tool body")
            self.assertEqual(ran["lookup"], 0)

            replayed_intent = (await self.intents_of(client, thread_id))[0]
            self.assertEqual(
                replayed_intent.payload["idempotency_key"],
                intent.payload["idempotency_key"],
                "the recorded key is identical on replay",
            )

        self.drive(body)

    # -- (i) finish_thread refuses a thread with an open intent ---------------

    def test_finish_thread_on_an_open_intent_is_refused_naming_the_run(self) -> None:
        thread_id = "thread-finish-open-intent"
        script = [
            {
                "content": "stamping the ledger",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": "ORD-4242", "note": "seen"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "Stamped ORD-4242."},
        ]

        async def body(client: Any) -> None:
            stamp_crashes["on"] = True
            crashed, _ = self.agent_for(script, client)
            with self.assertRaises(RuntimeError):
                await self.invoke(
                    crashed,
                    {"messages": [{"role": "user", "content": "stamp ORD-4242"}]},
                    self.thread(thread_id),
                )
            stamp_crashes["on"] = False

            run_id = run_id_for_thread(thread_id)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(finish_thread, client, thread_id)
            text = str(caught.exception)
            self.assertIn(run_id, text, "the error names the run")
            self.assertIn("never completed", text, "and says the call was never completed")

            # Nothing was appended: the log still ends at the same open intent.
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[-1],
                "ToolCallRequested",
                "finish_thread wrote nothing",
            )

        self.drive(body)

    # -- a thread id is required ----------------------------------------------

    def test_an_invoke_with_no_thread_id_is_refused(self) -> None:
        async def body(client: Any) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(getattr(agent, self.INVOKE), ASK)
            self.assertIn("thread id", str(caught.exception))

        self.drive(body)

    # -- the wrong kind of client says which one to pass -----------------------

    def test_the_wrong_kind_of_client_is_refused_naming_the_one_to_pass(self) -> None:
        """A middleware built over the other client cannot record this drive, and
        says which client would, rather than recording nothing quietly."""
        wrong = self.WRONG_CLIENT(self.base)
        try:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, wrong)

            async def body(client: Any) -> None:
                await self.invoke(agent, ASK, self.thread("thread-wrong-client"))

            with self.assertRaises(SalvorMiddlewareError) as caught:
                self.drive(body)
            self.assertIn(self.NAMES_CLIENT, str(caught.exception))
        finally:
            self.dispose(wrong)


class SyncTransport(MiddlewareScenarios, unittest.TestCase):
    """Every scenario through ``agent.invoke`` over ``salvor.Client``."""

    CLIENT = Client
    WRONG_CLIENT = AsyncClient
    INVOKE = "invoke"
    STREAM = "stream"
    NAMES_CLIENT = "`Client(...)`"

    def drive(self, body: Any) -> Any:
        client = self.CLIENT(self.base)

        async def scenario() -> Any:
            try:
                return await body(client)
            finally:
                client.close()

        return without_a_loop(scenario())

    def dispose(self, client: Any) -> None:
        asyncio.run(client.close())

    # -- the thread pool a synchronous turn runs on ---------------------------

    def test_a_parallel_turn_runs_its_tool_bodies_on_worker_threads_in_rank_order(
        self,
    ) -> None:
        """The synchronous turnstile, in the place it actually has to work.

        LangChain's synchronous ``ToolNode`` dispatches a turn's tool calls to a
        thread pool, so the calls that have to wait for the rank before them are
        threads waiting on a ``threading.Condition``. This runs a three-tool
        turn and asks the three things that has to be true of: the log records
        the calls in the model's order, no two bodies overlap, and each body,
        away on its own worker thread, read back the key salvor derived for the
        call it was running inside of. ``current_tool_call()`` is a
        ``ContextVar``, and a ``ContextVar`` read on the wrong thread reads
        nothing, so this is the case that proves it is read on the right one.
        """
        thread_id = "thread-sync-worker-threads"
        ask = {"messages": [{"role": "user", "content": "check all three"}]}

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(THREE_TOOL_SCRIPT, client)
            await self.invoke(agent, ask, self.thread(thread_id))
            self.assertEqual(ran["lookup"], 3)
            self.assertEqual(ran["peak_concurrent"], 1, "one at a time")

            intents = await self.intents_of(client, thread_id)
            self.assertEqual(
                [intent.payload["input"]["order_id"] for intent in intents],
                ["ORD-A", "ORD-B", "ORD-C"],
                "the model's order is the log's order, whatever the pool did",
            )

            runs = captured["calls"]
            self.assertEqual(len(runs), 3, "three tool bodies ran")
            self.assertNotIn(
                threading.get_ident(),
                [record["thread"] for record in runs],
                "the tool bodies ran on the pool, not on the thread driving the agent",
            )
            for record, intent in zip(runs, intents):
                self.assertEqual(
                    record["key"],
                    intent.payload["idempotency_key"],
                    "each worker thread read the key of its own call",
                )
                self.assertEqual(record["seq"], intent.seq)
                self.assertEqual(record["tool"], "lookup_order")
            self.assertEqual(
                len({record["key"] for record in runs}),
                3,
                "three calls, three distinct derived keys",
            )

        self.drive(body)


class AsyncTransport(MiddlewareScenarios, unittest.TestCase):
    """Every scenario through ``await agent.ainvoke`` over ``salvor.AsyncClient``.

    Identical bodies to :class:`SyncTransport`, which is the assertion: a
    behaviour that drifted between the two would fail here and pass there.
    """

    CLIENT = AsyncClient
    WRONG_CLIENT = Client
    INVOKE = "ainvoke"
    STREAM = "astream"
    NAMES_CLIENT = "`AsyncClient(...)`"

    def drive(self, body: Any) -> Any:
        async def scenario() -> Any:
            async with AsyncClient(self.base) as client:
                return await body(client)

        return asyncio.run(scenario())

    def dispose(self, client: Any) -> None:
        client.close()


# -- the vectors shared with the TypeScript suite ------------------------------


class SharedVectors(unittest.TestCase):
    """The two values that have to be identical in both SDKs.

    A thread has to name the same run and a request the same recorded key
    whichever language is driving, so both are pinned here to the bytes the
    TypeScript implementation produces. Regenerate them, if either rule ever
    changes on purpose, with::

        node -e 'const h = await import("./sdks/typescript/dist/langchain/hash.js");
                 console.log(await h.runIdForThread("order-7781"))' --input-type=module
    """

    #: `sdks/typescript/test/langchain.test.ts` derives this thread id's run id
    #: and asserts its shape; this is the id itself, from `runIdForThread`.
    THREAD_ID = "order-7781"
    RUN_ID = "ae0b11d6-1425-82b1-9efd-d0f6def16f4a"

    #: One canonical request, and the `sha256:` key `requestHash` gives it.
    #: The messages are the suite's own first turn, so the vector is a request
    #: this middleware could actually be asked to hash.
    CANONICAL_REQUEST = {
        "messages": [
            {"content": "how is ORD-7781?", "role": "human"},
            {
                "content": "looking that up",
                "role": "ai",
                "tool_calls": [
                    {"args": {"order_id": "ORD-7781"}, "id": "call-1", "name": "lookup_order"}
                ],
            },
            {
                "content": '{"order_id":"ORD-7781","status":"paid","total_cents":4200}',
                "name": "lookup_order",
                "role": "tool",
                "tool_call_id": "call-1",
            },
        ],
        "model": {"model": "vector-model", "temperature": 0, "type": "scripted-fake"},
        "model_settings": {},
        "system": "You are a careful order assistant.",
    }
    CANONICAL_JSON = (
        '{"messages":[{"content":"how is ORD-7781?","role":"human"},'
        '{"content":"looking that up","role":"ai","tool_calls":'
        '[{"args":{"order_id":"ORD-7781"},"id":"call-1","name":"lookup_order"}]},'
        '{"content":"{\\"order_id\\":\\"ORD-7781\\",\\"status\\":\\"paid\\",'
        '\\"total_cents\\":4200}","name":"lookup_order","role":"tool",'
        '"tool_call_id":"call-1"}],"model":{"model":"vector-model",'
        '"temperature":0,"type":"scripted-fake"},"model_settings":{},'
        '"system":"You are a careful order assistant."}'
    )
    REQUEST_HASH = "sha256:335c51f638395676943b95304ecfe69e00f1bf22c6c50737d86e29489a071215"

    def test_the_thread_id_vector_is_the_run_id_the_typescript_sdk_derives(self) -> None:
        from salvor.langchain.hash import canonical_json

        self.assertEqual(run_id_for_thread(self.THREAD_ID), self.RUN_ID)
        self.assertEqual(canonical_json({"a": 1}), '{"a":1}')

    def test_the_request_hash_vector_is_the_key_the_typescript_sdk_derives(self) -> None:
        from salvor.langchain.hash import canonical_json

        self.assertEqual(canonical_json(self.CANONICAL_REQUEST), self.CANONICAL_JSON)
        self.assertEqual(hash_value(self.CANONICAL_REQUEST), self.REQUEST_HASH)

    def test_a_python_model_request_canonicalizes_to_the_shared_vector(self) -> None:
        """The vector is not a hand-written constant that nothing produces: the
        same request, built out of a Python ``ModelRequest``, canonicalizes to
        exactly it, and hashes to exactly the key the TypeScript SDK gives."""
        from langchain.agents.middleware import ModelRequest
        from langchain_core.messages import HumanMessage, SystemMessage

        request = ModelRequest(
            model=ScriptedModel(turns=ONE_TOOL_SCRIPT, calls={"count": 0}),
            messages=[
                HumanMessage(content="how is ORD-7781?"),
                AIMessage(
                    content="looking that up",
                    tool_calls=[
                        {
                            "name": "lookup_order",
                            "args": {"order_id": "ORD-7781"},
                            "id": "call-1",
                            "type": "tool_call",
                        }
                    ],
                ),
                ToolMessage(
                    content='{"order_id":"ORD-7781","status":"paid","total_cents":4200}',
                    name="lookup_order",
                    tool_call_id="call-1",
                ),
            ],
            system_message=SystemMessage(content="You are a careful order assistant."),
        )
        self.assertEqual(canonical_request(request), self.CANONICAL_REQUEST)
        self.assertEqual(request_hash(request), self.REQUEST_HASH)

    def test_a_uuid_thread_id_is_the_run_id_and_anything_else_is_hashed(self) -> None:
        uuid = "3f2504e0-4f89-41d3-9a0c-0305e82c3301"
        self.assertTrue(is_uuid(uuid))
        self.assertEqual(run_id_for_thread(uuid), uuid)
        self.assertEqual(run_id_for_thread(uuid.upper()), uuid)

        derived = run_id_for_thread("order-7781")
        self.assertEqual(derived[14], "8", "version 8: custom, hash-derived")
        self.assertIn(derived[19], "89ab", "the RFC's variant bits")
        self.assertEqual(run_id_for_thread("order-7781"), derived, "the mapping is stable")
        self.assertNotEqual(run_id_for_thread("order-7782"), derived)


# -- the plain entry must not pull LangChain in --------------------------------


class PlainImportStaysPlain(unittest.TestCase):
    """`import salvor` must not reach LangChain, whatever else is installed."""

    def test_importing_the_plain_sdk_loads_no_langchain_module(self) -> None:
        script = (
            "import sys\n"
            "import salvor\n"
            "salvor.AsyncClient\n"
            "salvor.GraphBuilder\n"
            "leaked = sorted(n for n in sys.modules if n.split('.')[0] "
            "in ('langchain', 'langchain_core', 'langgraph'))\n"
            "print(','.join(leaked))\n"
        )
        result = subprocess.run(
            [__import__("sys").executable, "-c", script],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(
            result.stdout.strip(),
            "",
            "the plain entry reached LangChain: {seen}".format(seen=result.stdout.strip()),
        )


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
