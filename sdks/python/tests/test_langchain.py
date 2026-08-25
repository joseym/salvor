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

The model is a small ``BaseChatModel`` scripted turn by turn rather than one of
the fakes in ``langchain_core.language_models.fake_chat_models``. Those cannot
script a multi-turn tool-calling agent, and a fake whose ``bind_tools`` rebuilds
itself drops any counter attached to it, which is precisely the thing these
cases have to count. Both facts are checked by this file's own script: no key,
no network, one counter that survives binding.

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
import json
import socket
import subprocess
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
    from langchain_core.tools import tool
except ImportError:  # pragma: no cover - depends on what is installed
    raise unittest.SkipTest(
        "LangChain is not installed; install the extra to run these "
        "(pip install -e 'sdks/python[langchain]')"
    ) from None

from salvor import AsyncClient
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

#: How often each tool body actually ran, and how many ran at once.
ran = {"lookup": 0, "stamp": 0, "concurrent": 0, "peak_concurrent": 0}
#: Set to make the next `stamp_ledger` body raise, standing in for a crash.
stamp_crashes = {"on": False}
#: What `current_tool_call()` reported the last time `lookup_order`'s body ran.
captured = {"call": None}  # type: Dict[str, Any]


def enter() -> None:
    ran["concurrent"] += 1
    ran["peak_concurrent"] = max(ran["peak_concurrent"], ran["concurrent"])


@tool
async def lookup_order(order_id: str) -> Dict[str, Any]:
    """Look up an order that has already been placed."""
    enter()
    try:
        captured["call"] = current_tool_call()
        await asyncio.sleep(0.015)
        ran["lookup"] += 1
        return {"order_id": order_id, "status": "paid", "total_cents": 4200}
    finally:
        ran["concurrent"] -= 1


@tool
async def stamp_ledger(order_id: str, note: str) -> Dict[str, Any]:
    """Write one line into the order's ledger."""
    enter()
    try:
        ran["stamp"] += 1
        if stamp_crashes["on"]:
            raise RuntimeError("the ledger writer died mid-call")
        return {"order_id": order_id, "entry_id": "entry-{n}".format(n=len(note))}
    finally:
        ran["concurrent"] -= 1


@tool
async def send_email(to: str) -> Dict[str, Any]:
    """Send an email. Deliberately never declared to salvor."""
    return {"sent": True}


def reset() -> None:
    ran.update({"lookup": 0, "stamp": 0, "concurrent": 0, "peak_concurrent": 0})
    stamp_crashes["on"] = False
    captured["call"] = None


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


