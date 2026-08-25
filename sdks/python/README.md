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
