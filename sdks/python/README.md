# salvor (Python)

A thin Python client for the Salvor control plane.

```sh
pip install salvor
```

```python
from salvor import Client

with Client("http://127.0.0.1:8080") as client:
    agent = client.register_agent(open("agent.toml").read())
    run_id = client.start_run(agent, {"question": "..."})

    for event in client.stream_events(run_id):
        print(event.seq, event.kind)

    state = client.get_run(run_id)
    print(state.status.state)
```

You need a control plane to talk to: `npm install -g @salvor-run/cli && salvor serve`, or
see the [repository](https://github.com/joseym/salvor) for other install routes.

## What the control plane is

Salvor is a durable execution runtime for AI agents. A run is an append-only
log of events: every model call and every tool call is recorded before the run
moves on, so a process that dies mid-flight is recovered from the log and
finished from exactly where it stopped, with no completed step run twice.

The control plane is a small HTTP and server-sent-events server that puts that
runtime on a network. It owns one event store and drives runs in the
background. You submit an agent definition and an input, then read the run's
events as they land. The full contract is in
`crates/salvor-server/API.md`.

## Why the SDK is thin

The durability guarantees stay in one Rust process. Exact replay, crash-safe
resume, and the write-ahead rule that parks a run whose write was recorded but
never completed all live server-side, enforced by the same runtime the CLI
uses. So this SDK is a few hundred lines: it submits data, reads events, and
maps the server's error envelope to exceptions. It holds no agent loop, no
run state, and no durability logic of its own. Because the server does all the work,
the SDK stays consistent with it by construction.

## Install

```sh
pip install salvor
```

The one runtime dependency is `httpx`. To work on the SDK itself, install it
from a checkout instead: `pip install -e sdks/python`.

## The client surface

```python
from salvor import Client

client = Client("http://127.0.0.1:8080", token=None)

agent    = client.register_agent(toml_or_dict)      # -> agent hash
run_id   = client.start_run(agent, input=None)      # -> run id
state    = client.get_run(run_id)                   # -> RunState
runs     = client.list_runs()                       # -> list[RunSummary]
stream   = client.stream_events(run_id, from_seq=None)  # -> EventStream
result   = client.resume(run_id, input=None)        # -> ResumeResult
state    = client.resolve(run_id, output)           # record a dangling write
result   = client.abandon(run_id, reason=None)      # retire a run by hand; the dangling write stays named
projected = client.replay(run_id)                   # -> ReplayState (dry run)
```

`register_agent` accepts a TOML string (sent as `application/toml`) or a dict
of the same fields (sent as `application/json`). An agent is data, so it has a
content hash; submit it once and reference it by that hash on every start.

## The streaming and cursor model

`stream_events` returns an `EventStream` you iterate for
[`Event`](salvor/models.py) objects in sequence order:

```python
stream = client.stream_events(run_id)
for event in stream:
    print(event.seq, event.kind)
print(stream.end.status.state)   # the resting status the end frame carried
```

On connect the server replays every recorded event at or after the cursor,
then tails new events as they land, then sends one terminal `end` frame and
closes. A run's log has contiguous, ascending sequence numbers, so the stream
is gap-free and duplicate-free by construction, and the client only has to
track one number: the next sequence to expect.

That same number is what makes a dropped connection recoverable. If the socket
drops mid-tail, the client reconnects with `?from_seq=<next>` and the server
resumes from there. Any event that arrived just before the drop is skipped by
sequence number, so the merged stream stays gap-free and duplicate-free across
the reconnect. Iteration stops at the `end` frame; its status (and a
`detached` flag, set when the run is mid-step with no driver in this server
process) is then on `stream.end`.

## Errors

Every server error is decoded from the one JSON envelope
(`{"error": {"code", "message", "details?}}`) into a `SalvorAPIError` carrying
the stable `code` and the `message`. The one refusal with structured evidence,
a resume blocked because a write was recorded but never completed, raises
`NeedsReconciliationError`, whose `.intent` is the recorded write. Verify what
that write did, then call `resolve(run_id, output)` to record its completion so
replay never re-runs it.

```python
from salvor import NeedsReconciliationError

try:
    client.resume(run_id)
except NeedsReconciliationError as e:
    print("stuck on write:", e.intent.get("tool"), e.intent.get("input"))
    client.resolve(run_id, output={"charged": True})
    client.resume(run_id)
```

## The two modes

Salvor has two modes, and this SDK speaks both. The one above is **server-driven**:
`start_run` hands the agent loop to the server, which drives it in a background
task, and you read the events it produces. The second is **client-driven**: your
code owns the loop and streams the events it produces, while the server still
owns the durable log and, on every append, re-folds the log to confirm the
incoming event is the one legal next event. The two never collide: a
client-driven run and a server-driven run cannot share an id, and each surface
serves only its own runs.

Open a client-driven run and drive it with a `ClientRunDriver`:

```python
from salvor import Client

with Client("http://127.0.0.1:8080") as client:
    run = client.open_client_run(record_prompts=False)   # -> ClientRunDriver

    # The client emits its own control and context events through the guarded
    # append; the server confirms each is the legal next event before recording.
    run.append([run.envelope(0, "RunStarted", agent_def_hash=agent, input=task)])

    # The one side-effecting step the server must perform (it holds the key):
    result = run.model_step(1, request)          # -> ModelStepResult (response, usage)
    # or stream it, painting a live ticker:
    stream = run.model_step_stream(1, request)
    for delta in stream:
        ...                                      # {"type": "text_delta", ...}
    completion = stream.completion               # -> ModelStepResult

    # A tool the server's registry holds:
    output = run.tool_step(3, "render", {"doc": "plan.typ"})

    # A tool the server holds no code for at all, the payment case: you run
    # it, salvor just records that it happened.
    intent = run.client_tool_intent(4, "charge_card", {"amount_cents": 500})
    receipt = charge_card(intent.idempotency_key, {"amount_cents": 500})  # your code, your key
    run.client_tool_completion(4, receipt)

    # A model call the client makes itself, with its own key and config:
    m_intent = run.client_model_intent(5, "sha256:the-request")     # -> ClientModelIntentResult
    if not m_intent.settled:
        response, usage = call_your_own_model(request)              # your code, your key
        run.client_model_completion(5, response, usage)

    run.append([run.envelope(6, "RunCompleted", output=answer)])
```

The driver can also park the run on a durable timer: `sleep_until(seq, wake_at)`
records the park at a chosen instant, `sleep_for(seq, duration)` records a
clock reading first and derives `wake_at` from it so the same instant
replays later, and `await_wake(seq)` is what a later drive calls to find out
whether the deadline has passed. Nothing on the server watches the clock for
you, so the client wakes its own run: it replays its log, calls
`await_wake`, and either learns the run is still asleep (nothing appended) or
gets the `SleepCompleted` appended and carries on. Once the deadline is past,
a `sleeping` run's status also reports `overdue` (`True`) and
`overdue_seconds` (whole seconds since `wake_at`); both fold to `False`/`None`
before then.

The driver's full surface: `open` (also re-opens, i.e. resumes, an existing
run), `log(from_seq=0)`, `append(events)`, `model_step`, `model_step_stream`,
`tool_step`, `client_tool_intent`, `client_tool_completion`,
`client_model_intent`, `client_model_completion`, `sleep_until`,
`sleep_for`, `await_wake`, and
`resolve(output)`. Re-opening a run returns its recorded log on
`run.log_envelopes` and mints a fresh drive token (the single-writer lease every
append presents), so a refreshed client rebuilds its cursor and re-drives from
the log, paying nothing for a step the log already covers. A client-driven
append the log rejects raises `DivergenceError`; a tool step that lands on a
dangling write raises `NeedsReconciliationError` (whose `.intent` is the recorded
write), which `resolve(output)` clears.

