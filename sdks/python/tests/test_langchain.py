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
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import uuid
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
    from langchain.agents.middleware import AgentMiddleware
    from langchain_core.language_models.chat_models import BaseChatModel
    from langchain_core.messages import (
        AIMessage,
        BaseMessage,
        SystemMessage,
        ToolMessage,
    )
    from langchain_core.outputs import ChatGeneration, ChatResult
    from langchain_core.tools import StructuredTool
except ImportError:  # pragma: no cover - depends on what is installed
    raise unittest.SkipTest(
        "LangChain is not installed; install the extra to run these "
        "(pip install -e 'sdks/python[langchain]')"
    ) from None

from salvor import AsyncClient, Client, SalvorAPIError
from salvor.langchain import (
    SalvorMiddleware,
    SalvorMiddlewareError,
    ToolNeedsResolution,
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
    Path(__file__).resolve().parent / "client-tools" / "track-shipment.toml",
    Path(__file__).resolve().parent / "client-tools" / "wire-payout.toml",
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
ran = {
    "lookup": 0,
    "stamp": 0,
    "track": 0,
    "payout": 0,
    "concurrent": 0,
    "peak_concurrent": 0,
}
counting = threading.Lock()
#: Set to make the next `stamp_ledger` body raise, standing in for a crash.
stamp_crashes = {"on": False}
#: Something for a `lookup_order` body to do to the world while the middleware
#: holds that call's intent open: take the run's lease from under this drive, or
#: stop the server altogether. The cases that need one set it; every other case
#: leaves it alone and nothing happens.
meddling = {"do": None}  # type: Dict[str, Any]
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


def meddle() -> None:
    """Do whatever this case wants done between a tool's intent and its
    completion, which is where a lost lease actually hurts."""
    interference = meddling["do"]
    if interference is not None:
        interference()


def lookup_body(order_id: str) -> Dict[str, Any]:
    """Look up an order that has already been placed."""
    enter()
    try:
        capture_call(order_id)
        meddle()
        time.sleep(0.015)
        count("lookup")
        return {"order_id": order_id, "status": "paid", "total_cents": 4200}
    finally:
        leave()


async def alookup_body(order_id: str) -> Dict[str, Any]:
    enter()
    try:
        capture_call(order_id)
        meddle()
        await asyncio.sleep(0.015)
        count("lookup")
        return {"order_id": order_id, "status": "paid", "total_cents": 4200}
    finally:
        leave()


def track_body(order_id: str) -> Dict[str, Any]:
    """Track the shipment an order was sent on.

    Answers with its keys in an order sorting would move, which is the whole
    point of it: `tracking_number`, then `status`, then `eta`. What salvor hands
    back is sorted, so a middleware that recorded one spelling and replayed
    another would fork this thread on every invoke.
    """
    count("track")
    return {
        "tracking_number": "1Z-{order}".format(order=order_id),
        "status": "in_transit",
        "eta": "2026-08-27",
    }


async def atrack_body(order_id: str) -> Dict[str, Any]:
    return track_body(order_id)


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


def payout_body(order_id: str, amount_cents: int) -> Dict[str, Any]:
    """Wire a payout for an order whose card refund is not available."""
    count("payout")
    return {
        "provider_transfer_id": "wt-{order}".format(order=order_id),
        "status": "succeeded",
        "amount_cents": amount_cents,
    }


async def apayout_body(order_id: str, amount_cents: int) -> Dict[str, Any]:
    return payout_body(order_id, amount_cents)


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
track_shipment = both_ways(track_body, atrack_body, "track_shipment")
wire_payout = both_ways(payout_body, apayout_body, "wire_payout")
send_email = both_ways(email_body, aemail_body, "send_email")


def reset() -> None:
    ran.update(
        {
            "lookup": 0,
            "stamp": 0,
            "track": 0,
            "payout": 0,
            "concurrent": 0,
            "peak_concurrent": 0,
        }
    )
    stamp_crashes["on"] = False
    meddling["do"] = None
    captured["call"] = None
    captured["calls"] = []


class StampAfterTools(AgentMiddleware):
    """Change the system message of every model call that follows a tool call.

    A graph branching on something outside the log is the honest cause of most
    forks, and this is the smallest one that can be written down. It is applied
    only after a tool call so the fork lands partway down a recorded path rather
    than at its first step: the messages before it still replay, the messages
    after it do not, and a marker that only appeared on the first message after
    a fork would be caught by the case that uses this.
    """

    def __init__(self, nonce: str) -> None:
        super().__init__()
        self._stamp = SystemMessage(content="stamped {nonce}".format(nonce=nonce))

    @property
    def name(self) -> str:
        return "StampAfterTools"

    def _stamped(self, request: Any) -> Any:
        if not any(message.type == "tool" for message in request.messages):
            return request
        return request.override(system_message=self._stamp)

    def wrap_model_call(self, request: Any, handler: Any) -> Any:
        return handler(self._stamped(request))

    async def awrap_model_call(self, request: Any, handler: Any) -> Any:
        return await handler(self._stamped(request))


# -- the server ----------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def serve(port: int, store: str) -> subprocess.Popen:
    """One `salvor serve` on ``port`` over ``store``, with this suite's
    client-tool declarations loaded."""
    declarations = []  # type: List[str]
    for path in DECLS:
        declarations += ["--client-tool", str(path)]
    return subprocess.Popen(
        [
            str(SALVOR),
            "--store",
            store,
            "serve",
            "--bind",
            "127.0.0.1:{port}".format(port=port),
        ]
        + declarations,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={"PATH": "/usr/bin:/bin"},
    )


def stop(proc: Optional[subprocess.Popen]) -> None:
    """Stop a server this file started, hard if it will not stop gently."""
    if proc is None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:  # pragma: no cover
        proc.kill()


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
    #: This class's own directory under the system temp dir, holding the store
    #: the server writes and nothing else, removed however the class ends.
    workspace: str

    @classmethod
    def setUpClass(cls) -> None:
        if not SALVOR.exists():
            raise unittest.SkipTest(
                "build the binary first (cargo build): {path}".format(path=SALVOR)
            )
        port = free_port()
        cls.base = "http://127.0.0.1:{port}".format(port=port)
        cls.workspace = tempfile.mkdtemp(prefix="salvor-py-")
        cls.proc = serve(port, str(Path(cls.workspace) / "langchain.db"))
        if not wait_until_up(cls.base):
            cls.tearDownClass()
            raise unittest.SkipTest("salvor serve did not come up")

    @classmethod
    def tearDownClass(cls) -> None:
        stop(getattr(cls, "proc", None))
        # However this class ended, including the failures that skipped it in
        # `setUpClass`, its store goes with it: a suite that leaves databases
        # behind in the system temp directory is a suite nobody can run twice
        # on a small disk.
        workspace = getattr(cls, "workspace", None)
        if workspace is not None:
            shutil.rmtree(workspace, ignore_errors=True)

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

    def agent_for(
        self,
        turns: List[Dict[str, Any]],
        client: Any,
        tools: Any = None,
        on_fork: Any = None,
    ):
        model = ScriptedModel(turns=turns, calls={"count": 0})
        agent = create_agent(
            model=model,
            tools=list(tools if tools is not None else [lookup_order, stamp_ledger]),
            middleware=[SalvorMiddleware(client, on_fork=on_fork)],
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
            # Nobody passed an `on_fork`, so the default channel is what says
            # the thread left its recorded path: one warning on the
            # `salvor.langchain` logger, naming the thread and the position.
            with self.assertLogs("salvor.langchain", "WARNING") as warned:
                answer = await self.invoke(
                    second,
                    {"messages": [{"role": "user", "content": "how is ORD-9999?"}]},
                    self.thread(thread_id),
                )
            self.assertEqual(len(warned.records), 1, "one warning, once per invoke")
            self.assertIn(thread_id, warned.output[0])
            self.assertIn("seq 1", warned.output[0])
            self.assertEqual(model.calls["count"], 1, "the new question was asked for real")
            self.assertEqual(
                self.text_of(answer["messages"][-1]), "ORD-9999 is not one of ours."
            )
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[7:],
                ["ModelCallRequested", "ModelCallCompleted"],
            )

        self.drive(body)

    # -- (j) a tool result whose keys sorting would move ----------------------

    def test_a_tool_result_whose_keys_are_not_alphabetical_replays_for_nothing(
        self,
    ) -> None:
        """The spelling of a tool result must not decide whether a thread replays.

        Salvor stores what a tool returned as JSON and hands it back with its
        keys sorted; a Python dictionary comes back in the order the tool built
        it. If the live tool message carried one of those spellings and the
        replayed one the other, the model call after it would hash to a
        position the log does not hold: the thread would fork on every invoke,
        the write below the fork would run again every time, and it would run
        under a fresh key every time, which is the one thing an idempotency key
        exists to prevent. So both tools in this case answer with keys sorting
        would move, and the second invoke has to cost nothing at all.
        """
        thread_id = "thread-key-order"
        tools = [track_shipment, stamp_ledger]
        script = [
            {
                "content": "tracking it",
                "tool_calls": [
                    {
                        "name": "track_shipment",
                        "args": {"order_id": "ORD-5150"},
                        "id": "call-track",
                    }
                ],
            },
            {
                "content": "writing that down",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": "ORD-5150", "note": "in transit"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "ORD-5150 is in transit, and the ledger says so."},
        ]
        ask = {"messages": [{"role": "user", "content": "where is ORD-5150?"}]}

        def tool_texts(answer: Any) -> List[str]:
            return [
                self.text_of(message)
                for message in answer["messages"]
                if message.type == "tool"
            ]

        async def body(client: Any) -> None:
            agent, model = self.agent_for(script, client, tools=tools)
            answer = await self.invoke(agent, ask, self.thread(thread_id))
            self.assertEqual(model.calls["count"], 3, "two tool turns and the answer")
            self.assertEqual(ran["track"], 1)
            self.assertEqual(ran["stamp"], 1)
            final = self.text_of(answer["messages"][-1])
            self.assertEqual(len(await self.kinds_of(client, thread_id)), 11)

            # The live tool message carries the log's spelling, keys sorted,
            # which is the byte-for-byte thing a replayed one will carry.
            self.assertEqual(
                tool_texts(answer)[0],
                '{"eta":"2026-08-27","status":"in_transit",'
                '"tracking_number":"1Z-ORD-5150"}',
            )
            keys = [
                intent.payload["idempotency_key"]
                for intent in await self.intents_of(client, thread_id)
            ]
            self.assertEqual(len(keys), 2, "one intent per call, no more")

            # And the second invoke pays for nothing, runs nothing, and writes
            # nothing: the read replays, the write replays, and the write's
            # recorded key is the one the first invoke performed under.
            reset()
            again_agent, again_model = self.agent_for(script, client, tools=tools)
            again = await self.invoke(again_agent, ask, self.thread(thread_id))
            self.assertEqual(again_model.calls["count"], 0, "zero model calls")
            self.assertEqual(ran["track"], 0, "zero tool runs")
            self.assertEqual(ran["stamp"], 0, "the write did not happen twice")
            replayed = again["messages"][-1]
            self.assertEqual(self.text_of(replayed), final, "the same final message")
            self.assertIs(replayed.response_metadata["salvor"]["replayed"], True)
            self.assertEqual(tool_texts(again), tool_texts(answer), "the same bytes")
            self.assertEqual(
                len(await self.kinds_of(client, thread_id)), 11, "nothing appended"
            )
            self.assertEqual(
                [
                    intent.payload["idempotency_key"]
                    for intent in await self.intents_of(client, thread_id)
                ],
                keys,
                "one recorded key per write, identical on both invokes",
            )

        self.drive(body)

    # -- (k) a fork is marked and told about ----------------------------------

    def test_a_fork_marks_every_answer_after_it_and_tells_the_app_once(self) -> None:
        """Leaving the recorded path partway is marked, and said once.

        The fork is forced the only way a fork can be forced: by asking
        something the log does not hold at that position. A model's answer
        cannot do it on its own, because a replayed invoke never asks the model
        anything; what moves the tape off the path is the request at the cursor
        no longer being the request recorded there. Here a middleware ahead of
        salvor's stamps a different system message onto the model call that
        follows the tool call, which is what a graph branching on the clock
        looks like from the log's side.

        Everything before the fork still replays and still says so. Everything
        from the fork on says where the fork was, and the application has heard
        about it exactly once.
        """
        thread_id = "thread-fork-partway"
        run_id = run_id_for_thread(thread_id)

        def agent_stamped(turns: List[Dict[str, Any]], nonce: str, on_fork: Any):
            model = ScriptedModel(turns=turns, calls={"count": 0})
            agent = create_agent(
                model=model,
                tools=[lookup_order, stamp_ledger],
                middleware=[
                    StampAfterTools(nonce),
                    SalvorMiddleware(client_of[0], on_fork=on_fork),
                ],
            )
            return agent, model

        client_of = [None]  # type: List[Any]

        def never(fork: Any) -> None:
            raise AssertionError("the first invoke has no recorded path to leave")

        async def body(client: Any) -> None:
            client_of[0] = client
            first, first_model = agent_stamped(ONE_TOOL_SCRIPT, "one", never)
            answer = await self.invoke(first, ASK, self.thread(thread_id))
            self.assertEqual(first_model.calls["count"], 2)
            self.assertEqual(len(await self.kinds_of(client, thread_id)), 7)

            # Nothing forked, so every message says what it was: live, at the
            # position it was recorded at. Without this the absence of a marker
            # would be the only thing distinguishing a live message, and an
            # absence is not evidence of anything.
            messages = answer["messages"]
            self.assertEqual(
                messages[-1].response_metadata["salvor"],
                {"live": True, "seq": 5, "run": run_id},
            )
            tool_message = [m for m in messages if m.type == "tool"][0]
            self.assertEqual(
                tool_message.response_metadata["salvor"],
                {"live": True, "seq": 3, "run": run_id},
            )

            # The same question, the same tool result, a different stamp on the
            # model call after the tool call, and a model that answers something
            # else when it is actually asked.
            reset()
            forks = []  # type: List[Any]
            second, model = agent_stamped(
                [ONE_TOOL_SCRIPT[0], {"content": "ORD-7781 was refunded after all."}],
                "two",
                forks.append,
            )
            forked = await self.invoke(second, ASK, self.thread(thread_id))
            self.assertEqual(model.calls["count"], 1, "only the diverged call was live")
            self.assertEqual(ran["lookup"], 0, "the tool call before the fork replayed")
            self.assertEqual(
                self.text_of(forked["messages"][-1]), "ORD-7781 was refunded after all."
            )

            self.assertEqual(len(forks), 1, "told once, however many steps followed")
            self.assertEqual(forks[0].at, 5, "the second model call's position")
            self.assertEqual(forks[0].thread, thread_id)
            self.assertEqual(forks[0].run, run_id)
            self.assertIn(thread_id, forks[0].message, "the sentence names the thread")
            self.assertIn(run_id, forks[0].message, "and the run")
            self.assertIn("seq 5", forks[0].message, "and the position")
            self.assertIn("branches on the clock", forks[0].message, "and what to check")

            # Before the fork, replayed and saying so; from the fork on, forked
            # and saying where.
            answers = [m for m in forked["messages"] if m.type == "ai"]
            self.assertEqual(
                answers[0].response_metadata["salvor"],
                {"replayed": True, "seq": 1, "run": run_id},
            )
            self.assertEqual(
                [m for m in forked["messages"] if m.type == "tool"][0]
                .response_metadata["salvor"],
                {"replayed": True, "seq": 3, "run": run_id},
            )
            self.assertEqual(
                answers[-1].response_metadata["salvor"],
                {"forked": {"at": 5, "thread": thread_id, "run": run_id}},
            )

            # The fork was appended, not lost.
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[7:],
                ["ModelCallRequested", "ModelCallCompleted"],
            )

        self.drive(body)

    # -- (k2) a tool whose operator settles its calls by hand -----------------

    def test_a_tool_the_operator_settles_by_hand_stops_and_says_who_settles_it(
        self,
    ) -> None:
        """A write nobody may self-report stops for a person, and resumes for one.

        A client-tool declaration with `trust_completion = false` is an operator
        saying that the party performing this write does not get to decide it
        succeeded. Salvor refuses such a completion, so a middleware that posted
        one anyway would put a bare 403 in the middle of somebody's graph, after
        the money moved. It performs the call, records nothing about how it
        went, and says who settles it: the log ends at the intent, the result is
        on the error, and the next invoke replays what the person recorded.
        """
        thread_id = "thread-needs-resolution"
        run_id = run_id_for_thread(thread_id)
        script = [
            {
                "content": "sending the payout",
                "tool_calls": [
                    {
                        "name": "wire_payout",
                        "args": {"order_id": "ORD-77", "amount_cents": 4200},
                        "id": "call-wire",
                    }
                ],
            },
            {"content": "Payout wt-ORD-77 is confirmed."},
        ]
        ask = {"messages": [{"role": "user", "content": "pay ORD-77 out"}]}

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(script, client, tools=[wire_payout])
            with self.assertRaises(ToolNeedsResolution) as caught:
                await self.invoke(agent, ask, self.thread(thread_id))
            error = caught.exception

            self.assertEqual(ran["payout"], 1, "the tool did run: the work is done")
            self.assertEqual(error.tool, "wire_payout")
            self.assertEqual(error.run_id, run_id)
            self.assertEqual(error.thread_id, thread_id)
            self.assertEqual(error.seq, 3, "the seq the call's intent landed at")
            self.assertEqual(
                error.output,
                {
                    "provider_transfer_id": "wt-ORD-77",
                    "status": "succeeded",
                    "amount_cents": 4200,
                },
                "what the tool returned, for the person who settles it",
            )
            self.assertEqual(
                error.key,
                (await self.intents_of(client, thread_id))[0].payload["idempotency_key"],
                "the key the call was performed under, to look it up by",
            )
            text = str(error)
            self.assertIn("wire_payout", text, "the error names the tool")
            self.assertIn("trust_completion", text, "and the rule it broke")
            self.assertIn("salvor resolve {run}".format(run=run_id), text, "and how")

            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                ],
                "the log ends at the intent: a write nobody has confirmed yet",
            )

            # A person confirms what the payout did and records it, which is the
            # one way this run moves again.
            run = await call(client.open_client_run, run_id=run_id)
            await call(run.resolve, error.output)

            reset()
            again, model = self.agent_for(script, client, tools=[wire_payout])
            answer = await self.invoke(again, ask, self.thread(thread_id))
            self.assertEqual(ran["payout"], 0, "the payout did not happen twice")
            self.assertEqual(model.calls["count"], 1, "only the answer turn was live")
            settled = [m for m in answer["messages"] if m.type == "tool"][0]
            self.assertEqual(
                json.loads(self.text_of(settled)),
                error.output,
                "the model reads what the person recorded",
            )
            self.assertIs(settled.response_metadata["salvor"]["replayed"], True)
            self.assertEqual(
                self.text_of(answer["messages"][-1]), "Payout wt-ORD-77 is confirmed."
            )

        self.drive(body)

    # -- (l) another driver takes the run's lease mid-invoke ------------------

    def test_a_second_driver_mid_invoke_is_taken_back_once_and_then_refused(
        self,
    ) -> None:
        """A lease belongs to a process, and losing it is not losing the work.

        Another instance of the same app opening this thread's run takes the
        lease out from under this invoke, and salvor then refuses its next
        write. Once, that costs nothing: the run is taken up again (which
        returns the recorded state and a fresh lease), the log is read again,
        and the step is retried at the position it already reserved. Twice in
        one invoke is two drivers taking turns, which no retry settles, so it
        stops and says which rule was broken.
        """
        thread_id = "thread-two-drivers"
        run_id = run_id_for_thread(thread_id)
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

        def take_the_lease() -> None:
            httpx.post(
                "{base}/v1/client-runs".format(base=self.base),
                json={"run_id": run_id},
                timeout=10,
            )

        async def body(client: Any) -> None:
            meddling["do"] = take_the_lease
            agent, _ = self.agent_for(script, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(agent, ask, self.thread(thread_id))
            meddling["do"] = None

            text = str(caught.exception)
            self.assertIn(thread_id, text, "the error names the thread")
            self.assertIn(run_id, text, "and the run")
            self.assertIn("one driver per thread", text, "and the rule")

            self.assertEqual(ran["lookup"], 2, "each tool body ran once, not twice")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                    "ToolCallRequested",
                ],
                "the first call survived its lost lease; the second is where it stopped",
            )

        self.drive(body)

    # -- (m) the server this run was opened on restarts mid-invoke ------------

    def test_a_server_restart_mid_invoke_is_survived_by_the_reopen_once_path(
        self,
    ) -> None:
        """A restarted salvor hands a client-driven run back, and the invoke
        that was mid-flight when it restarted carries straight through.

        A server holds its client-driven leases in memory, but the log is on
        disk, and `RunStarted` carries `driven_by: client`, stamped once by the
        server and read back on every open. A restarted salvor no longer holds
        the lease, but it still reads its own store, recognises the run as
        client-driven from that marker, and hands it back with a fresh lease
        and the recorded log. That is exactly what the tape's re-open-once path
        (`_guarded`) already does for a lease another driver took, so a restart
        mid-invoke costs this invoke nothing beyond the one retried write: the
        tool runs exactly once, its intent and completion are each recorded
        exactly once, and the invoke finishes with the answer it would have
        given without the restart at all.
        """
        thread_id = "thread-server-restart"
        port = free_port()
        base = "http://127.0.0.1:{port}".format(port=port)
        workspace = tempfile.mkdtemp(prefix="salvor-py-")
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)
        store = str(Path(workspace) / "restart.db")
        serving = {"proc": serve(port, store)}
        self.addCleanup(lambda: stop(serving["proc"]))
        if not wait_until_up(base):
            raise unittest.SkipTest("salvor serve did not come up")

        def restart_the_server() -> None:
            meddling["do"] = None  # once, in the middle of the one tool call
            stop(serving["proc"])
            serving["proc"] = serve(port, store)
            wait_until_up(base)

        async def body(_class_client: Any) -> None:
            own = self.CLIENT(base)
            try:
                meddling["do"] = restart_the_server
                agent, _ = self.agent_for(ONE_TOOL_SCRIPT, own)
                answer = await self.invoke(agent, ASK, self.thread(thread_id))
                self.assertEqual(
                    self.text_of(answer["messages"][-1]),
                    "Order ORD-7781 is paid, 4200 cents.",
                    "the invoke completed across the restart",
                )
                self.assertEqual(ran["lookup"], 1, "the tool body ran exactly once")
                kinds = await self.kinds_of(own, thread_id)
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
                    "one intent and one completion for the call the restart landed inside",
                )
                self.assertEqual(kinds.count("ToolCallRequested"), 1)
                self.assertEqual(kinds.count("ToolCallCompleted"), 1)

                # A further invoke of the same thread replays entirely: the
                # restart is behind it, and nothing about it is paid for twice.
                reset()
                again_agent, again_model = self.agent_for(ONE_TOOL_SCRIPT, own)
                again = await self.invoke(again_agent, ASK, self.thread(thread_id))
                self.assertEqual(again_model.calls["count"], 0, "zero model calls on replay")
                self.assertEqual(ran["lookup"], 0, "zero tool executions on replay")
                self.assertEqual(
                    self.text_of(again["messages"][-1]),
                    "Order ORD-7781 is paid, 4200 cents.",
                    "the same final message",
                )
            finally:
                meddling["do"] = None
                await call(own.close)

        self.drive(body)

    # -- (n) a run id that belongs to salvor's other mode ----------------------

    def test_a_server_driven_run_ids_open_is_refused_naming_the_thread_and_the_reason(
        self,
    ) -> None:
        """A run started in salvor's other mode does not become client-driven
        just because a thread's id happens to map to it.

        `open_client_run` recognises a client-driven run two ways: this
        process's own lease registry, or `driven_by: client` on the recorded
        `RunStarted`, which is the marker that lets the restart above be
        survived. A run this test starts with `start_run` carries neither, so
        the very first open the middleware makes for a thread mapped to its id
        is refused (`run_exists`), before any model call or tool call happens.
        This is the refusal `cannot_reopen` exists to give a name to when it
        turns up on a re-open mid-invoke instead of on the first one; nothing
        else in this suite still reaches a salvor that refuses to hand a run
        back at all, now that a restart does not. A UUID thread id is used
        unchanged as the run id (`run_id_for_thread`), so the refusal, which
        can only name the run, names the thread too.
        """

        async def body(client: Any) -> None:
            agent_hash = await call(
                client.register_agent,
                {"model": "vector-model", "system_prompt": "a plain assistant"},
            )
            thread_id = str(uuid.uuid4())
            await call(client.start_run, agent_hash, {"q": "hi"}, run_id=thread_id)

            agent, model = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorAPIError) as caught:
                await self.invoke(agent, ASK, self.thread(thread_id))
            text = str(caught.exception)
            self.assertIn(
                thread_id, text, "the error names the thread (its run id, unchanged)"
            )
            self.assertIn(
                "server-driven run",
                text,
                "and the reason: it belongs to salvor's other mode",
            )
            self.assertEqual(model.calls["count"], 0, "refused before any model call")
            self.assertEqual(ran["lookup"], 0, "refused before any tool call")

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
    #:
    #: The tool result here is deliberately prose rather than JSON. A tool
    #: message whose content parses as JSON is hashed as the value it holds
    #: rather than as one spelling of that value (see `_content` in
    #: `salvor/langchain/request.py`), which is what stops a tool's own key
    #: order forking a thread. Pinning a JSON result here would pin the
    #: canonicalization of the parsed value instead, and the vector's job is to
    #: pin the one thing the two SDKs must never disagree about: the bytes the
    #: canonical writer produces. The TypeScript suite pins the same request.
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
                "content": "ORD-7781 is paid, 4200 cents.",
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
        '{"content":"ORD-7781 is paid, 4200 cents.","name":"lookup_order",'
        '"role":"tool","tool_call_id":"call-1"}],"model":{"model":"vector-model",'
        '"temperature":0,"type":"scripted-fake"},"model_settings":{},'
        '"system":"You are a careful order assistant."}'
    )
    REQUEST_HASH = "sha256:3eb94c7b6f6bd64a0fbccce14ccd0eddba3fa3efb7ab72882e59f96635735178"

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
                    content="ORD-7781 is paid, 4200 cents.",
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
