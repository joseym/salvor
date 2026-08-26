# langchain: the agent you already have, made durable

A support desk looks an order up and refunds it. That is two calls, and the
second one moves money. The failure that matters is not the model saying
something odd; it is the process dying in the half second between the payment
provider saying "done" and anything writing that down. Restart the desk and it
refunds the customer twice, or not at all, and the log cannot tell you which.

This example is that desk, written twice: [`app.ts`](app.ts) in TypeScript and
[`app.py`](app.py) in Python. Both are ordinary LangChain `createAgent` /
`create_agent` apps. Neither has a salvor-shaped design. The only salvor line in
either is one middleware in the `middleware` array, and what that buys is a
recorded ledger of every model call and every tool call the agent makes, with
replay on the far side of a crash.

Everything runs offline with no API key. Each app carries a scripted model that
reads the conversation so far and answers the way a real one would for this
desk, so the whole thing is free and identical on every machine. Set
`ANTHROPIC_API_KEY` and the same apps use `ChatAnthropic` instead, with nothing
else changing.

## The desk

Three tools, and one order book standing in for a real order system:

- **`lookup_order`** (a `read`): what an order is and what it cost.
- **`refund_order`** (a `write`): the ordinary refund, up to the desk's limit.
- **`refund_large`** (a `write`, `trust_completion = false`): the refund above
  that limit, which no report from the desk is allowed to close.

The conversation is always the same three turns: look the order up, refund it,
say what happened. Each refund tool appends a line to a ledger file, which is
this desk's stand-in for a payment provider, and each line is keyed by the
idempotency key salvor derived for that call. A key already on file returns the
refund that key produced and writes nothing, which is exactly what a real
provider does with an `Idempotency-Key` header.

## The one line that is salvor's

```ts
const agent = createAgent({
  model,
  tools: [lookupOrder, refundOrder, refundLarge],
  middleware: [salvorMiddleware({ client: new SalvorClient(server) })],
});

await agent.invoke(
  { messages: [{ role: "user", content: ask }] },
  { configurable: { thread_id: "orders-7781" } },
);
```

```python
agent = create_agent(
    model=model,
    tools=[lookup_order, refund_order, refund_large],
    middleware=[SalvorMiddleware(Client(server))],
)

agent.invoke(
    {"messages": [{"role": "user", "content": ask}]},
    {"configurable": {"thread_id": "orders-7781"}},
)
```

The `thread_id` LangGraph already wanted is the run. A thread id that is already
a UUID is used unchanged; anything else is hashed into one, the same way in both
SDKs, which is why `orders-7781` is run `ea14b3ef-42b6-82dc-85bd-7fd80cc53df1`
under TypeScript and under Python alike. Everything else in these apps is the
LangChain app a team would already have.

## What the operator declares, and what the app cannot

The three files in [`tools/`](tools/) are the operator's, loaded when the server
starts:

```sh
salvor serve --client-tool examples/langchain/tools/lookup-order.toml \
             --client-tool examples/langchain/tools/refund-order.toml \
             --client-tool examples/langchain/tools/refund-large.toml
```

There is no code behind them. The declaration format has no command field, no
path field and no URL field, and an unknown key is a parse error rather than an
ignored line, so a declaration cannot name `app.ts` even by accident. What each
one carries is the effect class, the schema the input must satisfy, the schema a
reported result must satisfy, and whether the desk may close its own call.

The middleware sends the tool's name and the arguments the model produced, and
nothing else. A tool with no declaration is refused by name rather than quietly
recorded as a harmless read, and the app has no way to register one: there is no
endpoint that accepts a declaration. That matters most for the pair here. The
desk's own limit lives twice in the declarations, as a `maximum` on
`refund_order` and a `minimum` on `refund_large`, so a large refund routed to
the trusted tool is refused by the server rather than by a hopeful `if` in the
application.

[`refund-order.toml`](tools/refund-order.toml) also carries
`require_equal = ["order_id", "amount_cents"]`, so salvor compares those fields
in the report to what the intent recorded and refuses a completion that alters
one. A schema check says a report is well shaped; `require_equal` says it is
about the call that was authorized.

