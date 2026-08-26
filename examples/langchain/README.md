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
else changing; `run.sh` unsets it for its own invokes, for a reason given below.

## The desk

Three tools, and one order book standing in for a real order system:

- **`lookup_order`** (a `read`): what an order is and what it cost.
- **`refund_order`** (a `write`): the ordinary refund, up to the desk's limit.
- **`refund_large`** (a `write`, `trust_completion = false`): the refund above
  that limit, which no report from the desk is allowed to close.

The conversation is nearly always the same three turns: look the order up,
refund it, say what happened. Each refund tool appends a line to a ledger file,
which is this desk's stand-in for a payment provider, and each line is keyed by
the idempotency key salvor derived for that call. A key already on file returns
the refund that key produced and writes nothing, which is exactly what a real
provider does with an `Idempotency-Key` header.

One ticket takes a shorter path. It names its own amount and says the refund is
on it twice, so there is nothing to look up and the desk asks for the same
refund twice in the one turn. Proof 8 is what salvor does with that.

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
[`refund-large.toml`](tools/refund-large.toml) carries the same pair, and there
it binds a person rather than the app: nothing may report on that tool, so the
only thing those names can hold to the intent is a hand-recorded resolution.

[`refund-order.toml`](tools/refund-order.toml) carries one more field,
`idempotency_key = ["order_id", "amount_cents"]`, which decides what a call's
identity is. Left unset, salvor derives the key from the call's position in the
run, `(run, seq, tool)`: an attempt identifier, the same on every attempt at that
one call, which is all proof 3 needs. Naming fields makes the key what those
fields say the call is about instead of where it sits: a hash of
`(run, tool, order_id, amount_cents)` with no position in it, so two refunds of
the same order in one run derive one key only when the amount also matches, and
the second call settles from the first rather than moving money again. The field
list is the check to run against your own tools: a field left out of it is a
field two distinct calls may differ on and still collapse into one, so naming
`order_id` alone once let a second call for a different amount on the same order
silently take the first's result, with nothing telling the app a call had been
copied except the log (`salvor history` shows it as
`deduplicated: copied from`). Every name must be required by `input_schema`, or
the server refuses the declaration when it loads it. Proof 8 is that difference,
and the choice is the operator's in the same file as everything else: an app
cannot widen its own key.

Here is that file whole, with its long comments stripped down to the fields
themselves:

```toml
name = "refund_order"
effect = "write"
trust_completion = true
require_equal = ["order_id", "amount_cents"]
idempotency_key = ["order_id", "amount_cents"]

[input_schema]
type = "object"
required = ["order_id", "amount_cents"]
description = "Refund an order in full, up to the desk's own limit."

[input_schema.properties.order_id]
type = "string"
description = "The order to refund, for example ORD-7781."

[input_schema.properties.amount_cents]
type = "integer"
minimum = 1
maximum = 99999
description = "The amount to refund, in cents. Refunds above 99999 go to refund_large."

[output_schema]
type = "object"
required = ["order_id", "amount_cents", "refund_id", "status"]

[output_schema.properties.order_id]
type = "string"

[output_schema.properties.amount_cents]
type = "integer"

[output_schema.properties.refund_id]
type = "string"

[output_schema.properties.status]
type = "string"
enum = ["succeeded"]
```

### The app's schema and the operator's declaration

Each tool is described twice, from two sides, and neither copy is redundant. The
`z.object({...})` / typed signature in `app.ts` and `app.py` is what the MODEL
sees: it is the function definition LangChain sends to the provider, and it is
how the model knows a refund takes an order id and an amount in cents. The
declaration in [`tools/`](tools/) is what the SERVER enforces: every intent's
input is checked against `input_schema` before it becomes history, and every
completion against `output_schema` and `require_equal` before it does.

A completion that fails either check is refused before anything is written, so
the intent stays open exactly as if the completion had never arrived. The next
invoke does what an open intent always does next: performs the call again
under the same key when the tool may close its own report (proof 3), or waits
for a person when it may not (proof 6).