`client_tool_intent` and `client_tool_completion` are for a tool salvor never
runs: a team keeps its payment code in its own process, and salvor only
records that the call happened and what it returned. Open the intent to get an
idempotency key the server derived (not one you chose, so a retry cannot mint
itself a second charge), perform the call yourself under that key, then report
the result. `client.list_client_tools()` fetches the declared tools, each with
the schema to hand the model as that tool's function definition. A completion
is refused, unrecorded, when the declaration does not trust a client's own
report or carries no output schema to check it against; settle those by hand
with `resolve` once you have verified the call externally. A reported output
that fails the declared schema is refused too, and there the fix is the output
itself.

`client_model_intent` and `client_model_completion` are the same idea for a
model call: a team that wants its own key and its own model configuration
calls the provider itself, and salvor only records that the call happened and
what it returned. Open the intent with `request_hash`, your own hash of the
request you are about to send, since this server never sees the request and
cannot recompute the hash the way `model_step` does; a `settled` of `False`
means make the call, `True` means it is already recorded and the returned
`response` and `usage` are the answer, paid for on an earlier drive. Report
the completion with the response and the token counts, since `usage` is what
every budget counts against and there is no way for salvor to fold one out of
a response shape it has never seen. A different hash at an already-recorded
position raises `DivergenceError`, the same as a diverging tool call would;
there is no `client_completion_refused` case here beyond the server having
performed the call itself, since a model response carries no operator schema
to check a report against.

`examples/browser-client-run` drives this same client-driven surface from a
browser page, and `example/client_run_loop.py` drives it from Python.

## Async

Everything above has an async twin: `AsyncClient` for `Client`, and
`AsyncClientRunDriver` for `ClientRunDriver`. Construct it the same way, with
the same arguments, and await every method by the same name.

```python
from salvor import AsyncClient

async with AsyncClient("http://127.0.0.1:8080", token=None) as client:
    agent  = await client.register_agent(toml_or_dict)
    run_id = await client.start_run(agent, {"question": "..."})
    state  = await client.get_run(run_id)
```

There is no separate list of methods to learn, because there is no separate
list. Both clients read one sans-IO core that holds every path, every body and
every decode, so a method means on `AsyncClient` exactly what it means on
`Client`, returns the same models, and raises the same errors. The test suite
runs one set of scenario bodies through both transports for that reason.

Streaming is `async for`, and the two streaming methods are not coroutines:
they hand back the iterator, and the request goes out when iteration starts.

```python
stream = client.stream_events(run_id)          # not awaited
async for event in stream:
    print(event.seq, event.kind)
print(stream.end.status.state)
```