class MiddlewareRealServer(unittest.TestCase):
    """The whole middleware surface against the real control-plane binary."""

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

    def run_async(self, body: Any) -> Any:
        """Run one coroutine factory with a client that is closed either way."""

        async def wrapped() -> Any:
            async with AsyncClient(self.base) as client:
                return await body(client)

        return asyncio.run(wrapped())

    def agent_for(self, turns: List[Dict[str, Any]], client: AsyncClient, tools: Any = None):
        model = ScriptedModel(turns=turns, calls={"count": 0})
        agent = create_agent(
            model=model,
            tools=list(tools if tools is not None else [lookup_order, stamp_ledger]),
            middleware=[SalvorMiddleware(client)],
        )
        return agent, model

    async def kinds_of(self, client: AsyncClient, thread_id: str) -> List[str]:
        run = await client.open_client_run(run_id=run_id_for_thread(thread_id))
        return [event.kind for event in run.log_envelopes]

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

        async def body(client: AsyncClient) -> None:
            # (a) The first invoke pays for everything, and the log says so.
            agent, model = self.agent_for(ONE_TOOL_SCRIPT, client)
            answer = await agent.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
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
            again = await again_agent.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
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

        self.run_async(body)

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

        async def body(client: AsyncClient) -> None:
            # The tool dies after salvor recorded the intent and before anything
            # could report a result, which is the shape of every real mid-write
            # crash.
            stamp_crashes["on"] = True
            crashed, _ = self.agent_for(script, client)
            with self.assertRaises(RuntimeError) as caught:
                await crashed.ainvoke(
                    {"messages": [{"role": "user", "content": "stamp ORD-9001"}]},
                    self.thread(thread_id),
                )
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
            answer = await recovered.ainvoke(
                {"messages": [{"role": "user", "content": "stamp ORD-9001"}]},
                self.thread(thread_id),
            )
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

        self.run_async(body)

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

        async def body(client: AsyncClient) -> None:
            agent, model = self.agent_for(script, client)
            answer = await agent.ainvoke(
                {"messages": [{"role": "user", "content": "check ORD-1 and ORD-2"}]},
                self.thread(thread_id),
            )
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
            run = await client.open_client_run(run_id=run_id_for_thread(thread_id))
            inputs = [
                event.payload["input"]["order_id"]
                for event in run.log_envelopes
                if event.kind == "ToolCallRequested"
            ]
            self.assertEqual(inputs, ["ORD-1", "ORD-2"])

            # And a replay of the whole turn touches neither model nor tools.
            reset()
            second, second_model = self.agent_for(script, client)
            await second.ainvoke(
                {"messages": [{"role": "user", "content": "check ORD-1 and ORD-2"}]},
                self.thread(thread_id),
            )
            self.assertEqual(second_model.calls["count"], 0)
            self.assertEqual(ran["lookup"], 0)

        self.run_async(body)

    def test_a_turn_records_its_tool_calls_in_the_model_order_every_time(self) -> None:
        """The ordering the turnstile exists for, run enough times to catch the
        loop scheduling the turn differently.

        In Python the hooks do not reach the middleware in the model's order:
        the same three-tool turn arrived in three different orders across five
        runs of a bare probe middleware. So the recorded order is taken from the
        AI message rather than from arrival, and this is the case that says so.
        """
        script = [
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

        async def body(client: AsyncClient) -> None:
            for attempt in range(5):
                reset()
                thread_id = "thread-order-{n}".format(n=attempt)
                agent, _ = self.agent_for(script, client)
                await agent.ainvoke(
                    {"messages": [{"role": "user", "content": "check all three"}]},
                    self.thread(thread_id),
                )
                self.assertEqual(ran["lookup"], 3)
                self.assertEqual(ran["peak_concurrent"], 1, "one at a time")
                run = await client.open_client_run(run_id=run_id_for_thread(thread_id))
                inputs = [
                    event.payload["input"]["order_id"]
                    for event in run.log_envelopes
                    if event.kind == "ToolCallRequested"
                ]
                self.assertEqual(inputs, ["ORD-A", "ORD-B", "ORD-C"])

        self.run_async(body)

    # -- (e) a replayed answer under streaming --------------------------------

    def test_a_replayed_answer_streams_as_one_whole_chunk_marked_replayed(self) -> None:
        thread_id = "thread-streaming-replay"
        message_in = {"messages": [{"role": "user", "content": "how is ORD-7781?"}]}

        async def body(client: AsyncClient) -> None:
            first, first_model = self.agent_for(ONE_TOOL_SCRIPT, client)
            await first.ainvoke(message_in, self.thread(thread_id))
            self.assertEqual(first_model.calls["count"], 2)

            reset()
            second, second_model = self.agent_for(ONE_TOOL_SCRIPT, client)
            chunks = []  # type: List[AIMessage]
            async for message, _metadata in second.astream(
                message_in, self.thread(thread_id), stream_mode="messages"
            ):
                if message.type == "ai":
                    chunks.append(message)

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

        self.run_async(body)

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

        async def body(client: AsyncClient) -> None:
            agent, _ = self.agent_for(
                script, client, tools=[lookup_order, stamp_ledger, send_email]
            )
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await agent.ainvoke(
                    {"messages": [{"role": "user", "content": "email ops"}]},
                    self.thread("thread-undeclared-tool"),
                )
            text = str(caught.exception)
            self.assertIn("send_email", text, "the error names the tool")
            self.assertIn("client-tool declaration", text, "and the declaration it needs")
            self.assertIn("--client-tool", text, "and how to load it")

        self.run_async(body)

    # -- leaving the recorded path --------------------------------------------

    def test_an_invoke_off_the_recorded_path_appends_instead_of_replaying(self) -> None:
        thread_id = "thread-second-question"

        async def body(client: AsyncClient) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await first.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
            self.assertEqual(len(await self.kinds_of(client, thread_id)), 7)

            # A different question down the same thread is a different first
            # model call, so nothing at the recorded positions applies. The run
            # carries on at the end of its log rather than pretending the old
            # answers are still answers.
            reset()
            second, model = self.agent_for(
                [{"content": "ORD-9999 is not one of ours."}], client
            )
            answer = await second.ainvoke(
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

        self.run_async(body)

    # -- (g) finish_thread closes a thread's run ------------------------------

    def test_finish_thread_completes_the_run_and_a_further_invoke_is_refused(self) -> None:
        thread_id = "thread-finish"

        async def body(client: AsyncClient) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            answer = await first.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
            final = self.text_of(answer["messages"][-1])

            run_id = run_id_for_thread(thread_id)
            finished = await finish_thread(client, thread_id)
            self.assertEqual(finished.run_id, run_id)
            self.assertEqual((await self.kinds_of(client, thread_id))[-1], "RunCompleted")

            state = await client.get_run(run_id)
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
                await second.ainvoke(
                    {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                    self.thread(thread_id),
                )
            text = str(caught.exception)
            self.assertIn("thread-finish", text, "the error names the thread")
            self.assertIn("finish", text.lower(), "and says it is finished")

        self.run_async(body)

    # -- (h) current_tool_call() inside a tool body ---------------------------

    def test_a_tool_body_reads_the_key_the_intent_recorded_on_both_invokes(self) -> None:
        thread_id = "thread-current-tool-call"

        async def body(client: AsyncClient) -> None:
            first, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await first.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
            call = captured["call"]
            self.assertIsNotNone(call, "the tool body read a current call")
            self.assertEqual(call.tool, "lookup_order")
            self.assertEqual(call.run_id, run_id_for_thread(thread_id))

            run = await client.open_client_run(run_id=run_id_for_thread(thread_id))
            intent = next(
                event for event in run.log_envelopes if event.kind == "ToolCallRequested"
            )
            self.assertEqual(call.seq, intent.seq, "the seq matches the recorded intent")
            self.assertEqual(
                call.key,
                intent.payload["idempotency_key"],
                "the key is the one salvor recorded on the intent",
            )

            # A replayed invoke never runs the tool body, so nothing new is
            # captured, but the log's own recorded key is unchanged.
            reset()
            second, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await second.ainvoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread(thread_id),
            )
            self.assertIsNone(captured["call"], "the replay never ran the tool body")
            self.assertEqual(ran["lookup"], 0)

            replayed = await client.open_client_run(run_id=run_id_for_thread(thread_id))
            replayed_intent = next(
                event for event in replayed.log_envelopes if event.kind == "ToolCallRequested"
            )
            self.assertEqual(
                replayed_intent.payload["idempotency_key"],
                intent.payload["idempotency_key"],
                "the recorded key is identical on replay",
            )

        self.run_async(body)

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

        async def body(client: AsyncClient) -> None:
            stamp_crashes["on"] = True
            crashed, _ = self.agent_for(script, client)
            with self.assertRaises(RuntimeError):
                await crashed.ainvoke(
                    {"messages": [{"role": "user", "content": "stamp ORD-4242"}]},
                    self.thread(thread_id),
                )
            stamp_crashes["on"] = False

            run_id = run_id_for_thread(thread_id)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await finish_thread(client, thread_id)
            text = str(caught.exception)
            self.assertIn(run_id, text, "the error names the run")
            self.assertIn("never completed", text, "and says the call was never completed")

            # Nothing was appended: the log still ends at the same open intent.
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[-1],
                "ToolCallRequested",
                "finish_thread wrote nothing",
            )

        self.run_async(body)

    # -- a thread id is required ----------------------------------------------

    def test_an_invoke_with_no_thread_id_is_refused(self) -> None:
        async def body(client: AsyncClient) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await agent.ainvoke(
                    {"messages": [{"role": "user", "content": "how is ORD-7781?"}]}
                )
            self.assertIn("thread id", str(caught.exception))

        self.run_async(body)

    # -- the synchronous path says what it needs -------------------------------

    def test_a_synchronous_invoke_says_to_use_ainvoke(self) -> None:
        async def body(client: AsyncClient) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            return agent

        agent = self.run_async(body)
        with self.assertRaises(SalvorMiddlewareError) as caught:
            agent.invoke(
                {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
                self.thread("thread-sync-refusal"),
            )
        self.assertIn("ainvoke", str(caught.exception))


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