## Running it

```
cargo build
bash examples/langchain/run.sh
```

It brings up a `salvor serve` per language on its own port (18401 and 18402 by
default) over its own store, installs what each app needs, and then makes the
same seven proofs twice, TypeScript first. It exits 0 only if all fourteen hold,
and every check that does not hold prints a `FAILED: expected ...` line naming
what it wanted and what it found, so a run that stopped early can never be
mistaken for one that passed. It takes well under a minute, most of it two
deliberate waits: a tool body held open for five seconds, and a crashed driver's lease
lapsing.

The first run installs things. On the TypeScript side, `npm install
--install-links --omit=optional` into `examples/langchain/node_modules`
(gitignored): `--install-links` copies this checkout's SDK in rather than
symlinking it, so its `langchain` import resolves to the one copy installed
beside it, and `--omit=optional` leaves out `@langchain/anthropic`, which the
key-free path never loads. On the Python side, a `python3 -m venv` under the
scratch directory with `pip install -e '../../sdks/python[langchain]'` in it.
Point `SALVOR_EXAMPLE_PYTHON` at an interpreter that already has
`salvor[langchain]` and the script uses that instead of building a venv at all:

```sh
SALVOR_EXAMPLE_PYTHON=/path/to/venv/bin/python bash examples/langchain/run.sh
```

Everything else is overridable too: `SALVOR_BIN` for the binary,
`SALVOR_EXAMPLE_SCRATCH` for where stores, ledgers and captured output land,
`SALVOR_EXAMPLE_TS_PORT` and `SALVOR_EXAMPLE_PY_PORT` for the two ports,
`SALVOR_EXAMPLE_NODE` and `SALVOR_EXAMPLE_NPM` for the Node toolchain, and
`SALVOR_EXAMPLE_PYVENV` for where the venv goes. Nothing runtime is written into
the repository except that `node_modules` directory, and no port near 8080 is
ever bound.

Both apps take the same flags, so any step below can be run by hand:

```sh
node examples/langchain/app.ts --server http://127.0.0.1:18401 \
     --thread orders-7781 --ask "Refund ORD-7781, the item arrived damaged."
```

`--crash-in refund_order` kills the process inside that tool, after its ledger
write and before it returns. `--slow-tool lookup_order=5` holds a tool body open
for five seconds. `--finish` closes the thread. Each run prints `MODEL CALLS`,
`TOOL BODIES`, `MARKERS` and `FORKS`, which is what `run.sh` reads its proofs
out of.

## The seven proofs, against the recorded log

Every log below is real output from a run of `run.sh`, read back with
`salvor history <run> --store <store>`.

### 1. The first invoke pays for everything once

```
   0  RunStarted           agent sha256:51e608f… input {"thread_id":"orders-7781"}
   1  ModelCallRequested   request sha256:ffee658… [Client]
   2  ModelCallCompleted   usage in 0 out 0
   3  ToolCallRequested    lookup_order [Read] [Client] key sha256:02f11262… input {"order_id":"ORD-7781"}
   4  ToolCallCompleted    output {"order_id":"ORD-7781","status":"paid","total_cents":4200}
   5  ModelCallRequested   request sha256:989ad59… [Client]
   6  ModelCallCompleted   usage in 0 out 0
   7  ToolCallRequested    refund_order [Write] [Client] key sha256:07513bd5… input {"amount_cents":4200,"order_id":"ORD-7781"}
   8  ToolCallCompleted    output {"amount_cents":4200,"order_id":"ORD-7781","refund_id":"re_b9f07ccfcfd0",…
   9  ModelCallRequested   request sha256:1a0f26a… [Client]
  10  ModelCallCompleted   usage in 0 out 0
```

Three model calls, two tool calls, each an intent and a completion. `[Client]` is
the interesting column: salvor never made these calls, it recorded them. The
model request is a content hash and not the prompt, because the middleware opens
the intent with the hash, lets the call through to whatever provider and key the
app configured, and records the answer. Salvor never sees the request body
unless you ask for it with `recordPrompts` / `record_prompts`, and even then
replay does not read it, because the correlation key is the hash.