When they disagree, the declaration wins, always, and the disagreement surfaces
as a refusal naming the field rather than as a wrong recording. Type `amount`
in one place and `amount_cents` in the other and the first opened intent
answers with a `400` that says so. That is the intended
failure mode, not an inconvenience: the app's copy is a hint to a model that may
ignore it, and the operator's copy is a gate that cannot be. It also means
editing a tool is editing both files. Change the arguments in the app without
changing the declaration and the next call is refused; change the declaration
without changing the app and the model keeps producing arguments the server will
not take.

Fixing that disagreement is itself a fork. The app's `z.object({...})` / typed
schema is part of what the model request is hashed from, so changing it
changes the hash the next invoke opens its first model call with, and the
thread's recorded first position no longer matches. Invoking the same thread
again does not resume it; it forks, and the whole conversation runs again from
there, writes included. What that costs depends on the write's own
declaration. `refund_order` names `idempotency_key` fields, so the re-run
derives the same key from `order_id` and `amount_cents` and settles from the
first call rather than moving money again (`salvor history` shows it as
`deduplicated: copied from`); a write whose declaration names no fields
derives a new positional key on the fork and runs again for real, and a
refund tool that runs again for real can refund the customer twice. Every
model call before the fork is also paid for a second time (the rule is
stated in [The honest limits](#the-honest-limits)), but that bill is the
smaller of the two. Give the corrected task a new thread id whenever the thread holds a write.
That is not a preference: it is what keeps a second run from moving money
the first one already moved.

## Running it

```
cargo build
bash examples/langchain/run.sh
```

It brings up a `salvor serve` per language on its own port (18401 and 18402 by
default) over its own store, installs what each app needs, and then makes the
same eight proofs twice, TypeScript first. It exits 0 only if all sixteen hold,
and every check that does not hold prints a `FAILED: expected ...` line naming
what it wanted and what it found, so a run that stopped early can never be
mistaken for one that passed. It takes well under a minute, most of it two
deliberate waits: a tool body held open for five seconds, and a crashed driver's lease
lapsing.

Two things about the script are demo posture and not advice. It starts each
server with no `--auth-token`, which leaves the control plane open to anything
that can reach the port; that is fine for a loopback port that lives for forty
seconds and is wrong for anything else, so set one and put the server behind a
proxy for anything past a demo (see
[`docs/OPERATIONS.md`](../../docs/OPERATIONS.md)). And the scratch directory is
not cleaned up: the two stores, the six ledgers and every captured invoke are
still there when the run finishes, written at whatever file mode the account's
umask gives them, so anything else on the machine that can read the directory can
read the refunds. That is deliberate, because the proofs are only worth
something if you can go and read the log yourself afterwards. Delete the
directory when you are done reading it.

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

None of that is specific to this directory. Until the LangChain extra reaches
the registry, an app of your own runs the same `npm install <path-to-checkout>/sdks/typescript`
or `pip install '<path-to-checkout>/sdks/python[langchain]'` line from wherever
that app lives, not only from inside `examples/langchain`; see the install
notes in [`sdks/typescript/README.md`](../../sdks/typescript/README.md#langchain)
and [`sdks/python/README.md`](../../sdks/python/README.md#langchain).

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

## The eight proofs, against the recorded log

Every log below is real output from a run of `run.sh`, read back with
`salvor history <run> --store <store>`.

`run.sh` always runs the scripted model. It unsets `ANTHROPIC_API_KEY` for its
own invokes, whatever the shell that started it had, because most of the proofs
below assert an exact number of model calls and an app cannot count a call a
provider made on its behalf: under a key each invoke prints `MODEL CALLS:
unavailable (real provider)` instead of a number, and those checks would have
nothing to compare. Set a key and invoke an app by hand and the same replay
happens, position for position, with the counts reading `unavailable`. See
"Swapping in a real model" below.

### 1. The first invoke pays for everything once

```
   0  RunStarted           agent sha256:51e608f… input {"thread_id":"orders-7781"}
   1  ModelCallRequested   request sha256:95202bf… [Client]
   2  ModelCallCompleted   usage in 0 out 0
   3  ToolCallRequested    lookup_order [Read] [Client] key sha256:02f11262… input {"order_id":"ORD-7781"}
   4  ToolCallCompleted    output {"order_id":"ORD-7781","status":"paid","total_cents":4200}
   5  ModelCallRequested   request sha256:a8425f0… [Client]
   6  ModelCallCompleted   usage in 0 out 0
   7  ToolCallRequested    refund_order [Write] [Client] key sha256:9d6df81e… input {"amount_cents":4200,"order_id":"ORD-7781"}
   8  ToolCallCompleted    output {"amount_cents":4200,"order_id":"ORD-7781","refund_id":"re_e93422b6b8fe",…
   9  ModelCallRequested   request sha256:d0adbb0… [Client]
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
{"order_id":"ORD-7781","amount_cents":4200,"refund_id":"re_e93422b6b8fe","status":"succeeded","tool":"refund_order","idempotency_key":"sha256:9d6df81e6a7c0497538de28faffc6030fe497fbe8e2b6c0e6d20e93422b6b8fe"}
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
   7  21:53:50Z  ToolCallRequested  refund_order [Write] [Client] key sha256:b99ac79f… input {"amount_cents":15900,"order_id":"ORD-8120"}
```

An intent with nothing after it: the log's last word is a call that was asked
for, and nothing in the log says whether the money moved. The desk is then invoked
again, which is all a worker picking the job up does. It replays everything
before seq 7 for free, runs that one tool body again, and the body finds the key
already on file:

```
[desk] refund_order: key sha256:b99ac79f5b137... is already on file; no second refund
MODEL CALLS: 1
TOOL BODIES: 1
MARKERS: replayed@1,replayed@5,live@9
```

The log closes the call it left open, ten seconds after it opened it:

```
   7  21:53:50Z  ToolCallRequested  refund_order [Write] [Client] key sha256:b99ac79f…
   8  21:54:00Z  ToolCallCompleted  output {"amount_cents":15900,"order_id":"ORD-8120","refund_id":"re_5863eb647eaf",…
```

One intent, one completion, one key, one line in the ledger. Say plainly what
salvor did and did not do there. It guaranteed that the call is recorded once
and that the key it derived is the same on both attempts. It did not stop the
tool body from running twice, and nothing can: the crash happened after the
money moved. What collapses the duplicate is the key reaching the provider,
which is why both apps read it with `currentToolCall()` / `current_tool_call()`
and hand it to the ledger. A tool that drops that key can charge twice, and
salvor's log will record it once either way.

What `effect` buys is worth stating exactly, because it is easy to over-credit.
It does not buy the key: salvor derives one for a `read` as readily as for a
`write`, from the same declaration. It does not buy the recovery either: the
intent is in the log because it is written ahead of the call, whatever the
effect says, so the next invoke walks back to that position regardless. What
`write` versus `read` decides is what an unanswered intent MEANS, and therefore
who is allowed to act on it. A dangling `write` is the run's terminal question:
the fold in `salvor-replay` reports `needs_reconciliation`, and nothing performs
the call again on its own, because a write performed twice is the failure this
whole design exists to prevent. It waits for a person, and proof 6 is what that
looks like. A dangling `read` is not a question at all, because nothing outside
the process changed, so the next invoke simply performs it again and records the
answer, which is what `lookup_order` declaring `read` buys. The same split is
the retry policy on a call recorded as having FAILED: a failed read is worth
performing again, a failed write is worth someone looking first.

Neither of this desk's tools declares the third class, `idempotent`, but it is
worth naming here because it answers the same question differently again. A
dangling `idempotent` intent is not a question either: the next invoke performs
the call again, the way a dangling `read` does, but under the same derived key
a `write` relies on `trust_completion` for, so a provider that honors that key
collapses the retry on its own side without anyone confirming the report first.
It is for a call with a side effect worth keeping, like sending an email, that
still does not need a person to trust its result.

The reason proof 3's re-invoke can run the body again at all is that
`refund_order` is a write the desk may close for itself, and the key is what
makes running it twice safe. That is not a licence the effect class grants; it
is one `trust_completion = true` grants, and `refund_large` withholds.

The ten seconds between the two events are the lease the crashed process left
behind. A driver that dies says nothing, so its hold ends on a timer, and
nothing else may open the thread until it does. `run.sh` starts each server with
`SALVOR_CLIENT_LEASE_TTL_SECS=8` so the proof does not sit out a full minute,
which is a demo's number and not a production one. In production the default is
60 seconds: a desk that crashes hard holds its thread for that whole minute
before anything else can pick the job up, or until a person resolves its
dangling write over HTTP, which drops the dead lease along with the resolution
(proof 6 does exactly that). The retry belongs in the app, not in a sleep: catch
`lease_held`, read `lapsesInSeconds` / `lapses_in_seconds` off the refusal, and
poll rather than wait the window out, because a live driver usually finishes
well before its hold does. Both SDK READMEs write that handler out, and
`desk_when_free` in `run.sh` is the same loop. That lease lives in this one
server's memory, which is also why `run.sh` runs one `salvor serve` per store
and never two against the same file; see
[`docs/OPERATIONS.md`](../../docs/OPERATIONS.md#one-salvor-serve-per-store).

### 4. Two copies of the desk, one thread

`run.sh` starts one copy with `--slow-tool lookup_order=5` and, while that body
is open, starts a second on the same thread. The second is refused before it
calls a model or runs a tool:

```
MODEL CALLS: 0
TOOL BODIES: 0
REFUSED lease_held: thread `orders-3050` (run dd8ee71d-e5d1-8dc5-a82d-c6208fce7344)
cannot be opened: another driver holds its lease right now, and it lapses in 8s if
that driver goes quiet (or as soon as the run finishes). One driver per thread at a
time. Wait for the lease to lapse and invoke again, or confirm no other process is
already driving this thread.
LAPSES IN: 8
```

That is the whole line, wrapped to fit; the desk prints it as one. The lease is
HELD, not handed to whoever asks last, and the refusal says how long the hold has
left so a caller can back off rather than guess: `LAPSES IN` is that number,
lifted off the refusal, which is what a retry loop should sleep against.
Meanwhile the first copy finished normally: a tool body that takes longer than
the TTL keeps the run it never left, because the middleware beats a heartbeat
while the body runs.

### 5. A new question down an old thread forks, and says so

Thread `orders-7781` has already been asked about ORD-7781. Ask it about
ORD-9002 and the request at the first recorded position no longer matches what
the log holds there:

```
[desk] FORK at seq 1: salvor: thread `orders-7781` (run
ea14b3ef-42b6-82dc-85bd-7fd80cc53df1) left its recorded path at seq 1. Nothing from
there replays: every model call and every tool call for the rest of this invoke is
being performed and recorded afresh, and the messages carry
`response_metadata.salvor.forked` saying so. If this thread was meant to resume,
look for a tool whose result differs between invokes, or a graph that branches on
the clock or on randomness.
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
the server's URL and no store path at all.

Nothing times out or escalates while the run waits; it stays stopped until someone
resolves or abandons it. Such runs show up in `salvor list --store <path> --group waiting`
and in the [Bridge](../../README.md#the-bridge) dashboard's Inbox. Setting up
alerting on them is the operator's job.

A person's resolution is held to the declaration too, which is the thing worth
noticing here. It is not a back door: an output that fails `output_schema` or
that changes a `require_equal` field is refused before anything is written, so a
typed amount that is not the one the intent recorded cannot become the run's
history. `run.sh` tries a dropped zero first, and gets this:

```
400 {"error":{"code":"bad_request","message":"the output offered for `refund_large`
reports `amount_cents` as 24000, but the intent recorded 240000; a resolution may
not alter a require_equal field. Record what was authorized, or abandon the run if
the provider did something else"}}
```

Nothing was recorded, and the run still needs its resolution. A resolver's
authority is to say what the provider did with the call that was authorized, not
to authorize a different one; if the provider really did something else, the
honest move is `POST /v1/runs/{id}/abandon`, which the refusal names. Then the
right amount:

```sh
curl -X POST http://127.0.0.1:18401/v1/runs/2ac4ce19-…/resolve \
  -H 'content-type: application/json' \
  -d '{"output": {"order_id": "ORD-4400", "amount_cents": 240000, "refund_id": "re_06b3fa2dd968", "status": "succeeded"}}'
```

```
{"resolved":true,"run":"2ac4ce19-…","status":{"state":"running"}}
```

One honest thing about the demo: its "person" is a `python3` line that reads the
output the app printed and posts it straight back. That is a stand-in for the
step, not the step. The whole reason `trust_completion = false` exists is that
the desk's report is the one thing not to be believed about this call, so a real
resolver opens the payment provider's own records, finds the refund the
idempotency key names, and records what THAT says. Reposting the app's output
would record the desk's claim with a person's signature on it.

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
does. It is not quite indistinguishable: the completion carries `settled_by:
"operator"`, and `salvor log` renders it `[Operator]`, so a reader can always
tell a call a person closed from one the desk closed. Replay never reads that
field.

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

### 8. The same refund, asked for twice in one turn

Proof 3 is one call retried. This is two calls, at two positions, that are the
same refund. The ticket for ORD-5150 names its own amount and says the refund is
on it twice, so the desk asks for it twice in the one turn:

```
[desk] refund_order moved money: $33.00 on ORD-5150 as re_e540d225cf69
MODEL CALLS: 2
TOOL BODIES: 1
ANSWER: Refunded $33.00 on ORD-5150; the provider has it as re_e540d225cf69.
```

Two calls asked for, one tool body run, one line in the ledger. The log says
where the second one went:

```
   3  ToolCallRequested  refund_order [Write] [Client] key sha256:dd001dd6… input {"amount_cents":3300,"order_id":"ORD-5150"}
   4  ToolCallCompleted  output {"amount_cents":3300,"order_id":"ORD-5150","refund_id":"re_e540d225cf69",…
   5  ToolCallRequested  refund_order [Write] [Client] key sha256:dd001dd6… input {"amount_cents":3300,"order_id":"ORD-5150"}
   6  ToolCallCompleted  output {"amount_cents":3300,"order_id":"ORD-5150","refund_id":"re_e540d225cf69",… (deduplicated: copied from run c7b2cdc3-… seq 3)
```

Both intents are recorded, because both were genuinely asked for and a log that
hid the second one would be lying about what the run did. Both carry the same
key, because `refund-order.toml` declares
`idempotency_key = ["order_id", "amount_cents"]`, both calls name the same
order and the same amount, and the derivation has no `seq` in it. When the
second intent was opened, salvor
found that identity already claimed by a call this run had finished, so it
copied that call's completion onto the new position, named what it copied, and
answered the middleware `settled: true`. The desk's tool body was never called
for it: `TOOL BODIES: 1` is the whole point, and the desk's own "key already on
file" guard never even ran, because the request never reached it.

Leave the declaration silent and the same turn behaves differently: two
positions, two keys, two calls, and the money moves twice unless the desk's own
ledger catches it. This is the case a positional key deliberately does not
cover, and it is why the choice sits in the operator's file. A read never
deduplicates however it is declared, because answering a repeated read out of an
older one would freeze a loop that is polling for a change on purpose.

## Swapping in a real model

Set a key and invoke an app directly:

```sh
ANTHROPIC_API_KEY=sk-ant-... node examples/langchain/app.ts \
  --server http://127.0.0.1:18401 --thread orders-7781 \
  --ask "Refund ORD-7781, the item arrived damaged."
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

Not through `run.sh`, which unsets the key for its own invokes on purpose. Its
proofs assert exact model-call counts, and neither app can count a call a
provider made on its behalf: `MODEL CALLS` prints `unavailable (real provider)`
under a key, so those checks would have nothing to compare. Everything the
proofs are actually about still holds, because none of it is the model's: the
same positions replay, a crash still costs one refund, the same intents are
recorded. Only the counting stops.

Two other honest caveats. A real model costs money and answers differently every
time, so the desk's answers will not be the sentences quoted above. And a real
model deciding differently on a re-invoke is a fork by the rule in proof 5, which
is the correct behavior and not a bug: the question is no longer the one the
recorded answer was an answer to.

## Why the scripted model is hand-rolled

Neither app uses LangChain's own test doubles.
`FakeStreamingChatModel` answers every turn with its first response, so a
tool-calling agent loops on the same tool forever, and `FakeToolCallingModel`'s
`bindTools` rebuilds itself on every call, which silently drops anything attached
to the instance. So each app carries a few dozen lines of `BaseChatModel` that
decide the next turn from the messages so far: turn 0 looks the order up, turn 1
reads that lookup out of the tool message and picks the refund tool the amount
calls for, turn 2 reads the refund out of its tool message and closes out. A real
model decides the same three things from the same three inputs. The one
exception is proof 8's ticket, which names its own amount and says the refund is
on it twice: there the scripted turn 0 asks for the refund twice and skips the
lookup, which is what a model reading a duplicated line item does.

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

A log whose last event is a call that never completed is not one thing, and the
middleware treats the kinds differently. An open `read` intent is not refused at
all: nothing outside the process changed, so the next invoke performs that call
again and records the answer. An open `write` on a tool the desk may close for
itself is not refused either, and proof 3 is that path: the invoke runs the body
again at the same position under the same key, and the key is what stops the
duplicate. What IS refused outright is an open `write` on a tool a person must
confirm, the shape proof 6 leaves behind. There the whole point of
`trust_completion = false` is that this process's word is not accepted for this
call, so running it a second time is precisely the move the declaration exists to
rule out. The middleware stops and names the run, and a resolution over HTTP (or
`salvor resolve`) is the only way past it.

A thrown tool body is recorded as the call's failure only when the tool is
declared `effect = "write"`, and which kind of failure it was decides what
happens next. A `write` body that raises is posted to salvor as that call's
failure, recorded as the same error sentinel a native tool writes when its
retries run out, and then rethrown unchanged, so LangChain still sees exactly
what the tool raised. The call is closed rather than dangling, so the thread is
not wedged. The consequence is worth being clear about: a later invoke reaching
that position meets the recorded failure and is refused with
`SalvorMiddlewareError` (`tool_failed`), naming the seq, without running the body
again. A recorded failure settles the call the same way on every replay, exactly
as a recorded success does, so a permanently failing input fails the same way
forever. It also survives a fork of the thread: a reworded ask that forks still
opens the same write under the same key, and meets the same recorded failure.
Only a new thread id escapes it. A `read` or `idempotent` body that raises is
the opposite case: nothing is posted at all, its intent stays
open exactly as it would if the process had simply died there, and the next
invoke performs the call again, which is why a transient connection error on
a lookup does not wedge the thread the way a failed write's record would. A
tool a person must confirm is the exception on the write side, as ever: salvor
would not take this process's word for "it did not land" any more than for "it
landed", so a throw there posts nothing and stops the invoke with
`open_intent`, for a person to settle. "Settle" is not always "resolve": if
what the provider actually shows is that the call never happened, there is
nothing to resolve, and the honest move is to abandon the run
(`POST /v1/runs/{id}/abandon`, or `salvor abandon`) and give the next task a new
thread id.

A model call is the other side of it. When the provider errors, nothing is
recorded at all: the intent is left open, and the next invoke simply performs
that call again. That is right because a model call buys an answer and changes
nothing in the world, which is the same reason a dangling `read` is performed
again while a dangling `write` waits.

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