The client-driven driver works the same way, with two exceptions worth knowing
about. `open_client_run` IS awaited, because opening a run is a request. And
`envelope(...)` is not, because it builds a dict and touches nothing.

```python
async with AsyncClient("http://127.0.0.1:8080") as client:
    run = await client.open_client_run()                       # awaited
    await run.append([run.envelope(0, "RunStarted",            # envelope is not
                                   agent_def_hash=agent, input=task)])
    result = await run.model_step(1, request)

    stream = run.model_step_stream(1, request)                 # not awaited
    async for delta in stream:
        ...
    completion = stream.completion

    # the model step took seqs 1 and 2 (intent, completion), so the park
    # starts at 3 and the reading it derives from lands there first
    wake_at = await run.sleep_for(3, timedelta(hours=1))
    if not (await run.await_wake(5)).woken:
        return                                                 # still asleep

    await run.append([run.envelope(6, "RunCompleted", output=answer)])
```

`AsyncClientRunDriver.open(base_url, ...)` opens (or re-opens) a run on its own
connection, awaited, the way `ClientRunDriver.open` does on a synchronous one.
Closing is awaited on both async classes: `await client.close()`, or let
`async with` do it. `aclose()` is accepted as well, for hands used to httpx.

## Graphs

A graph document composes `agent`, `tool`, `gate`, `branch`, `map`, `fold`, and
`delay` nodes into an authored control flow: an acyclic set of steps submitted
once, hashed, and run by that hash exactly as an agent definition is.
`GraphBuilder` mirrors the seven node kinds as typed constructors, so a
document gets editor typing and completion instead of hand-written JSON; the
semantic checks (referential integrity, acyclicity) live server-side, on
submit or `salvor graph validate`.

An `agent` node references an agent by its content hash, never by path. Get
one from `register_agent`, which accepts a TOML string and returns the hash,
computed server-side and validated as a side effect; with no server running,
`salvor agent hash <FILE>` prints the same hash from the command line.

```python
from salvor import GraphBuilder

draft_schema = {
    "type": "object",
    "properties": {"draft": {"type": "string"}},
    "required": ["draft"],
}

graph = (
    GraphBuilder()
    .agent("research", research_agent_hash, output_schema=draft_schema)
    .agent(
        "review",
        review_agent_hash,
        input_schema=draft_schema,
        output_schema=draft_schema,
    )
    .gate(
        "approve",
        {
            "type": "object",
            "properties": {"approved": {"type": "boolean"}},
            "required": ["approved"],
        },
        prompt="Approve this draft for publication?",
    )
    .tool(
        "publish",
        "http_post",
        input={"body": "approve.draft", "url": "config.publish_url"},
    )
    .edge("research", "review")
    .edge("review", "approve")
    .edge("approve", "publish")
    .build()
)
```

`example/build_graph.py` builds this same research, review, gate, publish flow
and prints the document; pipe it into `salvor graph validate /dev/stdin` for
the semantic checks the builder itself does not run.

Submit the built document, then start a run from the hash it returns:

```python
submitted = client.submit_graph(graph)        # -> GraphSubmitted
run_id = client.start_graph_run(submitted.graph, {"topic": "..."})
projection = client.get_run_graph(run_id)      # -> GraphProjection
```

Two things every caller meets here. First, the server keeps submitted
documents IN MEMORY only: a restart drops them, and a hash from a previous
process no longer resolves. That is safe rather than lossy, since submitting
the identical document again mints the identical hash, so a caller can simply
resubmit before starting a run. Second, a stock `salvor serve` wires an empty
tool registry, so every `tool` node refuses with `unknown_tool` until a host
registers the tool it names; `salvor serve --demo-tools` is the built-in way to
get a non-empty one.

The `approve` node above is the interesting case. The run parks there with
`state == "suspended"` and the schema (`reason`, `input_schema`) the approval
must satisfy; `resume` continues it with that approval, the same call an
ordinary agent run's park uses:

```python
result = client.resume(run_id, {"approved": True})   # -> ResumeResult
```

A parked graph run continues through that same call. The run's log recorded
only the graph's hash, not the document itself, so resume takes the document
again: the server looks it back up by that hash before it can re-drive the
walk, which means resuming depends on the document still being resolvable in
memory, the same restart caveat submission carries above.

Forking continues a run from a node boundary into a new run without touching
the origin: `client.fork_run(run_id, "review")`, previewed first with
`client.preview_fork`, and listed per run with `client.list_forks`. See the
`fork_run` docstring in `salvor/client.py` for the write-replay-hazard refusal
a fork guards against.

## LangChain

`salvor.langchain` is an optional module that makes an existing `create_agent`
app durable without changing its graph, its provider or its key. LangChain is
an extra, so the plain `import salvor` pulls none of it in:

```sh
pip install 'salvor[langchain]'
```

The LangChain extra is in the next release of `salvor`; it is not on PyPI yet,
so until that release ships, install the SDK from a checkout of this
repository instead (`pip install '<path-to-checkout>/sdks/python[langchain]'`),
and come back to the line above once it is. That checkout install works the
same way from any directory, so an app of your own outside this repository
installs against it exactly as `examples/langchain` does.

Then add one middleware to the agent you already have:

```python
from langchain.agents import create_agent
from salvor import Client
from salvor.langchain import SalvorMiddleware

salvor = Client("http://127.0.0.1:8080")

agent = create_agent(
    model=model,
    tools=tools,
    middleware=[SalvorMiddleware(salvor)],
)

agent.invoke(
    {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
    {"configurable": {"thread_id": "order-7781"}},
)
```

### Try it without a key

The client-driven tool below needs a declaration before the model can call
it: its effect class, its schemas and its idempotency key are the operator's,
never the middleware's, and they come from a client-tool declaration the
server was started with. Skip this and the first call fails with

```
unknown_tool: no client-performed tool named `lookup_order` is declared on this
server; declarations are loaded by the operator (`salvor serve --client-tool
<FILE>`) and are never registered over HTTP
```

Save the declaration as `lookup-order.toml`:

```toml
name = "lookup_order"
effect = "read"
trust_completion = true

[input_schema]
type = "object"
required = ["order_id"]

[input_schema.properties.order_id]
type = "string"

[output_schema]
type = "object"
required = ["order_id", "status", "total_cents"]
```

and start a server over a throwaway store with it loaded:

```sh
salvor serve --store ./try-salvor.db --client-tool lookup-order.toml
```

The rest below runs the same middleware end to end with no provider key and no
network: a scripted model stands in for whatever provider your app actually
uses. Save it as `try_salvor.py`:

```python
from langchain.agents import create_agent
from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import AIMessage
from langchain_core.outputs import ChatGeneration, ChatResult
from langchain_core.tools import tool

from salvor import Client
from salvor.langchain import SalvorMiddleware


class ScriptedModel(BaseChatModel):
    """A hand-rolled model, not one of the fakes in
    langchain_core.language_models.fake_chat_models: those cannot script a
    multi-turn tool-calling agent, and a bind_tools that rebuilds the model
    drops anything attached to the instance it replaces."""

    script: list = [
        {
            "content": "looking that up",
            "tool_calls": [
                {"name": "lookup_order", "args": {"order_id": "ORD-7781"}, "id": "call-1"}
            ],
        },
        {"content": "Order ORD-7781 is paid, 4200 cents."},
    ]

    @property
    def _llm_type(self) -> str:
        return "scripted"

    def bind_tools(self, tools, **kwargs):
        return self

    def _generate(self, messages, stop=None, run_manager=None, **kwargs) -> ChatResult:
        turn = len([m for m in messages if m.type == "ai"])
        step = self.script[min(turn, len(self.script) - 1)]
        message = AIMessage(
            content=step["content"],
            tool_calls=[dict(call, type="tool_call") for call in step.get("tool_calls", [])],
        )
        return ChatResult(generations=[ChatGeneration(message=message)])


@tool
def lookup_order(order_id: str) -> dict:
    """Look up an order that has already been placed."""
    return {"order_id": order_id, "status": "paid", "total_cents": 4200}


salvor = Client("http://127.0.0.1:8080")

agent = create_agent(
    model=ScriptedModel(),
    tools=[lookup_order],
    middleware=[SalvorMiddleware(salvor)],
)

answer = agent.invoke(
    {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
    {"configurable": {"thread_id": "order-7781"}},
)

print(answer["messages"][-1].content)
```

Run it in a second terminal, with the server still up. The LangChain extra is
in the next release of `salvor`, not yet on PyPI, so until then install from a
checkout of this repository instead of the registry line below
(`pip install '<path-to-checkout>/sdks/python[langchain]'`):

```sh
pip install 'salvor[langchain]'
python try_salvor.py
```

It prints `Order ORD-7781 is paid, 4200 cents.`. Run it again and it prints the
same line without calling the model or the tool at all: the second invoke of
the thread replays what the first one recorded. `salvor history <run> --store
./try-salvor.db` shows the seven events behind that, and the run id for thread
`order-7781` is what `run_id_for_thread("order-7781")` returns.

When you are done, stop the server and delete `try-salvor.db` along with its
`try-salvor.db-wal` and `try-salvor.db-shm` side files: SQLite writes all three,
and deleting only the first leaves a store that can still be read back.

A real app replaces `ScriptedModel` with its provider model (`ChatAnthropic`,
`ChatOpenAI`, and so on) and nothing else changes.

Both ways of driving an agent work, and the client says which one this agent is
driven by. A `Client` records under `agent.invoke` and `agent.stream`; an
`AsyncClient` records under `await agent.ainvoke` and `agent.astream`:

```python
from salvor import AsyncClient

agent = create_agent(
    model=model,
    tools=tools,
    middleware=[SalvorMiddleware(AsyncClient("http://127.0.0.1:8080"))],
)

await agent.ainvoke(
    {"messages": [{"role": "user", "content": "how is ORD-7781?"}]},
    {"configurable": {"thread_id": "order-7781"}},
)
```

The recording is the same recording either way: the same log positions, the same
request hashes, the same derived keys, so a thread recorded under one client can
be resumed under the other. Driving the agent the way the client does not
support is refused by name, and the refusal says which client to pass, rather
than quietly recording nothing. Under the synchronous client nothing here starts
an event loop: the calls to the control plane are made on whichever thread
LangChain is already running the agent on. One background thread does get
started, and only while a tool body or a live model call is running: the
heartbeat that keeps the run's lease alive (see [The lease](#the-lease) below).
Under the asynchronous client that is a task on your loop instead.

### What gets recorded