The desk printed `MODEL CALLS: 3`, `TOOL BODIES: 2`, and the refunds ledger got
its one line:

```json
{"order_id":"ORD-7781","amount_cents":4200,"refund_id":"re_b9f07ccfcfd0","status":"succeeded","tool":"refund_order","idempotency_key":"sha256:07513bd5e1ecef2a5c65992d54120306037099493fb2e9f6fef7b9f07ccfcfd0"}
```

Every message the desk got back carries `MARKERS: live@1,live@5,live@9`: the
middleware marks each AI message with what happened to it, and `live` means this
invoke really made that call, on a path the log still agrees with.

### 2. The second invoke pays for nothing

Same thread, same question. `MODEL CALLS: 0`, `TOOL BODIES: 0`,
`MARKERS: replayed@1,replayed@5,replayed@9`, the same final answer, and neither
ledger grew. The log is unchanged: replay adds nothing, because every position
it walks is already recorded. The provider was not called, and the tool bodies
did not run, which is why the ledger can be checked rather than trusted.

That second invoke works immediately rather than waiting out a lease, because
the first one handed the thread's lease back on its way out.

### 3. A crash between the money and the record costs one refund

`run.sh` invokes a fresh thread with `--crash-in refund_order`. The refund lands
in the ledger, the process dies with exit 9, and the log ends here:

```
   7  05:07:09Z  ToolCallRequested  refund_order [Write] [Client] key sha256:a62b6c89… input {"amount_cents":15900,"order_id":"ORD-8120"}
```

An intent with nothing after it, which is exactly what an unfinished write looks
like and is why `write` is worth declaring. The desk is then invoked again,
which is all a worker picking the job up does. It replays everything before seq
7 for free, runs that one tool body again, and the body finds the key already on
file:

```
[desk] refund_order: key sha256:a62b6c89878e2... is already on file; no second refund
MODEL CALLS: 1
TOOL BODIES: 1
MARKERS: replayed@1,replayed@5,live@9
```

The log closes the call it left open, ten seconds after it opened it:

```
   7  05:07:09Z  ToolCallRequested  refund_order [Write] [Client] key sha256:a62b6c89…
   8  05:07:19Z  ToolCallCompleted  output {"amount_cents":15900,"order_id":"ORD-8120","refund_id":"re_3baa02ab8e00",…
```

One intent, one completion, one key, one line in the ledger. Say plainly what
salvor did and did not do there. It guaranteed that the call is recorded once
and that the key it derived is the same on both attempts. It did not stop the
tool body from running twice, and nothing can: the crash happened after the
money moved. What collapses the duplicate is the key reaching the provider,
which is why both apps read it with `currentToolCall()` / `current_tool_call()`
and hand it to the ledger. A tool that drops that key can charge twice, and
salvor's log will record it once either way.

The ten seconds between the two events are the lease the crashed process left
behind. A driver that dies says nothing, so its hold ends on a timer;
`run.sh` starts each server with `SALVOR_CLIENT_LEASE_TTL_SECS=8` and polls,
which is the `lease_held` handler both SDK READMEs write out. Production keeps
the 60 second default.

### 4. Two copies of the desk, one thread

`run.sh` starts one copy with `--slow-tool lookup_order=5` and, while that body
is open, starts a second on the same thread. The second is refused before it
calls a model or runs a tool:

```
MODEL CALLS: 0
TOOL BODIES: 0
REFUSED lease_held: thread `orders-3050` (run dd8ee71d-…) cannot be opened: another
driver holds its lease right now, and it lapses in 8s if that driver goes quiet
(or as soon as the run finishes). One driver per thread at a time.
```

The lease is HELD, not handed to whoever asks last, and the refusal says how
long the hold has left so a caller can back off rather than guess. Meanwhile the
first copy finished normally: a tool body that takes longer than the TTL keeps
the run it never left, because the middleware beats a heartbeat while the body
runs.

