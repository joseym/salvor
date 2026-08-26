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

from salvor import AsyncClient, Client, Event, SalvorAPIError
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
    salvor_error,
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
        if model_raises_once["on"]:
            model_raises_once["on"] = False
            raise RuntimeError("the model provider died mid-call")
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
#: Set to make the next `stamp_ledger` body leave by an interrupt rather than
#: fail: the sync body raises `KeyboardInterrupt` directly, and the async body
#: hangs on `"event"` (an `asyncio.Event` the test supplies) until the test
#: cancels its task, standing in for `asyncio.CancelledError`. Neither is an
#: `Exception`, which is the whole point: the middleware's failure-reporting
#: catch must not treat a process leaving as a call failing.
stamp_interrupts = {"on": False, "event": None}  # type: Dict[str, Any]
#: Set to make the next `wire_payout` body raise. `wire_payout` is declared
#: `trust_completion = false`, so this is how the suite proves the OTHER
#: outcome a raise can have: nothing posted, the intent left open for a
#: person, rather than a recorded failure.
payout_crashes = {"on": False}
#: Set to make the next `stamp_ledger` body report an output the server
#: refuses to record: `"schema"` drops the required `entry_id` (`bad_request`
#: against the declared `output_schema`), `"mismatch"` reports a different
#: `order_id` than the intent recorded (`client_completion_refused` against
#: the declaration's `require_equal`). `None` is the ordinary, accepted output.
stamp_bad_output = {"kind": None}  # type: Dict[str, Optional[str]]
#: Set to make the very next scripted model call raise, standing in for a
#: provider dying after its intent was already posted but before it answered.
model_raises_once = {"on": False}
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


#: How long a `lookup_order` body should take, and what to do halfway through
#: it. The short-lease case sets both: a body that outlives the whole lease TTL,
#: with a rival's attempt to take the run landing after the lease would have
#: lapsed if nothing were beating. Every other case leaves it at zero and the
#: bodies are as quick as they ever were.
dawdle = {"seconds": 0.0, "midway": None}  # type: Dict[str, Any]


def meddle() -> None:
    """Do whatever this case wants done between a tool's intent and its
    completion, which is where a lost lease actually hurts."""
    interference = meddling["do"]
    if interference is not None:
        interference()


def dawdle_halves() -> float:
    """Half of however long this case wants the tool body to take."""
    return float(dawdle["seconds"]) / 2.0


def dawdle_midway() -> None:
    """What happens at the halfway mark of a deliberately long body."""
    if dawdle["midway"] is not None:
        dawdle["midway"]()


def lookup_body(order_id: str) -> Dict[str, Any]:
    """Look up an order that has already been placed."""
    enter()
    try:
        capture_call(order_id)
        meddle()
        half = dawdle_halves()
        if half:
            time.sleep(half)
            dawdle_midway()
            time.sleep(half)
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
        half = dawdle_halves()
        if half:
            # Awaited, not slept through: a body that blocked the loop for
            # longer than the lease would block the heartbeat task with it, and
            # the point of this case is the beating, not the blocking.
            await asyncio.sleep(half)
            dawdle_midway()
            await asyncio.sleep(half)
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
        if stamp_interrupts["on"]:
            raise KeyboardInterrupt("ctrl-c mid-write")
        kind = stamp_bad_output["kind"]
        if kind == "schema":
            # No `entry_id`: fails the declaration's `output_schema`.
            return {"order_id": order_id}
        if kind == "mismatch":
            # A different `order_id` than the intent recorded: fails the
            # declaration's `require_equal`.
            return {
                "order_id": "ORD-NOT-THE-ONE-CALLED",
                "entry_id": "entry-{n}".format(n=len(note)),
            }
        return {"order_id": order_id, "entry_id": "entry-{n}".format(n=len(note))}
    finally:
        leave()


async def astamp_body(order_id: str, note: str) -> Dict[str, Any]:
    if stamp_interrupts["on"]:
        # Standing in for a task cancellation mid-write: signal that the body
        # has actually started, then hang at an await point until the test
        # cancels this task. Nothing here catches the `CancelledError` that
        # delivers.
        count("stamp")
        stamp_interrupts["event"].set()
        await asyncio.sleep(10)
    return stamp_body(order_id, note)


def payout_body(order_id: str, amount_cents: int) -> Dict[str, Any]:
    """Wire a payout for an order whose card refund is not available."""
    count("payout")
    if payout_crashes["on"]:
        raise RuntimeError("the payout provider died mid-call")
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
    stamp_interrupts["on"] = False
    stamp_interrupts["event"] = None
    payout_crashes["on"] = False
    stamp_bad_output["kind"] = None
    model_raises_once["on"] = False
    meddling["do"] = None
    dawdle["seconds"] = 0.0
    dawdle["midway"] = None
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


def serve(port: int, store: str, environment: Optional[Dict[str, str]] = None) -> subprocess.Popen:
    """One `salvor serve` on ``port`` over ``store``, with this suite's
    client-tool declarations loaded.

    ``environment`` adds to the server's environment, which is how the
    short-lease case asks for a `SALVOR_CLIENT_LEASE_TTL_SECS` a test can
    actually outlive.
    """
    declarations = []  # type: List[str]
    for path in DECLS:
        declarations += ["--client-tool", str(path)]
    env = {"PATH": "/usr/bin:/bin"}
    env.update(environment or {})
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
        env=env,
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