Every model call and every tool call the agent makes, each as the intent and
completion pair salvor records for any run. The model call is still LangChain's:
the middleware opens the intent with a content hash of the request, lets the
call through to whatever provider and key the app configured, and records the
answer and its token counts. Salvor never sees the request and never holds the
key, which is why the recorded `ModelCallRequested` says `performed_by:
"client"`. A tool call is the same shape, with the operator's derived
idempotency key on the intent, so a retried write presents the key the first
attempt used and the provider collapses the duplicate. Pass
`record_prompts=True` to store the request body on the intent as well, for an
inspector to show; replay never reads it, because the correlation key is the
hash alone.

Model responses, tool arguments and tool results are always recorded,
whatever `record_prompts` is set to; that flag only decides whether the
request body joins them. What a recorded payload can hold, and what that
means for personal data inside it, is spelled out in
[SECURITY.md](../../SECURITY.md#what-the-event-log-records)'s "What the event
log records". And because a thread's run stays open until `finish_thread`
closes it, an open thread keeps every one of those payloads for as long as
the store file exists: salvor has no retention of its own, which
[docs/OPERATIONS.md](../../docs/OPERATIONS.md#retention)'s "Retention"
section covers in full.

A tool that talks to its own provider can read that derived key without changing
its signature. `current_tool_call()` inside a tool body returns the
`ToolCallContext` salvor recorded for the call in flight: its `key`, `seq`,
`run_id` and `tool`. Hand `key` onward as the provider's own idempotency token,
and a retried write presents the key the first attempt used.

```python
from salvor.langchain import current_tool_call

@tool
async def refund_card(charge_id: str) -> dict:
    """Refund a charge."""
    call = current_tool_call()
    return await payments.refund(charge_id, idempotency_key=call.key)
```

What salvor guarantees about that key is worth stating plainly, because the
part it does not cover is the part that costs money. Salvor records the call
exactly once and derives the key from the run, the tool and the position, by
default; a declaration that names `idempotency_key` fields derives it from the
run, the tool and those fields instead, with no position in it. Either way the
key is stable: every attempt at that call, on this invoke or any later one, is
handed the same string. What salvor cannot make exactly-once is your
provider's side of it. A crash between the provider's success and salvor
recording the completion leaves the log ending at the intent, which is a call
that was asked for and never reported, and the next invoke runs the tool body
again under that same key. So the write happens twice unless the provider
treats the key as its idempotency token and collapses the second one. Pass
`call.key` to whatever your provider calls that (Stripe's `Idempotency-Key`,
your own API's dedupe column). A provider that ignores it, or a tool that never
passes it on, is a tool that can charge twice, and no ledger on this side
changes that.

A tool body that raises is a different case from the crash above, and salvor
does not leave it dangling: the middleware catches the raise, reports it as the
call's failure, and salvor records it as the call's completion the same way it
would record a returned value. So a raised tool body is recorded as a failure
and fails the same way on every replay, without running the body again; fix
whatever the tool keeps failing on and give the thread a new turn, or start a
new thread.

### What replay means

Invoking the same thread again re-opens the same run and walks the recorded
positions. Where the log already holds an answer, the middleware returns it and
the provider is not called; where the log already holds a tool result, the tool
body does not run. A thread that ran to the end and is invoked again a second
time costs nothing at all and returns the same final message.

Replay is keyed to the canonical request recorded at a log position, not to
whatever a model would currently answer for it, so a model or a test double
whose canonical request at that position differs from the one recorded forks
the thread there rather than replaying it.

A replayed answer says so. It carries `response_metadata["salvor"]` with
`{"replayed": True, "seq": ..., "run": ...}`, and under `agent.astream` it
arrives as one whole message rather than a re-tokenised imitation of the
original stream. The tokens happened once, on the invoke that paid for them, and
nothing here pretends otherwise.

A run that died between a tool's intent and its completion is the case the whole
design is for. The log ends at the intent, which is exactly what an unfinished
write looks like, and the next invoke replays everything before it for free,
performs that one call again under the same derived key, and records the
completion. One intent, one completion, no second charge. A provider error
between a model call's intent and its completion leaves the same kind of gap:
nothing is recorded for the failed attempt, the intent stays open, and the
next invoke posts it again and performs that call once more.

Parallel tool calls in one model turn are serialised rather than refused. A
turnstile inside the middleware admits one open intent per run at a time, in the
order the AI message listed the calls, so both are recorded and both replay at
the same positions on a later invoke. The order comes from that message and not
from arrival: measured over five identical runs of a three-tool turn under
`ainvoke`, LangChain reached the middleware in three different orders, so
arrival order could not be allowed to decide where a call lands in the log.
Under `invoke` the turn's calls arrive on LangChain's thread pool instead, and
the same rule admits them in the same order, so both drives record the same
turn at the same positions.

`current_tool_call()` reads back the key of the call in flight on either drive:
it is a `ContextVar`, and LangChain carries the current context into the tasks
and the worker threads it runs a turn's tools on, so a tool body reads its own
call's key and never a neighbour's.

### The thread id is the run id

A LangGraph `thread_id` that is already a UUID is used as the salvor run id
unchanged, so an application whose thread ids are UUIDs can look a run up by the
id it already holds. Any other thread id is hashed into one: SHA-256 of the
thread id, the first 16 bytes taken, with the version nibble set to 8 (RFC
9562's custom version, which is what a hash-derived id honestly is) and the
variant bits set. The mapping is stable forever and the same on every machine,
and the same in the TypeScript SDK, so a thread driven from either language is
the same run. Pass `thread_id_to_run_id` to replace it when your two ids live in
a table somewhere. Invoking without a thread id is an error, not a silent
pass-through: without one there is nothing for a later invoke to resume.

A thread's run stays open. The middleware never appends `RunCompleted` on its
own, because a thread that looks done today may get one more turn tomorrow. Call
`finish_thread(client, thread_id)` when the task really is over: it appends
`RunCompleted`, defaulting the output to the content of the last recorded AI
message, and a later invoke of that thread is refused by name. It refuses to
close a run whose log ends at a call that was never completed; settle that
first. It takes whichever client the middleware was given and answers the way
that client answers everything else, so it is `finish_thread(client, thread)`
under a `Client` and `await finish_thread(client, thread)` under an
`AsyncClient`.

### What the operator declares

A tool's effect class, its schemas and its idempotency key are the operator's,
never this middleware's. They come from a client-tool declaration the server was
started with:

```toml
name = "lookup_order"
effect = "read"
trust_completion = true

[input_schema]
type = "object"
required = ["order_id"]

[input_schema.properties.order_id]
type = "string"

[output_schema]
type = "object"
required = ["order_id", "status", "total_cents"]
```

```sh
salvor serve --client-tool lookup-order.toml
```

`effect` is one of three classes. `read` has no side effect, so a dangling
intent is simply performed again on the next invoke. `write` changes the
world in a way that is not safe to repeat blindly, so a dangling intent waits
for a person unless `trust_completion` says the tool may close its own call.
`idempotent` changes the world too, but under an identity safe to retry, so a
dangling intent is performed again under the same derived key and left for
the provider to collapse, with no person needed.

Keys are positional unless the declaration names `idempotency_key` fields
(`idempotency_key = ["order_id", "amount_cents"]`); the default is a hash of the
run, the position and the tool, so only the exact same call retried at the exact
same position shares one. With fields named, two calls in one run whose values
for those fields match share a key regardless of position, and the second's
intent comes back already settled, carrying the first's recorded result rather
than running its tool body. So a model that emits the same write twice in one
turn runs it once when those fields match and twice when they do not, unless
the provider's own idempotency handling dedupes it first. Naming fields is
naming what makes two calls the same call, so check the list against what
actually varies between calls you mean to keep distinct: a field left out of
it is a field two such calls may differ on and still collapse into one.

The middleware sends the tool's name and the arguments the model produced, and
nothing else. A tool with no declaration is refused, and the error names the
tool and the declaration it needs rather than quietly recording the call as a
harmless read. `trust_completion = true` with an `[output_schema]` is what lets
the middleware record what the tool returned; a declaration without them leaves
every call for that tool to be settled by hand, once someone has verified it
externally. Two ways, and the `ToolNeedsResolution` error this middleware
raises for such a call prints both:

```sh
# against the live server, over HTTP
curl -X POST http://127.0.0.1:8080/v1/runs/$RUN/resolve \
    -H 'content-type: application/json' \
    -d '{"output": <json the tool returned>}'

# or against the store, with the server up or down
salvor resolve $RUN --store <path to the server's store> --output '<json the tool returned>'
```

They differ in one way worth knowing before you pick. `POST
/v1/runs/{id}/resolve` also clears the run's lease, because unsticking a
dangling write says the driver that opened it is gone, so the thread can be
invoked again on the very next request. `salvor resolve` cannot: it reads and
writes the SQLite store directly rather than over HTTP, so a lease held in a
live server's memory survives it and lapses on its own, and the next invoke
waits up to the lease TTL. The command also always carries a `--store <path to
the server's store>` placeholder rather than a real path, because the
middleware only ever holds a base URL, never the file path `salvor serve
--store` was started with. `driver.resolve(output)` on a client run driver
holding the run's own lease settles it too.
`examples/client-tools/refund-card.toml` is the fully commented version of
the declaration file.

The recorded output is the tool's own result, which is what the output schema
describes. LangChain builds a tool message by stringifying whatever the tool
returned, so the result is recovered by parsing that content back when the parse
round-trips exactly; when it does not, the content is recorded as the string it
is, and an object schema will refuse it and say so.

### Catching a refusal

Every refusal the middleware raises carries a `code`, and `salvor_error(e)` is
how you get to it. It answers with the `SalvorMiddlewareError` inside whatever
you caught, or `None` when the error has nothing to do with salvor, which is
your signal to re-raise:

```python
import time
from salvor.langchain import salvor_error

try:
    agent.invoke(ask, {"configurable": {"thread_id": thread}})
except Exception as error:
    refusal = salvor_error(error)
    if refusal is None:
        raise                      # not ours; let it go past
    if refusal.code == "lease_held":
        # Somebody else is driving this thread right now, and it usually finishes
        # and releases well before its hold lapses, so poll every couple of seconds
        # instead of sleeping out the whole window.
        #
        # If the holder was a process that crashed rather than one that is still working,
        # nothing can release its lease from outside: it lapses on the timer, or sooner if
        # the run ended at a dangling write and a person resolves it over HTTP, which clears
        # the lease as well.
        deadline = time.monotonic() + refusal.lapses_in_seconds + 1
        while time.monotonic() < deadline:
            time.sleep(2)
            try:
                agent.invoke(ask, {"configurable": {"thread_id": thread}})
                break
            except Exception as retry_error:
                retry_refusal = salvor_error(retry_error)
                if retry_refusal is None or retry_refusal.code != "lease_held":
                    raise
        else:
            agent.invoke(ask, {"configurable": {"thread_id": thread}})
    elif refusal.code == "open_intent":
        alert_an_operator(refusal)  # a call was made and never reported
    else:
        raise
```

**Bare or wrapped, the helper covers both.** As of LangChain 1.3,
`create_agent` re-raises what a middleware hook raises exactly as it was
raised: a refusal from `before_agent`, `wrap_model_call`, `wrap_tool_call` or
`after_agent` reaches the caller of `invoke` bare, and so does an exception
your own tool body raised, and a parallel tool turn raises one of its failures
rather than a group of them. So `salvor_error(e)` usually hands back the same
object you caught. That is not a promise LangChain makes, though, and your own
retry wrapper or executor may put something around it later, so the helper
walks `__cause__`, `__context__` and the members of an exception group instead
of trusting the top of the chain. Catch `Exception`, call `salvor_error`, and
you do not have to care which shape arrived.

Each refusal also carries `cause`, the error underneath it when there was one
(the same object as `__cause__`; the second name is what the TypeScript
middleware calls it, so one handler reads the same against either SDK). A
`tool_undeclared` refusal, for instance, has the server's own `unknown_tool` on
its `cause`.

The codes:

| `code` | What happened |
| --- | --- |
| `lease_held` | Another driver holds this thread's run right now. `lapses_in_seconds` says how long their hold has left. |
| `lease_lost` | This invoke stopped being the driver: its token is no longer the current lease, or the lease went twice in one invoke. |
| `reopen_refused` | The lease was lost and the server would not hand the run back at all. The log is intact; this server is not the one to drive it from. |
| `run_exists` | The thread maps to a run id salvor's other, server-driven mode already started. Give the thread an id of its own. |
| `thread_finished` | `finish_thread` closed this thread's run, and a completed run takes no more events. |
| `thread_abandoned` | Somebody recorded a terminal `RunAbandoned` on this thread's run (`POST /v1/runs/{id}/abandon`, or `salvor abandon`). Nothing was replayed and nothing ran; give the next task a new thread id. |
| `thread_never_invoked` | `finish_thread` was asked to close a thread that has no run yet. |
| `thread_id_missing` | The invoke passed no `thread_id`. |
| `thread_id_invalid` | It passed one that is not a non-empty string. The message says what arrived. |
| `tool_undeclared` | The tool has no client-tool declaration on this server. |
| `tool_needs_resolution` | The tool ran and its operator settles such a call by hand. This one is the typed `ToolNeedsResolution`, with the result on `.output`. |
| `tool_returned_command` | A tool answered with a LangGraph `Command`, which is control flow, not a result to record. |
| `open_intent` | The log holds a call recorded as requested and never completed. Settle it and invoke again. |
| `unreadable_record` | A model answer is missing or does not read back as one. |
| `wrong_client` | The middleware was given the wrong client for the way the agent is being driven. |
| `bad_request` | The server's own refusal, unwrapped: a reported tool output failed the declared `output_schema`. |
| `client_completion_refused` | The server's own refusal: a `require_equal` field's reported value differed from the intent's, or the declaration has no `output_schema` to check against. |

Refusals that come from the control plane rather than from the middleware stay
`SalvorAPIError`, with their own stable `code` (see [Errors](#errors) above), so
`except SalvorAPIError` still catches those on their own terms. A driving call
the middleware makes inside a hook is different: when the server refuses it,
`salvor_error(e)` still finds a `SalvorMiddlewareError`, but its `code` is the
server's own (`bad_request`, `client_completion_refused`, and so on) and
`cause` is the `SalvorAPIError` underneath, so server refusals arrive with the
server's own code either way.

### The lease

One driver per thread at a time, and the drive token is how salvor says so. The
lease lives in the server's memory, not on disk, and it is HELD rather than
handed to whoever asks last: while a driver's lease on a thread's run is
current, a second instance invoking that same thread is refused at once, before
it runs a single model or tool call, naming the thread and how many seconds
until the hold lapses on its own (`lease_held`). A lease taken out from under an
active invoke mid-step means a second driver is live on the thread right now;
that is refused too (`invalid_drive_token`), by the same one-driver error, and
neither case is ever retried by re-opening, because there is no order in which
two live drivers can both be right about what comes next.

Four things end a hold, and only one of them is a timer.

**An invoke that ends releases it.** Success or failure, the middleware hands
the lease back on its way out (`POST /v1/client-runs/{id}/release`), so the
next process to invoke that thread takes it on the very next request. A
worker that picks the job up, a second replica, the same app after a redeploy:
none of them waits out a TTL for a drive that is already over. An invoke that
dies inside a tool body releases too, which matters more, because the thread a
crash touched is the thread somebody is about to retry.

**A long step keeps it alive.** A tool that takes minutes, or a model call your
app is streaming itself, makes no call to salvor while it works, and a lease
with no call inside its TTL lapses. So while a tool body or a live model call
is running, the middleware says "still here" (`POST
/v1/client-runs/{id}/heartbeat`) on the interval the server's own answer names:
a probe a quarter of a second in, then a third of the reported
`lapses_in_seconds` from there. Nothing beats for a tool that returns inside
that quarter second, which is nearly all of them. Under `invoke` that is one
daemon thread; under `ainvoke`, one task on your loop; both stop when the body
does.

**An HTTP resolve clears it.** Unsticking a run by hand with `POST
/v1/runs/{id}/resolve` says the driver that opened the dangling write is gone,
so the lease it left behind is dropped with the resolution and the thread is
invocable again at once. `salvor resolve` on the command line cannot do that:
it writes the store directly and never reaches a live server's memory, so a
lease held there survives the CLI resolve and lapses on its own instead.

**And a crash lets it lapse.** A driver that dies without releasing stops
presenting its token, and the hold ends when the TTL runs out
(`SALVOR_CLIENT_LEASE_TTL_SECS`, 60 seconds by default). That is the safety
net, not the way a drive is meant to end.

`Client` and `AsyncClient` each remember the last token they saw for a run and
present it back automatically on a later `open_client_run`, so your own app
re-invoking a thread it drove a moment ago is not what triggers either refusal;
each forgets a run's token the moment its lease is released, because a released
token opens nothing.

If salvor itself restarts mid-invoke, none of this applies: the lease registry
dies with the process but the log does not, so the middleware notices its open
run is gone (`unknown_run`), re-opens it once, and continues from the log as if
the restart had not happened. A restart is still survived.

### Reading a thread's log

Two ways, and they print the same envelopes:

```sh
# over HTTP, from the server
curl http://127.0.0.1:8080/v1/client-runs/$RUN/log

# off the store the server writes, with the server up or down
salvor history $RUN --store /path/to/store.db --json
```

`GET /v1/client-runs/{id}/log` is the read for a client-driven run. It needs no
drive token and no lease: it serves the run whether its driver is mid-invoke,
released the lease when the invoke ended, went quiet until the lease lapsed, or
was driving before the server last restarted. `?from_seq=<n>` fetches just the
tail. A run that is not client-driven is `404 unknown_run`.

`salvor history` reads the same log off the SQLite store instead, which is
where it actually lives, so it works with no server running at all;
`--json` prints the envelopes verbatim rather than the pretty log. In both,
`$RUN` is the run id the thread maps to, which is
`run_id_for_thread(thread_id)`.

### The honest limits

This is a recorded effect ledger with exactly-once writes and salvor's budgets,
under LangGraph's orchestration. It is not replay of the graph. LangGraph still
owns the clock, the randomness and the branch order, and salvor sees the calls
rather than the decisions between them. A graph that branches on `time.time()`,
a tool whose result differs between runs, or a genuinely new turn down an old
thread all mean the log holds a recorded position that does not match what the
invoke is actually doing this time. When that happens the middleware stops
replaying and appends the rest of the invoke at the end of the log, so the fork
is recorded rather than lost. Key order is no longer one of these causes: the
middleware writes every tool result in canonical, sorted-key JSON, so the live
bytes and the replayed bytes always match, and the model sees its tool results
with sorted keys either way.

Every AI message the middleware returns carries `response_metadata["salvor"]`,
saying which of the three things happened to it: `{"replayed": True, "seq":
...}` when the answer came from the log, `{"live": True, "seq": ...}` when it
was a real call on a path the log still agrees with, and `{"forked": {"at":
..., "thread": ..., "run": ...}}` on every message from the point the invoke
actually forked onward. A fork also calls `on_fork` once per invoke, naming
the thread, the run and the seq it forked at. That seq is the first recorded position that no longer matches, so when several things changed between invokes it points at the earliest of them, not necessarily the one you meant. By default it logs a warning,
and you can pass your own callback to route that wherever your app already
logs.

The one case it refuses is a log whose last event is a call that never
completed: settle that first (`POST /v1/runs/{id}/resolve`, or `salvor
resolve`, as above), then invoke again.

A thread is one task. Re-invoking it replays it; sending a genuinely new turn
down the same thread is a fork by the rule above, and pays for the calls the new
turn makes. Give a new task a new thread id.

`wrap_tool_call` exists only inside `create_agent`. A hand-built `StateGraph`
that calls tools from its own node has no hook for the middleware to sit in, so
such a graph gets model recording only and its tool calls stay outside the
ledger.

Changing a tool's schema or a model's settings mid-flight changes the request
hash, which is the same fork as above and is meant to be: the question is not
the one the recorded answer was an answer to.

## Runnable example

`example/agent.toml` is a model-only agent that answers one question. It is the
Python mirror of `examples/web-research`, driven over the control plane instead
of the CLI. Start a server with a key on its environment, then run the script:

```sh
npm install -g @salvor-run/cli  # or: cargo install salvor-cli

ANTHROPIC_API_KEY=sk-ant-... \
    salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db &

pip install salvor
python example/answer.py http://127.0.0.1:8080    # from sdks/python in a checkout
```

It registers the agent, starts a run, streams every event to completion, and
prints the final answer, the event count, and the token usage.