### 5. A new question down an old thread forks, and says so

Thread `orders-7781` has already been asked about ORD-7781. Ask it about
ORD-9002 and the request at the first recorded position no longer matches what
the log holds there:

```
[desk] FORK at seq 1: salvor: thread `orders-7781` (run ea14b3ef-…) left its
recorded path at seq 1. Nothing from there replays: every model call and every
tool call for the rest of this invoke is being performed and recorded afresh.
FORKS: 1
MARKERS: forked@1,forked@1
```

A fork is not an error and not a refusal. The invoke carries on and appends to
the log, so what happened is recorded rather than lost, and every message from
the fork onward carries the marker. The callback fires once per invoke; the
default warns on the console, and both apps replace it with their own line.

The rule this makes concrete: a thread is one task. Re-invoking it replays it;
sending a genuinely new turn down it forks and pays for the new calls. Give a
new task a new thread id.

### 6. A refund the desk is not allowed to close

ORD-4400 is $2,400.00, which is over the line, so the model calls `refund_large`.
That tool runs: refusing to run it would fix nothing, since not trusting the
desk's report is a different decision from not sending the money. What changes is
what happens next.

```
[desk] refund_large moved money: $2400.00 on ORD-4400 as re_06b3fa2dd968
NEEDS RESOLUTION: {"run":"2ac4ce19-…","seq":7,"tool":"refund_large","key":"sha256:1a00ecf8…",
                   "output":{"order_id":"ORD-4400","amount_cents":240000,…}}
```

The invoke stops with `ToolNeedsResolution` rather than letting salvor's `403
client_completion_refused` tear through LangGraph after the money moved. The run
is left at seq 7, an intent with no completion, the same shape the crash left.
A person confirms the transfer at the provider and records what they saw.
`run.sh` does that over HTTP, because a container running an agent usually has
the server's URL and no store path at all:

```sh
curl -X POST http://127.0.0.1:18402/v1/runs/2ac4ce19-…/resolve \
  -H 'content-type: application/json' \
  -d '{"output": {"amount_cents": 240000, "order_id": "ORD-4400", "refund_id": "re_06b3fa2dd968", "status": "succeeded"}}'
```

```
{"resolved":true,"run":"2ac4ce19-…","status":{"state":"running"}}
```

The HTTP resolve clears the run's lease along with the resolution, so the thread
re-opens at once. Invoke it again and the resolved output replays in the call's
place:

```
MODEL CALLS: 1
TOOL BODIES: 0
MARKERS: replayed@1,replayed@5,live@9
ANSWER: Refunded $2400.00 on ORD-4400; the provider has it as re_06b3fa2dd968.
```

Zero tool bodies, and the large-refunds ledger still holds exactly one line. What
resolve recorded is the same `ToolCallCompleted` the trusted tool wrote for
itself in proof 1, so a later replay of this log behaves the same way that one
does.

### 7. A finished thread takes no more invokes

A thread's run stays open by default: replay checks whether a position is
recorded, never whether the whole thread is done, because a task that looks
finished today may get one more turn tomorrow. `finishThread` /
`finish_thread` is how an application says it is really over:

```
FINISHED: run=ea14b3ef-… seq=17
  17  RunCompleted  output "ORD-9002 is paid, $15.00. Nothing to refund."