def _read_log(store: str, thread_id: str) -> List[Event]:
    """Read a thread's recorded log for pure inspection, off the store itself.

    A test that wants to see what a run recorded is not driving it, so this
    never opens the run: opening asks for (or presents) the lease, and a case
    that took one would be interfering with the very thing it is checking. It
    reads the store instead of ``GET /v1/client-runs/{id}/log``, and off the
    same file the server writes: what these cases assert is what was durably
    recorded, not what a live server chose to serve back, and a read off the
    store holds whether the server is up, down, or holding the run. `salvor
    history --json` prints exactly the envelope wire shape the endpoint
    returns, so nothing about the assertions changes. The TypeScript suite's
    own log helper reads the same way.
    """
    printed = subprocess.run(
        [str(SALVOR), "history", run_id_for_thread(thread_id), "--store", store, "--json"],
        capture_output=True,
        text=True,
        check=True,
        env={"PATH": "/usr/bin:/bin"},
    )
    return [Event.from_envelope(envelope) for envelope in json.loads(printed.stdout)]


def _grouped(errors: List[BaseException]) -> Any:
    """One exception group over ``errors``, or ``None`` on a Python without
    them (3.11 added the type; this suite still runs on 3.9)."""
    maker = getattr(__import__("builtins"), "ExceptionGroup", None)
    if maker is None:  # pragma: no cover - depends on the interpreter
        return None
    return maker("several things failed", errors)


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
    store: str
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
        #: The store this class's server writes, which is also where a test
        #: reads a run's log back from (see :func:`_read_log`).
        cls.store = str(Path(cls.workspace) / "langchain.db")
        cls.proc = serve(port, cls.store)
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
        #: The middleware `agent_for` most recently built, for a scenario that
        #: needs its still-open tape back. See `agent_for`.
        self.last_middleware = None  # type: Optional[SalvorMiddleware]

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
        middleware = SalvorMiddleware(client, on_fork=on_fork)
        # Stashed for a scenario that needs the SAME driver back after a
        # `ToolNeedsResolution`: the middleware's tape (and its currently
        # held lease) survives an invoke that raised, because `after_agent`
        # never runs to pop it (see `before_agent`'s own docstring). Re-using
        # it, rather than opening the run again, is exactly what the error's
        # own `driver.resolve(output)` suggestion means: the held lease is
        # still current, so a bare re-open from anywhere else would now be
        # refused `lease_held`.
        self.last_middleware = middleware
        agent = create_agent(
            model=model,
            tools=list(tools if tools is not None else [lookup_order, stamp_ledger]),
            middleware=[middleware],
        )
        return agent, model

    async def kinds_of(
        self, client: Any, thread_id: str, store: Optional[str] = None
    ) -> List[str]:
        """Every event kind this thread's run recorded, in order.

        Read off the store rather than over HTTP (see :func:`_read_log`).
        ``store`` names a different one for the two cases that run a server of
        their own; every other case reads the class's.
        """
        return [event.kind for event in _read_log(store or self.store, thread_id)]

    async def intents_of(self, client: Any, thread_id: str) -> List[Any]:
        return [
            event
            for event in _read_log(self.store, thread_id)
            if event.kind == "ToolCallRequested"
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

    # -- (c) a tool body raises between its intent and a completion -----------

    def test_a_raised_tool_body_is_recorded_as_a_failure_and_fails_the_same_way_again(
        self,
    ) -> None:
        """A trusted tool's raise is not left dangling: it is the call's
        failure, recorded as a completion the same way a returned value is.

        The first invoke runs the body, which raises; the middleware reports
        that raise to salvor as the call's failure and re-raises the original
        error, and the lease comes back with the step that raised, same as any
        other raising step. A second invoke meets the recorded failure at the
        same position and raises naming it, without running the body again:
        the call is settled, not retried.
        """
        thread_id = "thread-crash-mid-write"
        run_id = run_id_for_thread(thread_id)
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
                    "ToolCallCompleted",
                ],
                "the raise is recorded as the call's completion, not left dangling",
            )
            stamp_crashes["on"] = False

            # The lease came back with the failed step, so a stranger opens the
            # run at once rather than meeting `lease_held`.
            stranger = self.CLIENT(self.base)
            try:
                taken = await call(stranger.open_client_run, run_id=run_id)
                self.assertEqual(taken.run_id, run_id, "the run was free at once")
                await call(taken.release)
            finally:
                await call(stranger.close)

            # The next invoke replays the model turn for free, meets the
            # recorded failure at the tool's position, and raises naming it
            # without running the body a second time.
            again, model = self.agent_for(script, client)
            with self.assertRaises(SalvorMiddlewareError) as caught2:
                await self.invoke(again, ask, self.thread(thread_id))
            refusal = caught2.exception
            self.assertEqual(refusal.code, "tool_failed")
            self.assertIn("ledger writer died mid-call", str(refusal))
            self.assertIn(run_id, str(refusal), "the error names the run")
            self.assertEqual(ran["stamp"], 1, "the body did not run a second time")
            self.assertEqual(
                model.calls["count"], 0, "the second model turn never ran either"
            )
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                    "ToolCallCompleted",
                ],
                "nothing new was appended: the failure is settled, not retried",
            )

        self.drive(body)

    # -- (c2) a provider error between a model call's intent and its completion --

    def test_a_provider_error_mid_model_call_leaves_the_intent_open_for_the_next_invoke(
        self,
    ) -> None:
        """The model half of the crash-mid-write case above.

        The intent is posted, the provider raises before answering, and
        nothing is recorded for that attempt: the log ends at the intent, the
        error is the application's own (`salvor_error` finds nothing of
        salvor's in it), and the lease goes back the same way a raising tool
        body's does. The next invoke meets the same open intent, posts it
        again, performs the call once more, and this time records the
        completion.
        """
        thread_id = "thread-provider-error-mid-model-call"
        run_id = run_id_for_thread(thread_id)
        script = [{"content": "the answer, once the provider cooperates"}]
        ask = {"messages": [{"role": "user", "content": "hello"}]}

        async def body(client: Any) -> None:
            model_raises_once["on"] = True
            first, first_model = self.agent_for(script, client)
            with self.assertRaises(RuntimeError) as caught:
                await self.invoke(first, ask, self.thread(thread_id))
            self.assertIn("provider died mid-call", str(caught.exception))
            self.assertIsNone(
                salvor_error(caught.exception),
                "a provider's own error is not salvor's to report",
            )
            self.assertEqual(first_model.calls["count"], 1, "the one call that raised")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                ["RunStarted", "ModelCallRequested"],
                "the intent went in; nothing was recorded for the failed attempt",
            )

            # The lease came back with the failed step, so a stranger opens the
            # run at once rather than meeting `lease_held`.
            stranger = self.CLIENT(self.base)
            try:
                taken = await call(stranger.open_client_run, run_id=run_id)
                self.assertEqual(taken.run_id, run_id, "the run was free at once")
                await call(taken.release)
            finally:
                await call(stranger.close)

            # The next invoke meets the same open intent, posts it again, and
            # this time the provider answers: one call, one completion.
            second, second_model = self.agent_for(script, client)
            answer = await self.invoke(second, ask, self.thread(thread_id))
            self.assertEqual(
                second_model.calls["count"], 1, "the model was called once more"
            )
            self.assertEqual(
                self.text_of(answer["messages"][-1]),
                "the answer, once the provider cooperates",
            )
            kinds = await self.kinds_of(client, thread_id)
            self.assertEqual(kinds, ["RunStarted", "ModelCallRequested", "ModelCallCompleted"])
            self.assertEqual(kinds.count("ModelCallRequested"), 1, "exactly one intent")
            self.assertEqual(kinds.count("ModelCallCompleted"), 1, "exactly one completion")

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
            refusal = caught.exception
            self.assertEqual(refusal.code, "tool_undeclared")
            self.assertIsInstance(
                refusal.cause,
                SalvorAPIError,
                "the server's `unknown_tool` is underneath it",
            )
            self.assertEqual(refusal.cause.code, "unknown_tool")
            self.assertEqual(
                refusal.cause.message,
                "no client-performed tool named `send_email` is declared on "
                "this server; declarations are loaded by the operator (`salvor "
                "serve --client-tool <FILE>`) and are never registered over "
                "HTTP",
                "the sentence the README quotes, verbatim",
            )
            text = str(refusal)
            self.assertIn("send_email", text, "the error names the tool")
            self.assertIn("client-tool declaration", text, "and the declaration it needs")
            self.assertIn("--client-tool", text, "and how to load it")

        self.drive(body)

    # -- (f2) a server refusal escaping a hook is wrapped, not left bare ------

    def _stamp_script(self, order_id: str) -> Any:
        return [
            {
                "content": "stamping the ledger",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": order_id, "note": "seen"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "Stamped {order}.".format(order=order_id)},
        ]

    def test_an_output_schema_violation_surfaces_through_salvor_error_named_bad_request(
        self,
    ) -> None:
        """A tool's own reported output can fail the operator's schema, and
        salvor refuses to record it: `400 bad_request`. Left alone that
        refusal would tear through the graph as a bare `SalvorAPIError`
        naming no thread at all; the middleware wraps it like every other
        escaping refusal, keeping the server's own code and sentence.
        """
        thread_id = "thread-bad-output-schema"
        order_id = "ORD-9301"

        async def body(client: Any) -> None:
            stamp_bad_output["kind"] = "schema"
            agent, _ = self.agent_for(self._stamp_script(order_id), client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "stamp it"}]},
                    self.thread(thread_id),
                )
            refusal = caught.exception
            self.assertIs(salvor_error(caught.exception), refusal, "it arrived bare")
            self.assertEqual(refusal.code, "bad_request")
            self.assertIsInstance(
                refusal.cause, SalvorAPIError, "the server's own refusal is underneath it"
            )
            self.assertEqual(refusal.cause.code, "bad_request")
            self.assertIn(thread_id, str(refusal), "the error names the thread")
            self.assertIn(
                refusal.cause.message, str(refusal), "and keeps the server's own sentence"
            )
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[-1],
                "ToolCallRequested",
                "the refused completion recorded nothing",
            )

        self.drive(body)

    def test_a_require_equal_mismatch_surfaces_through_salvor_error_named_client_completion_refused(
        self,
    ) -> None:
        """A tool that reports a `require_equal` field differently from what
        its intent recorded is refused the same way, `403
        client_completion_refused`, and the middleware wraps it identically.
        """
        thread_id = "thread-require-equal-mismatch"
        order_id = "ORD-9302"

        async def body(client: Any) -> None:
            stamp_bad_output["kind"] = "mismatch"
            agent, _ = self.agent_for(self._stamp_script(order_id), client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "stamp it"}]},
                    self.thread(thread_id),
                )
            refusal = caught.exception
            self.assertIs(salvor_error(caught.exception), refusal, "it arrived bare")
            self.assertEqual(refusal.code, "client_completion_refused")
            self.assertIsInstance(
                refusal.cause, SalvorAPIError, "the server's own refusal is underneath it"
            )
            self.assertEqual(refusal.cause.code, "client_completion_refused")
            self.assertIn(thread_id, str(refusal), "the error names the thread")
            self.assertIn(
                refusal.cause.message, str(refusal), "and keeps the server's own sentence"
            )
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[-1],
                "ToolCallRequested",
                "the refused completion recorded nothing",
            )

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
            self.assertEqual(error.code, "tool_needs_resolution")
            text = str(error)
            self.assertIn("wire_payout", text, "the error names the tool")
            self.assertIn("trust_completion", text, "and the rule it broke")
            self.assertIn(
                "POST /v1/runs/{run}/resolve".format(run=run_id),
                text,
                "and the endpoint that settles it on a live server",
            )
            self.assertIn(
                "salvor resolve {run}".format(run=run_id),
                text,
                "and the command that settles it off the store",
            )

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

            # A person confirms what the payout did and records it, which is
            # the one way this run moves again. `after_agent` never ran (the
            # invoke raised), but the step that raised handed the lease back on
            # its way out, so this is an ordinary operator resolve over HTTP
            # from a caller holding no drive token at all: `POST
            # /v1/runs/{id}/resolve`, which is also what the error's own text
            # names first.
            await call(client.resolve, run_id, error.output)

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

    # -- (k3) a re-invoke meets that same untrusted call, still unresolved ----

    def test_a_reinvoke_before_resolving_an_untrusted_tool_is_refused_not_rerun(
        self,
    ) -> None:
        """A re-invoke that meets its own untrusted tool's dangling intent,
        still unresolved, never runs the tool body a second time.

        `trust_completion = false` exists so the party that performed a write
        does not get to decide it succeeded; retrying the call itself on a
        later invoke, before a person has settled the first one, would be
        exactly that. So this is refused outright: the same "never completed"
        refusal a mismatched replay raises (see `Tape._slot`), because that is
        what an unresolved intent is either way. The tool is touched nowhere
        in this: not to decide the refusal, and not after it.
        """
        thread_id = "thread-untrusted-reinvoke"
        run_id = run_id_for_thread(thread_id)
        script = [
            {
                "content": "sending the payout",
                "tool_calls": [
                    {
                        "name": "wire_payout",
                        "args": {"order_id": "ORD-88", "amount_cents": 1500},
                        "id": "call-wire",
                    }
                ],
            },
            {"content": "Payout wt-ORD-88 is confirmed."},
        ]
        ask = {"messages": [{"role": "user", "content": "pay ORD-88 out"}]}

        async def body(client: Any) -> None:
            # (1) The first invoke stops for a person, typed, having run the
            # call exactly once.
            agent, _ = self.agent_for(script, client, tools=[wire_payout])
            with self.assertRaises(ToolNeedsResolution) as caught:
                await self.invoke(agent, ask, self.thread(thread_id))
            error = caught.exception
            self.assertEqual(ran["payout"], 1, "the call ran once")

            # (2) A re-invoke before anyone resolves it meets that SAME open
            # intent and is refused, without running the tool a second time.
            again, _ = self.agent_for(script, client, tools=[wire_payout])
            with self.assertRaises(SalvorMiddlewareError) as caught2:
                await self.invoke(again, ask, self.thread(thread_id))
            self.assertEqual(caught2.exception.code, "open_intent")
            text = str(caught2.exception)
            self.assertIn("wire_payout", text, "the error names the tool")
            self.assertIn("trust_completion", text, "and the rule it broke")
            self.assertIn(run_id, text, "and the run")
            self.assertIn(thread_id, text, "and the thread")
            self.assertEqual(
                ran["payout"], 1, "the tool body did not run a second time"
            )
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                ],
                "still just the one dangling intent: the refusal wrote nothing",
            )

            # (3) A person confirms what the call did and records it, over the
            # same operator endpoint as case (k2): both invokes gave the lease
            # back as they died, so nothing is holding the run.
            await call(client.resolve, run_id, error.output)

            # (4) Now a further invoke replays the resolved completion, and
            # the tool runs no further.
            reset()
            final, model = self.agent_for(script, client, tools=[wire_payout])
            answer = await self.invoke(final, ask, self.thread(thread_id))
            self.assertEqual(ran["payout"], 0, "zero executions: this is a replay")
            self.assertEqual(model.calls["count"], 1, "only the answer turn was live")
            settled = [m for m in answer["messages"] if m.type == "tool"][0]
            self.assertEqual(json.loads(self.text_of(settled)), error.output)
            self.assertIs(settled.response_metadata["salvor"]["replayed"], True)
            self.assertEqual(
                self.text_of(answer["messages"][-1]), "Payout wt-ORD-88 is confirmed."
            )

        self.drive(body)

    # -- (l) another instance holds the run's lease mid-invoke ------------------

    def test_a_second_instance_on_a_held_thread_is_refused_before_running_anything(
        self,
    ) -> None:
        """The rule is not "newest caller wins": a lease is held until it
        lapses.

        A second instance of the same app invoking a thread the first is
        still driving cannot take the lease out from under it any more: its
        own open is refused outright, `lease_held`, naming how long the hold
        has left, before it runs a single model or tool call. The first
        invoke never notices and carries on to its ordinary finish, one tool
        run, one full drive, with no trace of the refused second instance in
        the log.
        """
        thread_id = "thread-two-instances"
        run_id = run_id_for_thread(thread_id)
        caught = {}  # type: Dict[str, Any]

        def second_instance_attempt() -> None:
            """What a second app instance invoking this same thread while the
            first is mid-tool-call looks like: its own client, its own
            middleware, its own agent, refused before any of it runs. Always
            the synchronous surface, regardless of which transport is driving
            the outer invoke: `meddle()` itself is a plain call (see
            `alookup_body`), and the point under test, `_tape_for` and
            `_aopen_tape` wrapping `lease_held` the same way, does not need a
            second event loop nested inside this one to be proven.
            """
            second_client = Client(self.base)
            try:
                second_agent, _ = self.agent_for(ONE_TOOL_SCRIPT, second_client)
                try:
                    second_agent.invoke(ASK, self.thread(thread_id))
                except Exception as error:  # noqa: BLE001 - captured, not raised, here
                    caught["error"] = error
            finally:
                second_client.close()

        async def body(client: Any) -> None:
            meddling["do"] = second_instance_attempt
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            answer = await self.invoke(agent, ASK, self.thread(thread_id))
            meddling["do"] = None

            error = caught.get("error")
            self.assertIsInstance(
                error,
                SalvorMiddlewareError,
                "the second instance's own open was refused outright",
            )
            self.assertIs(salvor_error(error), error, "and it arrived bare")
            self.assertEqual(error.code, "lease_held")
            self.assertGreaterEqual(
                error.lapses_in_seconds, 1, "carrying how long the hold has left"
            )
            text = str(error)
            self.assertIn(thread_id, text, "the error names the thread")
            self.assertIn(run_id, text, "and the run")
            self.assertLess(
                text.index(thread_id),
                text.index(run_id),
                "the thread leads, then the run, matching the TypeScript twin",
            )
            self.assertIn("lapses in", text, "and how long the hold has left")

            self.assertEqual(
                ran["lookup"], 1, "only the first instance's tool body ran"
            )
            self.assertEqual(
                self.text_of(answer["messages"][-1]),
                "Order ORD-7781 is paid, 4200 cents.",
                "the first invoke completes normally, undisturbed",
            )
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
                "one ordinary drive; the refused second instance left no trace",
            )

        self.drive(body)

    # -- (l2) a write's token is no longer the run's current one --------------

    def test_an_invalid_drive_token_mid_invoke_is_the_one_driver_error_immediately(
        self,
    ) -> None:
        """`invalid_drive_token` is the other one-driver refusal: a write
        whose token is no longer the run's current lease.

        Under the old "newest caller wins" rule this could mean a benign race
        this drive would win by simply retrying; under the held-until-it-
        lapses rule it can now only mean another driver already holds the
        run, so it stops the invoke immediately, the same as `lease_held`
        does on an open, and never retries.

        There is no `salvor serve` flag to shrink the lease TTL and force a
        real lapse: `salvor serve --help` names only the environment variable
        `SALVOR_CLIENT_LEASE_TTL_SECS` in `API.md`, and shows no flag for it
        (checked against the built binary). So this drives the refusal
        through the driver API directly instead of waiting one out: the token
        this invoke's own driver holds is swapped for one that is not the
        run's current lease, at exactly the point a second driver's own write
        would have made it stale (between the tool's intent and its
        completion), and the very next write meets the refusal immediately.
        """
        thread_id = "thread-invalid-token"
        run_id = run_id_for_thread(thread_id)
        middleware_ref = {}  # type: Dict[str, Any]

        def corrupt_the_token() -> None:
            tape = middleware_ref["middleware"]._tapes[run_id]
            tape.run.drive_token = "dt_not_the_current_lease"

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            middleware_ref["middleware"] = self.last_middleware
            meddling["do"] = corrupt_the_token
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(agent, ASK, self.thread(thread_id))
            meddling["do"] = None

            self.assertEqual(caught.exception.code, "lease_lost")
            self.assertIsNone(
                caught.exception.lapses_in_seconds,
                "invalid_drive_token carries no lapse figure to report",
            )
            text = str(caught.exception)
            self.assertIn(thread_id, text, "the error names the thread")
            self.assertIn(run_id, text, "and the run")
            self.assertLess(
                text.index(thread_id),
                text.index(run_id),
                "the thread leads, then the run, matching the TypeScript twin",
            )
            self.assertNotIn(
                "lapses in",
                text,
                "invalid_drive_token carries no lapse figure to report",
            )
            self.assertEqual(
                ran["lookup"], 1, "the tool ran once; the failed write was not retried"
            )
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                ],
                "the completion never landed: the corrupted token refused it",
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
                kinds = await self.kinds_of(own, thread_id, store=store)
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
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(agent, ASK, self.thread(thread_id))
            refusal = caught.exception
            self.assertEqual(refusal.code, "run_exists")
            self.assertIsInstance(
                refusal.cause,
                SalvorAPIError,
                "the server's own refusal is underneath it",
            )
            self.assertEqual(refusal.cause.code, "run_exists")
            text = str(refusal)
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
            self.assertEqual(caught.exception.code, "thread_finished")
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
        """A dangling intent stops `finish_thread`, whatever left it dangling.

        A raised tool body is no longer one of those ways for a TRUSTED tool
        (see the case above: the raise is recorded as the call's failure, a
        completion), so this uses the case that still is one: a tool nobody
        may self-report ran, returned a result, and the log ends at the
        intent because `trust_completion = false` leaves it for a person.
        """
        thread_id = "thread-finish-open-intent"
        script = [
            {
                "content": "sending the payout",
                "tool_calls": [
                    {
                        "name": "wire_payout",
                        "args": {"order_id": "ORD-4242", "amount_cents": 4200},
                        "id": "call-wire",
                    }
                ],
            },
            {"content": "Payout confirmed."},
        ]

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(script, client, tools=[wire_payout])
            with self.assertRaises(ToolNeedsResolution):
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "pay ORD-4242 out"}]},
                    self.thread(thread_id),
                )

            run_id = run_id_for_thread(thread_id)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(finish_thread, client, thread_id)
            self.assertEqual(caught.exception.code, "open_intent")
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

    # -- (i2) finish_thread on a thread nobody ever invoked --------------------

    def test_finish_thread_on_a_thread_never_invoked_is_refused(self) -> None:
        """There is nothing to close, and saying so beats appending a
        `RunCompleted` to a run that has no `RunStarted` under it."""

        async def body(client: Any) -> None:
            thread_id = "thread-never-invoked"
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(finish_thread, client, thread_id)
            refusal = caught.exception
            self.assertEqual(refusal.code, "thread_never_invoked")
            self.assertIn(thread_id, str(refusal), "the refusal names the thread")
            self.assertIn("never been invoked", str(refusal))

        self.drive(body)

    # -- (o) the lease goes back when an invoke ends --------------------------

    def test_an_invoke_that_ends_hands_the_lease_back_for_the_next_opener(
        self,
    ) -> None:
        """A drive that is over stops holding the run, immediately.

        Lapsing is the safety net, not how a drive ends. Without a release, an
        invoke that returns leaves its lease standing for the rest of the TTL,
        and the next process to invoke that thread (a worker that picked the
        job up, a second replica, the same app after a redeploy) is refused
        `lease_held` for up to a minute over a drive that finished. So the
        middleware hands the lease back in `after_agent`, and a stranger with
        no memory of any token for this run takes it on the very next request.
        """
        thread_id = "thread-release-on-finish"
        run_id = run_id_for_thread(thread_id)

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            await self.invoke(agent, ASK, self.thread(thread_id))

            self.assertNotIn(
                run_id,
                client._client_run_tokens,
                "the client stopped remembering a token that now opens nothing",
            )

            # A different client object entirely, so nothing is being carried
            # by the token memory `Client` keeps for its own re-opens: this is
            # the bare open a second process would make.
            stranger = self.CLIENT(self.base)
            try:
                taken = await call(stranger.open_client_run, run_id=run_id)
                self.assertEqual(taken.run_id, run_id)
                self.assertEqual(
                    len(taken.log_envelopes),
                    7,
                    "the recorded log came back with the fresh lease",
                )
                self.assertIs(
                    await call(taken.release),
                    True,
                    "and this holder gives it back the same way",
                )
            finally:
                await call(stranger.close)

        self.drive(body)

    # -- (o2) and it goes back when the invoke dies instead -------------------

    def test_an_untrusted_tool_that_raises_stops_with_open_intent_and_nothing_posted(
        self,
    ) -> None:
        """An invoke that dies inside a tool body releases the lease -- and for
        an untrusted tool, posts nothing on its way out.

        `after_agent` does not run when a hook or a tool raises: LangChain
        re-raises and the graph stops there. If the lease only went back from
        that hook, the thread every crash touched would be locked for the rest
        of the TTL, which is exactly the thread somebody is about to retry. So
        the step that raised gives it back on its way out.

        `wire_payout` is declared `trust_completion = false`: the client never
        gets to say whether its own write landed, and that includes saying it
        FAILED, which is exactly what reporting this raise would be. So unlike
        a trusted tool's raise (recorded as the call's failure, a completion),
        nothing is posted here: the log ends at the intent the crash left
        open, for a person to settle once they have confirmed with the
        provider what the call actually did.
        """
        thread_id = "thread-release-after-a-crash"
        run_id = run_id_for_thread(thread_id)
        script = [
            {
                "content": "sending the payout",
                "tool_calls": [
                    {
                        "name": "wire_payout",
                        "args": {"order_id": "ORD-9119", "amount_cents": 900},
                        "id": "call-wire",
                    }
                ],
            },
            {"content": "Payout confirmed."},
        ]

        async def body(client: Any) -> None:
            payout_crashes["on"] = True
            agent, _ = self.agent_for(script, client, tools=[wire_payout])
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "pay ORD-9119 out"}]},
                    self.thread(thread_id),
                )
            payout_crashes["on"] = False
            refusal = caught.exception
            self.assertEqual(refusal.code, "open_intent")
            self.assertIn(run_id, str(refusal), "the error names the run")
            self.assertEqual(ran["payout"], 1, "the body ran and raised, once")
            self.assertEqual(
                (await self.kinds_of(client, thread_id))[-1],
                "ToolCallRequested",
                "nothing was posted: the intent is exactly as recorded, still open",
            )

            stranger = self.CLIENT(self.base)
            try:
                taken = await call(stranger.open_client_run, run_id=run_id)
                self.assertEqual(taken.run_id, run_id, "the run was free at once")
                await call(taken.release)
            finally:
                await call(stranger.close)

        self.drive(body)

    # -- (o3) a body longer than the whole lease --------------------------------

    def test_a_tool_body_longer_than_the_lease_keeps_the_run_by_beating(self) -> None:
        """A driver inside a long body says "still here" and keeps its run.

        A lease lapses when its driver makes no call for the TTL, and a tool
        body is exactly that: the intent goes in, the body runs for minutes,
        and nothing touches salvor until the completion. Without a heartbeat
        the lease lapses mid-body, another opener takes a run whose driver
        never went anywhere, and the completion is refused after the work was
        already done.

        So this runs a tool body three times the length of the whole lease
        (against a server started with `SALVOR_CLIENT_LEASE_TTL_SECS=1`) and
        asks two things of it: a rival invoking the same thread halfway
        through, well past the point the lease would have lapsed, is still
        refused `lease_held`; and the invoke itself finishes normally, with one
        intent and one completion recorded for the call.
        """
        thread_id = "thread-long-tool-body"
        port = free_port()
        base = "http://127.0.0.1:{port}".format(port=port)
        workspace = tempfile.mkdtemp(prefix="salvor-py-")
        self.addCleanup(shutil.rmtree, workspace, ignore_errors=True)
        store = str(Path(workspace) / "short-lease.db")
        proc = serve(port, store, {"SALVOR_CLIENT_LEASE_TTL_SECS": "1"})
        self.addCleanup(lambda: stop(proc))
        if not wait_until_up(base):
            raise unittest.SkipTest("salvor serve did not come up")

        rival = {}  # type: Dict[str, Any]

        def rival_attempt() -> None:
            """A second instance invoking the same thread, halfway through the
            first one's tool body. Always the synchronous surface, for the same
            reason case (l) gives: the refusal happens in the open, before any
            model or tool call, and needs no second event loop to prove."""
            # Once, whatever happens: were the rival ever to get the run (which
            # is the failure this case is here to catch), its own tool body
            # would reach this same hook and start a third instance, and so on.
            dawdle["midway"] = None
            second_client = Client(base)
            try:
                second_agent, _ = self.agent_for(ONE_TOOL_SCRIPT, second_client)
                try:
                    second_agent.invoke(ASK, self.thread(thread_id))
                    rival["took_it"] = True
                except Exception as error:  # noqa: BLE001 - captured, not raised
                    rival["error"] = error
            finally:
                second_client.close()

        async def body(_class_client: Any) -> None:
            own = self.CLIENT(base)
            try:
                dawdle["seconds"] = 3.0
                dawdle["midway"] = rival_attempt
                agent, _ = self.agent_for(ONE_TOOL_SCRIPT, own)
                started = time.monotonic()
                answer = await self.invoke(agent, ASK, self.thread(thread_id))
                took = time.monotonic() - started
            finally:
                dawdle["seconds"] = 0.0
                dawdle["midway"] = None

            try:
                self.assertGreater(
                    took, 3.0, "the body really did outlive the one-second lease"
                )
                self.assertEqual(
                    self.text_of(answer["messages"][-1]),
                    "Order ORD-7781 is paid, 4200 cents.",
                    "the invoke finished, its lease kept alive under it",
                )
                self.assertEqual(ran["lookup"], 1, "the tool body ran once")

                self.assertNotIn(
                    "took_it", rival, "the rival did not take the run mid-body"
                )
                refusal = salvor_error(rival.get("error"))
                self.assertIsNotNone(refusal, "the rival was refused by name")
                self.assertEqual(refusal.code, "lease_held")
                self.assertGreaterEqual(
                    refusal.lapses_in_seconds, 1, "and told how long the hold has"
                )
                self.assertIn(thread_id, str(refusal), "naming the thread it wanted")
                self.assertIn(run_id_for_thread(thread_id), str(refusal))

                self.assertEqual(
                    await self.kinds_of(own, thread_id, store=store),
                    [
                        "RunStarted",
                        "ModelCallRequested",
                        "ModelCallCompleted",
                        "ToolCallRequested",
                        "ToolCallCompleted",
                        "ModelCallRequested",
                        "ModelCallCompleted",
                    ],
                    "one intent, one completion: nothing was refused after the work",
                )
            finally:
                await call(own.close)

        self.drive(body)

    # -- (o4) catching a refusal, bare or wrapped -----------------------------

    def test_salvor_error_finds_the_refusal_bare_and_wrapped(self) -> None:
        """`salvor_error` is the one way an application catches these.

        LangChain re-raises what a middleware hook raises exactly as it was
        raised, so today the refusal arrives bare and `salvor_error` hands the
        same object back. That is not a promise, and an application's own retry
        or executor may wrap it, so the helper is written to find it under a
        wrapper, under an implicit context, and inside an exception group too.
        Anything with no salvor refusal in it at all answers `None`, which is
        what tells a handler to re-raise.
        """

        async def body(client: Any) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(getattr(agent, self.INVOKE), ASK)
            refusal = caught.exception

            self.assertIs(
                salvor_error(refusal), refusal, "the shape LangChain raises today"
            )
            self.assertEqual(refusal.code, "thread_id_missing")

            wrapped = RuntimeError("the app's own retry gave up")
            wrapped.__cause__ = refusal
            self.assertIs(salvor_error(wrapped), refusal, "found under a wrapper")

            try:
                try:
                    raise refusal
                except SalvorMiddlewareError:
                    raise RuntimeError("raised while handling it")
            except RuntimeError as chained:
                self.assertIs(
                    salvor_error(chained), refusal, "found under an implicit context"
                )

            grouped = _grouped([ValueError("unrelated"), refusal])
            if grouped is not None:
                self.assertIs(
                    salvor_error(grouped), refusal, "found inside an exception group"
                )

            self.assertIsNone(
                salvor_error(ValueError("nothing to do with salvor")),
                "an unrelated error is not claimed",
            )

        self.drive(body)

    # -- (o5) a thread id of the wrong type -----------------------------------

    def test_a_thread_id_that_is_not_a_string_is_refused_by_name(self) -> None:
        """An id this middleware cannot use is a different refusal from no id.

        The two have different fixes, so they are told apart: nothing passed is
        `thread_id_missing` (add the config), while an integer, or the empty
        string an app uses for "no thread yet", is `thread_id_invalid`, and the
        sentence says what arrived so the reader is not left guessing which of
        their ids reached it.
        """

        async def body(client: Any) -> None:
            for given, said in ((7781, "int"), ("", "an empty string")):
                agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
                with self.assertRaises(SalvorMiddlewareError) as caught:
                    await call(
                        getattr(agent, self.INVOKE),
                        ASK,
                        {"configurable": {"thread_id": given}},
                    )
                refusal = salvor_error(caught.exception)
                self.assertEqual(refusal.code, "thread_id_invalid")
                self.assertIn(said, str(refusal), "the refusal says what arrived")
                self.assertIn(
                    "non-empty string", str(refusal), "and what it needed instead"
                )

        self.drive(body)

    # -- a thread id is required ----------------------------------------------

    def test_an_invoke_with_no_thread_id_is_refused(self) -> None:
        async def body(client: Any) -> None:
            agent, _ = self.agent_for(ONE_TOOL_SCRIPT, client)
            with self.assertRaises(SalvorMiddlewareError) as caught:
                await call(getattr(agent, self.INVOKE), ASK)
            self.assertEqual(caught.exception.code, "thread_id_missing")
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
            self.assertEqual(caught.exception.code, "wrong_client")
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

    # -- a process leaving is not a call failing -------------------------------

    def test_a_keyboard_interrupt_mid_write_leaves_the_intent_open_and_propagates(
        self,
    ) -> None:
        """`KeyboardInterrupt` is a `BaseException`, not an `Exception`.

        The middleware's failure-reporting catch around a tool body is
        narrowed to `Exception` precisely so an interrupt is never mistaken
        for the call failing: nothing is posted, the intent is left exactly as
        recorded, open, and the interrupt propagates unchanged, the same
        dangling-write case a real crash leaves (see the case above with
        `RuntimeError`, now recorded as a failure, for the contrast).
        """
        thread_id = "thread-keyboard-interrupt-mid-write"
        run_id = run_id_for_thread(thread_id)
        script = [
            {
                "content": "stamping the ledger",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": "ORD-9500", "note": "seen"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "Stamped ORD-9500."},
        ]

        async def body(client: Any) -> None:
            stamp_interrupts["on"] = True
            agent, _ = self.agent_for(script, client)
            with self.assertRaises(KeyboardInterrupt):
                await self.invoke(
                    agent,
                    {"messages": [{"role": "user", "content": "stamp ORD-9500"}]},
                    self.thread(thread_id),
                )
            stamp_interrupts["on"] = False
            self.assertEqual(ran["stamp"], 1, "the body ran once and was interrupted")
            self.assertEqual(
                await self.kinds_of(client, thread_id),
                [
                    "RunStarted",
                    "ModelCallRequested",
                    "ModelCallCompleted",
                    "ToolCallRequested",
                ],
                "nothing was posted: the intent is exactly as recorded, still open",
            )

            stranger = self.CLIENT(self.base)
            try:
                taken = await call(stranger.open_client_run, run_id=run_id)
                self.assertEqual(taken.run_id, run_id, "the run was free at once")
                await call(taken.release)
            finally:
                await call(stranger.close)

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

    # -- a process leaving is not a call failing -------------------------------

    def test_cancelling_the_task_mid_write_leaves_the_intent_open_and_propagates(
        self,
    ) -> None:
        """The asyncio twin of the sync interrupt case, a real task
        cancellation rather than a raised exception the tool body chose.

        `asyncio.CancelledError` is a `BaseException`, not an `Exception`
        (since 3.8), for the same reason: cancelling a task is the caller
        taking the work away, not the work failing, and the middleware's
        failure-reporting catch around a tool body must not treat it as one
        either. Nothing is posted, the intent is left exactly as recorded,
        open, and the cancellation propagates out of the awaited task.
        """
        thread_id = "thread-cancelled-mid-write"
        run_id = run_id_for_thread(thread_id)
        script = [
            {
                "content": "stamping the ledger",
                "tool_calls": [
                    {
                        "name": "stamp_ledger",
                        "args": {"order_id": "ORD-9600", "note": "seen"},
                        "id": "call-stamp",
                    }
                ],
            },
            {"content": "Stamped ORD-9600."},
        ]

        async def scenario() -> None:
            async with AsyncClient(self.base) as client:
                stamp_interrupts["on"] = True
                stamp_interrupts["event"] = asyncio.Event()
                agent, _ = self.agent_for(script, client)
                task = asyncio.ensure_future(
                    agent.ainvoke(
                        {"messages": [{"role": "user", "content": "stamp ORD-9600"}]},
                        self.thread(thread_id),
                    )
                )
                # Wait for the body to actually be running -- past its intent,
                # inside its await point -- before taking the task away.
                await stamp_interrupts["event"].wait()
                task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                stamp_interrupts["on"] = False
                self.assertEqual(
                    ran["stamp"], 1, "the body started once and was cancelled"
                )
                self.assertEqual(
                    await self.kinds_of(client, thread_id),
                    [
                        "RunStarted",
                        "ModelCallRequested",
                        "ModelCallCompleted",
                        "ToolCallRequested",
                    ],
                    "nothing was posted: the intent is exactly as recorded, still open",
                )

                stranger = AsyncClient(self.base)
                try:
                    taken = await stranger.open_client_run(run_id=run_id)
                    self.assertEqual(taken.run_id, run_id, "the run was free at once")
                    await taken.release()
                finally:
                    await stranger.close()

        asyncio.run(scenario())


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