```

It records `RunCompleted` with the last AI message's content, or with whatever
value you pass, and closes the run for good. After that:

```
REFUSED thread_finished: thread `orders-7781` (run ea14b3ef-…) is finished:
`finishThread` recorded its `RunCompleted`, and a completed run cannot be
appended to. Give the next task a new thread id.
```

It refuses the same way, naming the run instead of the thread, when the log ends
at an open intent: settle that call first, then finish the thread.

## Swapping in a real model

Set a key and run the same script:

```sh
ANTHROPIC_API_KEY=sk-ant-... bash examples/langchain/run.sh
```

Each app picks `ChatAnthropic` over its scripted model when the key is there
(`SALVOR_LC_MODEL` names the model; the default is `claude-opus-5`). Nothing
else changes: the tools, the declarations, the middleware, the thread ids and
the log positions are the same, because salvor records the call and never the
provider. Install the provider package first, which the key-free path leaves
out:

```sh
cd examples/langchain && npm install --install-links   # adds @langchain/anthropic
pip install langchain-anthropic                        # into whichever interpreter runs app.py
```

Two honest caveats. A real model costs money and answers differently every time,
so the proofs that count model calls are written against the scripted one:
`MODEL CALLS` reads `unavailable (real provider)` under a key, and the desk's
answers will not be the sentences quoted above. And a real model deciding
differently on a re-invoke is a fork by the rule in proof 5, which is the correct
behavior and not a bug: the question is no longer the one the recorded answer was
an answer to.

## Why the scripted model is hand-rolled

Neither app uses LangChain's own test doubles.
`FakeStreamingChatModel` answers every turn with its first response, so a
tool-calling agent loops on the same tool forever, and `FakeToolCallingModel`'s
`bindTools` rebuilds itself on every call, which silently drops anything attached
to the instance. So each app carries about thirty lines of `BaseChatModel` that
decide the next turn from the messages so far: turn 0 looks the order up, turn 1
reads that lookup out of the tool message and picks the refund tool the amount
calls for, turn 2 reads the refund out of its tool message and closes out. A real
model decides the same three things from the same three inputs.

## What the log holds, and what it does not

Tool arguments, tool results and model answers are recorded verbatim, always.
This desk passes order ids and cent amounts and nothing else, so nothing about a
customer reaches the log; the amounts and the refund identifiers that do reach it
are the desk's own records duplicated. That is deliberate, and it follows
[`SECURITY.md`](../../SECURITY.md#what-the-event-log-records): the log cannot be
edited or deleted, so personal data kept out of what is passed into a tool call
is personal data kept out of the log. A thread's run also stays open until
something finishes it, and salvor has no retention of its own, so an open thread
keeps every recorded payload for as long as the store file exists. See
[`docs/OPERATIONS.md`](../../docs/OPERATIONS.md#retention)'s "Retention".

## The honest limits

This is a recorded effect ledger with exactly-once writes, under LangGraph's
orchestration. It is not replay of the graph. LangGraph still owns the clock, the
randomness and the branch order, and salvor sees the calls rather than the
decisions between them. A graph that branches on the clock, a tool whose result
differs between runs, or a genuinely new turn down an old thread all mean a
recorded position no longer matches what the invoke is doing, and all of them
fork, as proof 5 does on purpose.

`wrapToolCall` / `wrap_tool_call` exists only inside `createAgent`. A hand-built
`StateGraph` that calls tools from its own node has no hook for the middleware to
sit in, so such a graph gets model recording only and its tool calls stay outside
the ledger.

The one thing the middleware refuses outright is a log whose last event is a call
that never completed. Proof 3 meets that (the crash) and proof 6 meets it (the
large refund), and both are settled the same way: run the call again under its
key, or have a person resolve it.

## What is here

- [`app.ts`](app.ts) and [`app.py`](app.py): the same desk in both languages.
  Same tools, same order book, same scripted model, same flags, same printed
  counters.
- [`tools/`](tools/): the three client-tool declarations, one per tool, with the
  reasoning for each field in the file.
- [`package.json`](package.json): the TypeScript app's four dependencies, with
  `@salvor-run/client` by relative path because the LangChain entry point is not
  on npm yet, and `@langchain/anthropic` optional.
- [`requirements.txt`](requirements.txt): the same for Python, this checkout's
  SDK with its `langchain` extra.
- [`run.sh`](run.sh): the whole sequence, twice, with every port and path
  overridable.

The SDK reference for the middleware is the LangChain section of
[`sdks/typescript/README.md`](../../sdks/typescript/README.md#langchain) and
[`sdks/python/README.md`](../../sdks/python/README.md#langchain), which cover the
error codes, the lease, `recordPrompts`, `threadIdToRunId` and the streaming
variants this example does not use.
