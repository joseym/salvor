# @salvor-run/client (TypeScript)

A thin TypeScript client for the Salvor control plane.

```sh
npm install @salvor-run/client
```

```ts
import { SalvorClient } from "@salvor-run/client";

const salvor = new SalvorClient("http://127.0.0.1:8080");
const agent = await salvor.registerAgent(agentToml);
const runId = await salvor.startRun(agent, { question: "..." });

for await (const event of salvor.streamEvents(runId)) {
  console.log(event.seq, event.kind);
}
```

You need a control plane to talk to: `npm install -g @salvor-run/cli && salvor serve`.

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
uses. So this client is a few hundred lines: it submits data, reads events, and
maps the server's error envelope to exceptions. It holds no agent loop, no run
state, and no durability logic of its own. Because the server does all the work, the
client stays consistent with it by construction.

## Streaming over fetch, not EventSource

The stream is read with a small SSE parser over `fetch`'s response body
(`ReadableStream`). This is the lowest-friction approach for Node: the built-in
`fetch` (Node 18 and later) needs no dependency, and unlike the browser
`EventSource` it lets the client set the `Authorization` header on the stream
request and track its own cursor. So the package has zero runtime dependencies.
The same code runs in any environment with the platform `fetch` and
`ReadableStream`.

## Install and build

```sh
cd sdks/typescript
npm install      # dev dependencies only: typescript, and langchain for its suite
npm run build    # tsc -> dist/ and dist/langchain/
```

Then in your project:

```ts
import { SalvorClient } from "@salvor-run/client";
```

## The client surface

```ts
const client = new SalvorClient("http://127.0.0.1:8080", { token: undefined });

const agent  = await client.registerAgent(tomlOrObject);   // -> agent hash
const runId  = await client.startRun(agent, input);        // -> run id
const state  = await client.getRun(runId);                 // -> RunState
const runs   = await client.listRuns();                    // -> RunSummary[]
const stream = client.streamEvents(runId, { fromSeq });    // -> EventStream
const result = await client.resume(runId, input);          // -> ResumeResult
const after  = await client.resolve(runId, output);        // record a dangling write
const status = await client.abandon(runId, reason);        // retire a run by hand; the dangling write stays named
const proj   = await client.replay(runId);                 // -> ReplayState (dry run)
```

`registerAgent` accepts a TOML string (sent as `application/toml`) or an object
of the same fields (sent as `application/json`). An agent is data, so it has a
content hash; submit it once and reference it by that hash on every start. All
methods are `async`.

## The streaming and cursor model

`streamEvents` returns an `EventStream`, an `AsyncIterable` of
[`SalvorEvent`](src/types.ts) in sequence order:

```ts
const stream = client.streamEvents(runId);
for await (const event of stream) {
  console.log(event.seq, event.kind);
}
console.log(stream.end?.status?.state); // the resting status the end frame carried
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
(`{ "error": { "code", "message", "details?" } }`) into a `SalvorApiError`
carrying the stable `code` and the `message`. The one refusal with structured
evidence, a resume blocked because a write was recorded but never completed,
throws `NeedsReconciliationError`, whose `.intent` is the recorded write.
Verify what that write did, then call `resolve(runId, output)` so replay never
re-runs it.

```ts
import { NeedsReconciliationError } from "@salvor-run/client";

try {
  await client.resume(runId);
} catch (e) {
  if (e instanceof NeedsReconciliationError) {
    console.log("stuck on write:", e.intent.tool, e.intent.input);
    await client.resolve(runId, { charged: true });
    await client.resume(runId);
  } else {
    throw e;
  }
}
```

## The two modes

Salvor has two modes, and this client speaks both. The one above is
**server-driven**: `startRun` hands the agent loop to the server, which drives it
in a background task, and you read the events it produces. The second is
**client-driven**: your code owns the loop and streams the events it produces,
while the server still owns the durable log and, on every append, re-folds the
log to confirm the incoming event is the one legal next event. The two never
collide: a client-driven run and a server-driven run cannot share an id, and each
surface serves only its own runs.

Open a client-driven run and drive it with a `ClientRunDriver`:

```ts
const run = await client.openClientRun({ recordPrompts: false }); // -> ClientRunDriver

// The client emits its own control and context events through the guarded
// append; the server confirms each is the legal next event before recording.
await run.append([run.envelope(0, "RunStarted", { agent_def_hash: agent, input: task })]);

// The one side-effecting step the server must perform (it holds the key):
const { response, usage } = await run.modelStep(1, request);
// or stream it, painting a live ticker:
const stream = run.modelStepStream(1, request);
for await (const delta of stream) {
  if (delta.type === "text_delta") paint(delta.text);
}
const completion = stream.completion;             // -> ModelStepResult

// A tool the server's registry holds:
const output = await run.toolStep(3, "render", { doc: "plan.typ" });

// A tool the server holds no code for at all, the payment case: you run it,
// salvor just records that it happened.
const { idempotencyKey } = await run.clientToolIntent(4, "charge_card", { amount_cents: 500 });
const receipt = await chargeCard(idempotencyKey, { amount_cents: 500 }); // your code, your key
await run.clientToolCompletion(4, receipt);

// A model call YOU made, with your own key and model configuration: salvor
// records it so a resume replays the answer instead of paying for it again.
const opened = await run.clientModelIntent(5, hashOf(request));
if (opened.settled) return opened.response;                  // already recorded, pay nothing
const answered = await callTheProvider(request);             // your code, your key
await run.clientModelCompletion(5, answered, { inputTokens: 10, outputTokens: 5 });

await run.append([run.envelope(7, "RunCompleted", { output: answer })]);
```

The driver can also park the run on a durable timer: `sleepUntil(seq, wakeAt)`
records the park at a chosen instant, `sleepFor(seq, durationMs)` records a
clock reading first and derives `wakeAt` from it so the same instant replays
later, and `awaitWake(seq)` is what a later drive calls to find out whether
the deadline has passed. Nothing on the server watches the clock for you, so
the client wakes its own run: it replays its log, calls `awaitWake`, and
either learns the run is still asleep (nothing appended) or gets the
`SleepCompleted` appended and carries on. Once the deadline is past, a
`sleeping` run's status also carries `overdue: true` and `overdueSeconds`
(whole seconds since `wakeAt`); both are absent before then.

The driver's full surface: `openClientRun` (also re-opens, i.e. resumes, an
existing run), `log(fromSeq)`, `append(events)`, `modelStep`, `modelStepStream`
(an `AsyncIterable` of ticker deltas with a `completion` after), `toolStep`,
`clientToolIntent`, `clientToolCompletion`, `clientModelIntent`,
`clientModelCompletion`, `sleepUntil`, `sleepFor`,
`awaitWake`, and `resolve(output)`. Re-opening a
run returns its recorded log on `run.logEnvelopes` and mints a fresh drive
token (the single-writer lease every append presents), so a refreshed client
rebuilds its cursor and re-drives from the log, paying nothing for a step the
log already covers. A client-driven append the log rejects throws
`DivergenceError`; a tool step that lands on a dangling write throws
`NeedsReconciliationError` (whose `.intent` is the recorded write), which
`resolve(output)` clears.

`clientToolIntent` and `clientToolCompletion` are for a tool salvor never runs:
a team keeps its payment code in its own process, and salvor only records that
the call happened and what it returned. Open the intent to get an idempotency
key the server derived (not one you chose, so a retry cannot mint itself a
second charge), perform the call yourself under that key, then report the
result. `client.listClientTools()` fetches the declared tools, each with the
schema to hand the model as that tool's function definition. A completion is
refused, unrecorded, when the declaration does not trust a client's own report
or carries no output schema to check it against; settle those by hand with
`resolve` once you have verified the call externally. A reported output that
fails the declared schema is refused too, and there the fix is the output
itself.

`clientModelIntent` and `clientModelCompletion` are the same idea for a model
call salvor never makes: a middleware calls the provider with its own key and
its own model configuration, and salvor records the call so a resume replays
the recorded answer instead of paying for it a second time. Open the intent
with your own canonical hash of the request; `settled` comes back `true` when
that position's completion is already recorded, carrying the recorded
`response` and `usage`, which is what lets a resumed run short-circuit without
a second request. Salvor never sees the request, so the hash and the reported
answer are your claims, not facts it witnessed, and the recorded
`ModelCallRequested` says so with `performed_by: "client"`. The claim is a key
into your own log, so an inconsistently hashed request diverges against your
own history and nobody else's: a different hash at a recorded position throws
`DivergenceError`, as does a completion with nothing outstanding.

The driver uses only `fetch` and the SDK's own SSE parser, with no Node-only
API, so it runs unchanged in a browser tab: the streaming model step is a POST
that returns server-sent events, which `EventSource` cannot do, so the SDK parses
the fetch body stream itself. `examples/browser-client-run` drives this same
surface from a browser page, and `example/client_run_loop.ts` drives it from
Node.

## LangChain

`@salvor-run/client/langchain` is an optional entry point that makes an
existing `createAgent` app durable without changing its graph, its provider or
its key. It is a peer dependency, so the plain SDK import pulls none of it in;
install LangChain alongside the client when you want it:

```sh
npm install @salvor-run/client langchain @langchain/core zod
```

The LangChain extra is in the next release of `@salvor-run/client`; it is not
on npm yet, so until that release ships, install the SDK from a checkout of
this repository instead (`npm install <path-to-checkout>/sdks/typescript
langchain @langchain/core zod`), and come back to the line above once it is.
That checkout install works the same way from any directory, so an app of
your own outside this repository installs against it exactly as
`examples/langchain` does.

Then add one middleware to the agent you already have:

```ts
import { createAgent } from "langchain";
import { SalvorClient } from "@salvor-run/client";
import { salvorMiddleware } from "@salvor-run/client/langchain";

const salvor = new SalvorClient("http://127.0.0.1:8080");

const agent = createAgent({
  model,
  tools,
  middleware: [salvorMiddleware({ client: salvor })],
});

await agent.invoke(
  { messages: [{ role: "user", content: "how is ORD-7781?" }] },
  { configurable: { thread_id: "order-7781" } },
);
```

### Catching what the middleware throws

Everything this middleware refuses by name is a `SalvorMiddlewareError`
carrying a `code` you can branch on, and the server's own refusal (a
`SalvorApiError`) on `cause` when there was one. Reach it with `salvorError(e)`,
which returns the middleware error or `undefined` when the failure was not
salvor's at all:

```ts
import { salvorError } from "@salvor-run/client/langchain";

try {
  await agent.invoke(input, { configurable: { thread_id: "order-7781" } });
} catch (e) {
  const refusal = salvorError(e);
  if (!refusal) throw e; // the app's own error, unchanged

  switch (refusal.code) {
    case "lease_held": {
      // Another driver has this thread right now. It usually finishes and releases
      // well before its hold lapses, so poll every couple of seconds instead of
      // sleeping out the whole window.
      //
      // If the holder was a process that crashed rather than one that is still working,
      // nothing can release its lease from outside: it lapses on the timer, or sooner if
      // the run ended at a dangling write and a person resolves it over HTTP, which clears
      // the lease as well.
      const deadline = Date.now() + (refusal.lapsesInSeconds ?? 1) * 1000;
      while (Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 2000));
        try {
          return await agent.invoke(input, { configurable: { thread_id: "order-7781" } });
        } catch (retry) {
          if (salvorError(retry)?.code !== "lease_held") throw retry;
        }
      }
      return agent.invoke(input, { configurable: { thread_id: "order-7781" } });
    }
    case "tool_needs_resolution":
      // A `trust_completion = false` tool ran and is waiting on a person.
      await alertOps(refusal); // it is a ToolNeedsResolution: run, seq, tool, output, key
      break;
    case "open_intent":
      // The log ends at a call that was never completed. Settle it first.
      await alertOps(refusal);
      break;
    default:
      throw e;
  }
}
```

The helper exists because the two shapes an error arrives in are not the same
shape. `createAgent` wraps an error thrown inside a graph node in its own
`MiddlewareError`, copying the `name` and `message` but keeping the real
instance only on `.cause`: that is how `ToolNeedsResolution` and the tool-side
refusals (`tool_undeclared`, `open_intent`, `lease_lost`) reach you. An error
thrown from `beforeAgent`, before any node runs, arrives **bare**: that is
`lease_held`, `thread_finished`, `thread_abandoned`, `thread_id_missing`,
`thread_id_invalid` and `run_exists`. A `catch` that checks only `e.cause`
misses the second group and a `catch` that checks only `instanceof` misses the
first; `salvorError` walks the `cause` chain and covers both. Note that a
middleware error now carries a `cause` of its own (the `SalvorApiError`
underneath), so reaching one level in by hand can land on the server's error
rather than the middleware's.

The codes:

| `code` | What happened |
| --- | --- |
| `lease_held` | Another driver holds this thread's run right now. `lapsesInSeconds` says how long their hold has left. |
| `lease_lost` | This invoke stopped being the driver: its token is no longer the current lease, or the lease went twice in one invoke. |
| `reopen_refused` | The lease was lost and the server would not hand the run back at all. The log is intact; this server is not the one to drive it from. |
| `run_exists` | The thread maps to a run id salvor's other, server-driven mode already started. Give the thread an id of its own. |
| `thread_finished` | `finishThread` closed this thread's run, and a completed run takes no more events. |
| `thread_abandoned` | Somebody recorded a terminal `RunAbandoned` on this thread's run (`POST /v1/runs/{id}/abandon`, or `salvor abandon`). Nothing was replayed and nothing ran; give the next task a new thread id. |
| `thread_never_invoked` | `finishThread` was asked to close a thread that has no run yet. |
| `thread_id_missing` | The invoke passed no `thread_id`. |
| `thread_id_invalid` | It passed one that is not a non-empty string. The message says what arrived. |
| `tool_undeclared` | The tool has no client-tool declaration on this server. |
| `tool_needs_resolution` | The tool ran and its operator settles such a call by hand. This one is the typed `ToolNeedsResolution`, with the result on `.output`. |
| `tool_returned_command` | A tool answered with a LangGraph `Command`, which is control flow, not a result to record. |
| `call_unranked` | The call's id is not among the ones the model's last recorded turn listed, so its position in the run cannot be pinned: either that turn was never recorded, or a middleware ahead of this one changed the call's id. |
| `tool_failed` | The log already holds a recorded failure at this position: an earlier invoke's `effect = "write"` tool body threw, this middleware reported it, and salvor settled the call on it. Carries `seq`, the position it was recorded at. Fails the same way on every replay and on every fork of this thread, because a fork opens the same write under the same key: give the task a new thread id. |
| `open_intent` | The log holds a call recorded as requested and never completed. Settle it and invoke again. |
| `unreadable_record` | A model answer is missing or does not read back as one. |
| `bad_request` | The server's own refusal, unwrapped: an intent's input failed the declared `input_schema` before the tool ran, or a reported tool output failed the declared `output_schema`. |
| `client_completion_refused` | The server's own refusal: a `require_equal` field's reported value differed from the intent's, or a client tried to close a call salvor itself already performed, or the declaration says `trust_completion = false` (for a reported result or a reported failure alike), or the declaration has no `output_schema` to check a reported result against. |

A fork is not among them: leaving the recorded path is not an error (see
`onFork` below). `ToolNeedsResolution` is still its own class, so
`salvorError(e) instanceof ToolNeedsResolution` works as well as its code does.
The last two rows are the control plane's own refusal, unwrapped rather than
translated: `cause` on that error is the `SalvorApiError` underneath, and the
server can answer with codes beyond those two (`divergence`, `unknown_tool`,
and so on) that arrive exactly the same way.

### Try it without a key

The client-driven tool below needs a declaration before the model can call
it: its effect class, its schemas and its idempotency key are the operator's,
never the middleware's, and they come from a client-tool declaration the
server was started with. Skip this and the invoke is refused with
`tool_undeclared`, carrying the server's own words underneath:

```
unknown_tool: no client-performed tool named `lookup_order` is declared on this
server; declarations are loaded by the operator (`salvor serve --client-tool
<FILE>`) and are never registered over HTTP
```

Save this as `lookup-order.toml`:

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

The rest below runs the same middleware end to end with no provider key and no
network: a scripted model stands in for whatever provider your app actually
uses. Save it as `try-salvor.ts` next to a `package.json` that says
`"type": "module"` and run it with Node 22.18 or newer, which strips the types
itself:

```sh
node try-salvor.ts
```

The `"type": "module"` is not optional: this file is ESM, and without it Node
reads it as CommonJS and stops at `Cannot use import statement outside a
module`. A `package.json` with the four dependencies, and one `npm install`, is
all it takes:

```json
{
  "type": "module",
  "dependencies": {
    "@salvor-run/client": "^0.9.2",
    "@langchain/core": "^1.2.9",
    "langchain": "^1.5.10",
    "zod": "^3.25.0"
  }
}
```

The LangChain extra is in the next release of the SDK, not yet in `0.9.2`
above: until it is on npm, point that first dependency at a checkout of this
repository instead (`npm install <path-to-checkout>/sdks/typescript` in place
of the registry line, keeping the other three), and switch back to the
registry version once the release with the extra is out.

```ts
import { createAgent, tool } from "langchain";
import { AIMessage, type BaseMessage } from "@langchain/core/messages";
import { BaseChatModel } from "@langchain/core/language_models/chat_models";
import type { ChatResult } from "@langchain/core/outputs";
import { z } from "zod";
import { SalvorClient } from "@salvor-run/client";
import { salvorMiddleware } from "@salvor-run/client/langchain";

// A hand-rolled model, not one of `@langchain/core/utils/testing`'s fakes:
// `FakeStreamingChatModel` answers every turn with its first response, so a
// tool-calling agent loops on the same tool forever, and
// `FakeToolCallingModel`'s `bindTools` rebuilds itself on every call, which
// silently drops anything attached to the instance.
class ScriptedModel extends BaseChatModel {
  private readonly script = [
    {
      content: "looking that up",
      toolCalls: [{ name: "lookup_order", args: { order_id: "ORD-7781" }, id: "call-1" }],
    },
    { content: "Order ORD-7781 is paid, 4200 cents." },
  ];

  constructor() {
    super({});
  }

  _llmType(): string {
    return "scripted";
  }

  bindTools(): this {
    return this;
  }

  async _generate(messages: BaseMessage[]): Promise<ChatResult> {
    const turn = messages.filter((m) => m.getType() === "ai").length;
    const step = this.script[Math.min(turn, this.script.length - 1)];
    const message = new AIMessage({
      content: step.content,
      tool_calls: step.toolCalls?.map((call) => ({ ...call, type: "tool_call" as const })),
    });
    return { generations: [{ text: step.content, message }] };
  }
}

const lookupOrder = tool(
  async ({ order_id }: { order_id: string }) => ({
    order_id,
    status: "paid",
    total_cents: 4200,
  }),
  {
    name: "lookup_order",
    description: "Look up an order that has already been placed.",
    schema: z.object({ order_id: z.string() }),
  },
);

const salvor = new SalvorClient("http://127.0.0.1:8080");

const agent = createAgent({
  model: new ScriptedModel(),
  tools: [lookupOrder],
  middleware: [salvorMiddleware({ client: salvor })],
});

const answer = await agent.invoke(
  { messages: [{ role: "user", content: "how is ORD-7781?" }] },
  { configurable: { thread_id: "order-7781" } },
);

console.log(answer.messages.at(-1)?.content);
```

It prints `Order ORD-7781 is paid, 4200 cents.` Run it a second time and it
prints the same thing without calling the model or the tool at all: the run is
already recorded, and the invoke replays it. That second run works immediately
rather than being refused for a minute because the first one handed the
thread's lease back on its way out.

To see what was recorded, read the run's log with the CLI, over the same store
the server is using (`salvor.db` in the working directory unless `salvor serve
--store <path>` said otherwise):

```sh
salvor history <run> --store ./salvor.db
```

The run id for a thread id is `await runIdForThread("order-7781")` (it hashes
asynchronously), exported from `@salvor-run/client/langchain`.

When you are done, the whole experiment is three files: delete `salvor.db`
along with its `salvor.db-wal` and `salvor.db-shm` side files, which SQLite
writes beside it, and the store is gone.

A real app replaces `ScriptedModel` with its provider model (`ChatAnthropic`,
`ChatOpenAI`, and so on) and nothing else changes.

### What gets recorded

Every model call and every tool call the agent makes, each as the intent and
completion pair salvor records for any run. The model call is still LangChain's:
the middleware opens the intent with a content hash of the request, lets the
call through to whatever provider and key the app configured, and records the
answer and its token counts. Salvor never sees the request and never holds the
key, which is why the recorded `ModelCallRequested` says `performed_by: "client"`.
A tool call is the same shape, with the operator's derived idempotency key on
the intent, so a retried write presents the key the first attempt used and the
provider collapses the duplicate. Pass `recordPrompts: true` to store the
request body on the intent as well, for an inspector to show; replay never
reads it, because the correlation key is the hash alone.

To read a thread's recorded log back: `GET /v1/client-runs/{id}/log` serves it
(no drive token needed) for any run the log marks as client-driven, whether
its driver is mid-invoke, released the lease when the invoke ended, went quiet
until the lease lapsed, or was driving before the server last restarted. The
same log is also readable with `salvor history <run> --store <path>` against
the store or `GET /v1/runs/{id}/events` against the server, both of which read
the durable log rather than the lease registry.

Model responses, tool arguments and tool results are always recorded,
whatever `recordPrompts` is set to; that flag only decides whether the
request body joins them. What a recorded payload can hold, and what that
means for personal data inside it, is spelled out in
[SECURITY.md](../../SECURITY.md#what-the-event-log-records)'s "What the event
log records". And because a thread's run stays open until `finishThread`
closes it, an open thread keeps every one of those payloads for as long as
the store file exists: salvor has no retention of its own, which
[docs/OPERATIONS.md](../../docs/OPERATIONS.md#retention)'s "Retention"
section covers in full.

### What replay means

Invoking the same thread again re-opens the same run and walks the recorded
positions. Where the log already holds an answer, the middleware returns it and
the provider is not called; where the log already holds a tool result, the tool
body does not run. A thread that ran to the end and is invoked again a second
time costs nothing at all and returns the same final message.

A replayed answer says so. It carries `response_metadata.salvor` with
`{ replayed: true, seq, run }`, and under `agent.stream` it arrives as one whole
message rather than a re-tokenised imitation of the original stream. The tokens
happened once, on the invoke that paid for them, and nothing here pretends
otherwise.

A run that died between a tool's intent and its completion is the case the whole
design is for. The log ends at the intent, which is exactly what an unfinished
write looks like, and the next invoke replays everything before it for free,
performs that one call again under the same derived key, and records the
completion. One intent, one completion, no second charge. A model call the
provider itself failed on works the same way: the intent is already recorded
by the time the provider throws, nothing records a completion for the failed
attempt, and the next invoke meets that same open intent and performs the call
again.

Parallel tool calls in one model turn are serialised rather than refused. A
turnstile inside the middleware admits one open intent per run at a time,
ranked by each call's position in the model's own `tool_calls`, never by the
order the calls happen to arrive at the hook, so they are recorded in the
model's order and replay at the same positions on a later invoke.

### Finishing a thread

A thread's run stays open by default: replay only checks whether a position is
already recorded, never whether the whole thread is done. Call
`finishThread(client, threadId)` once a thread genuinely has no more turns
coming; it records `RunCompleted` with the last AI message's content, or with
whatever value you pass as a third argument, and closes the run for good.
After that, invoking the same thread again is refused, naming the thread. It
refuses the same way, naming the run, when the log already ends at an open
intent: settle that call first, then finish the thread.

### The thread id is the run id

A LangGraph `thread_id` that is already a UUID is used as the salvor run id
unchanged, so an application whose thread ids are UUIDs can look a run up by the
id it already holds. Any other thread id is hashed into one: SHA-256 of the
thread id, the first 16 bytes taken, with the version nibble set to 8 (RFC
9562's custom version, which is what a hash-derived id honestly is) and the
variant bits set. The mapping is stable forever and the same on every machine.
Pass `threadIdToRunId` to replace it when your two ids live in a table
somewhere. Invoking without a thread id is an error, not a silent pass-through:
without one there is nothing for a later invoke to resume.

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

The middleware sends the tool's name and the arguments the model produced, and
nothing else. A tool with no declaration is refused, and the error names the
tool and the declaration it needs rather than quietly recording the call as a
harmless read. `trust_completion = true` with an `[output_schema]` is what lets
the middleware record what the tool returned; a declaration without them leaves
every call for that tool to be settled by hand with `resolve`, once someone has
verified it externally. `examples/client-tools/refund-card.toml` is the fully
commented version of the same file.

A call's idempotency key is positional by default (one identity per position
in the run) unless the declaration names `idempotency_key` fields, in which
case the key is derived from those fields' values instead. With fields
declared, two calls in the same run whose named fields carry the same values
share one key, and the second one settles from the first's recorded result
rather than performing the call again. So a model that emits the same write
twice in one turn runs it once when the declared fields match between the two
calls, and twice when the declaration names no fields at all, unless the
provider underneath happens to dedupe such a retry on its own. Naming fields
is naming what makes two calls the same call, so check the list against what
actually varies between calls you mean to keep distinct: a field left out of
it is a field two such calls may differ on and still collapse into one.

The recorded output is the tool's own result, which is what the output schema
describes. LangChain builds a tool message by stringifying whatever the tool
returned, so the result is recovered by parsing that content back when the parse
round-trips exactly; when it does not, the content is recorded as the string it
is, and an object schema will refuse it and say so.

### Tools a person must confirm

A tool declared `trust_completion = false` still runs: refusing to run it fixes
nothing, since not trusting its own report is a different decision from not
sending the payment. What changes is what happens next. Salvor refuses a
client completion for such a tool outright (`403 client_completion_refused`),
whatever it says, so the middleware never tries to post one. It throws
`ToolNeedsResolution` instead, carrying `{ run, seq, thread, tool, output, key }`,
and the invoke stops there rather than let that refusal tear through LangGraph
after the call has already happened.

The run is left at `seq`, an intent with no completion, the same shape a crash
between intent and completion leaves. Settle it by hand once you have checked
what the tool actually did. There are two ways in, and the error message names
both, because the person reading it usually has one of them and not the other:

```sh
# against the running server, from anywhere that can reach it
curl -X POST http://127.0.0.1:8080/v1/runs/<run>/resolve \
  -H 'content-type: application/json' \
  -d '{"output": {"provider_transfer_id": "ptx-...", "status": "succeeded"}}'

# or against the store file, from a shell on the machine that holds it
salvor resolve <run> --store <path> --output '<json the tool returned>'
```

A container running this agent often has no store path at all, only the
server's URL, which is why the HTTP endpoint is named first. The two differ in
one way worth knowing: the HTTP resolve clears the run's lease along with the
resolution, so the thread re-opens at once, while `salvor resolve` writes the
store directly and cannot reach a live server's memory, so a lease held there
survives it and lapses on its own (at most the TTL, 60 seconds by default).
`driver.resolve(output)` goes through the server too.

`--store` has to name the SERVER's store file; this middleware only ever
speaks HTTP to that server, so it has no way to know that path itself, which
is why `ToolNeedsResolution.message` prints a `--store <the server's store>`
placeholder there rather than a guess. Invoke the thread again and the
resolved output replays in the call's place; invoke it again before resolving
and it meets the same open intent, refused by name (`open_intent`), naming the
same fix.

### The honest limits

This is a recorded effect ledger with exactly-once writes and salvor's budgets,
under LangGraph's orchestration. It is not replay of the graph. LangGraph still
owns the clock, the randomness and the branch order, and salvor sees the calls
rather than the decisions between them. A graph that branches on `Date.now()`,
a tool whose result differs between runs, or a genuinely new turn down an old
thread all mean the log holds a recorded position that does not match what the
invoke is actually doing this time. When that happens the middleware stops
replaying and appends the rest of the invoke at the end of the log, so the fork
is recorded rather than lost. Key order is no longer one of these causes: the
middleware writes every tool result in canonical, sorted-key JSON, so the live
bytes and the replayed bytes always match, and the model sees its tool results
with sorted keys either way.

Every AI message the middleware returns carries `response_metadata.salvor`,
saying which of the three things happened to it: `{ replayed: true, seq }`
when the answer came from the log, `{ live: true, seq }` when it was a real
call on a path the log still agrees with, and `{ forked: { at, thread, run } }`
on every message from the point the invoke actually forked onward. A fork also
calls `onFork` once per invoke with a `SalvorForkNotice`, naming the log
position it forked at (`at`), the thread (`thread`), the run (`run`), and the
sentence the default handler warns with (`message`). That `at` is the first
recorded position that no longer matches, so when several things changed
between invokes it points at the earliest of them, not necessarily the one you
meant. The default is `console.warn`, and it is the hook to point at your own
logging instead.

The one case it refuses is a log whose last event is a call that never
completed: settle that first, then invoke again.

A thread is one task. Re-invoking it replays it; sending a genuinely new turn
down the same thread is a fork by the rule above, and pays for the calls the new
turn makes. Give a new task a new thread id.

The run's lease lives in the server's memory, not on disk. A lease is HELD,
not handed to whoever asks last: while a driver's lease on a thread's run is
current, a second instance invoking that same thread is refused at once,
before it runs a single tool, naming the thread and how many seconds until the
hold lapses on its own (`lease_held`, carrying `lapsesInSeconds`). A lease
taken out from under an active invoke mid-step, by contrast, means a second
driver is live on the thread RIGHT NOW; that is refused too (`lease_lost`,
`invalid_drive_token` on the wire), by the same one-driver message, and neither
case is ever retried by re-opening, because there is no order in which two live
drivers can both be right about what comes next.

An invoke does not keep the lease a moment longer than it is driving:

- **It is released when the invoke ends**, on the success path and on every
  error path alike (a tool body that threw, a `ToolNeedsResolution` stop, a
  LangChain error on its way out), so the next process to invoke the thread
  takes it on its very next request rather than waiting out a TTL. `finishThread`
  releases too. The one thing never released is a lease this invoke does not
  hold: `lease_held` and `lease_lost` leave the other driver's hold alone.
- **It is kept alive during long steps.** While a tool body or a live model
  call is running, nothing else presents the drive token, so the middleware
  beats a heartbeat every third of the TTL (it learns the TTL from the
  server's own answer) until the step returns. A tool that takes ten minutes
  keeps the run it never left.
- **It is cleared by an HTTP resolve.** `POST /v1/runs/{id}/resolve` says the
  driver that opened the dangling write is gone, so the lease it left behind
  goes with the resolution and the thread re-opens at once. `salvor resolve`
  on the command line writes the store directly, cannot reach the server's
  memory, and so leaves the lease to lapse on its own.
- **It lapses after a crash.** A process that dies says nothing, and a lease
  nobody refreshes for the TTL (60 seconds by default,
  `SALVOR_CLIENT_LEASE_TTL_SECS`) stops holding the run. That is the safety
  net, not the ordinary way a drive ends.

If salvor itself restarts mid-invoke, none of this applies: the lease registry
dies with the process but the log does not, so the middleware notices its open
run is gone (`unknown_run`), re-opens it once, and continues from the log as
if the restart had not happened.

`wrapToolCall` exists only inside `createAgent`. A hand-built `StateGraph` that
calls tools from its own node has no hook for the middleware to sit in, so such
a graph gets model recording only and its tool calls stay outside the ledger.

Changing a tool's schema or a model's settings mid-flight changes the request
hash, which is the same fork as above and is meant to be: the question is not
the one the recorded answer was an answer to.

A tool body can read its own recorded idempotency key with `currentToolCall()`,
returning `{ key, seq, runId, tool }` for the call `wrapToolCall` is recording
right now. `key` is the value salvor already derived for this call: positional,
a hash of `(run, seq, tool)`, unless the declaration names `idempotency_key`
fields, in which case it is derived from the run, the tool and those fields
instead, with no `seq` in it. Either way it is the same one sitting on the
intent; hand it straight to the tool's own provider as that provider's
idempotency token, so a retried write and the first attempt present the same
one. It works only from inside a tool body a
live `wrapToolCall` is running, and only in Node; called from anywhere else it
returns `undefined`, and the middleware keeps recording and replaying exactly
as it does without it.

Be plain about what that key does and does not buy. Salvor guarantees two
things: the call is recorded exactly once in the log, and the key it derives
for a given call is stable across every attempt at it. It does not guarantee
that the tool's body ran once. If the process dies between the provider
answering "done" and salvor recording the completion, the log still ends at
the intent, and the next invoke does the only honest thing it can: it runs the
body again, at the same seq, under the same key. Whether that second run
charges the card a second time is the provider's decision, made on that key.
So the key has to reach the provider as its idempotency token, not sit in a log
line: pass it, and the duplicate collapses into the first write; leave it out,
and the write can happen twice, with salvor's log recording it once either way.

That dangling-intent case is what a process dying mid-write leaves behind. A
tool body that instead throws is recorded as the call's failure only when the
tool is declared `effect = "write"`: the middleware posts the thrown message
as that failure, the same way salvor itself records a native tool's exhausted
retries, and the invoke rejects with the tool's own error. Re-invoking the
thread does not run that body again; it meets the recorded failure and rejects
with `tool_failed` on the spot, carrying the message that was recorded. A
permanently failing input fails the same way on every replay from here on, and
on every fork of this thread too, because a fork opens the same write under
the same key: only a new thread id escapes it. A `read` or `idempotent` tool
that throws posts nothing at all: its intent stays open exactly as if the call
had never returned, and the next invoke simply performs it again, which is why
a transient error on a lookup does not wedge the thread the way a failed
write's record would. A tool declared `trust_completion = false` is the one
exception on the write side, because it may not report even a failure on its
own say-so any more than it may report a result: it stops with the same open-intent
refusal `ToolNeedsResolution` gives a successful untrusted call, and a person
settles it by hand.

## Graphs

A graph document composes `agent`, `tool`, `gate`, `branch`, `map`, `fold`, and
`delay` nodes into an authored control flow: an acyclic set of steps submitted
once, hashed, and run by that hash exactly as an agent definition is.
`GraphBuilder` mirrors the seven node kinds as typed methods, so a document
gets editor completion and compile-time typing instead of hand-written JSON;
the semantic checks (referential integrity, acyclicity) live server-side, on
submit or `salvor graph validate`.

An `agent` node references an agent by its content hash, never by path. Get
one from `registerAgent`, which accepts a TOML string and returns the hash,
computed server-side and validated as a side effect; with no server running,
`salvor agent hash <FILE>` prints the same hash from the command line.

```ts
import { GraphBuilder } from "@salvor-run/client";

const draftSchema = {
  type: "object",
  properties: { draft: { type: "string" } },
  required: ["draft"],
};

const graph = new GraphBuilder()
  .agent("research", researchAgentHash, { outputSchema: draftSchema })
  .agent("review", reviewAgentHash, {
    inputSchema: draftSchema,
    outputSchema: draftSchema,
  })
  .gate(
    "approve",
    {
      type: "object",
      properties: { approved: { type: "boolean" } },
      required: ["approved"],
    },
    { prompt: "Approve this draft for publication?" },
  )
  .tool("publish", "http_post", {
    input: { body: "approve.draft", url: "config.publish_url" },
  })
  .edge("research", "review")
  .edge("review", "approve")
  .edge("approve", "publish")
  .build();
```

`example/build_graph.ts` builds this same research, review, gate, publish flow
and prints the document; pipe it into `salvor graph validate /dev/stdin` for
the semantic checks the builder itself does not run.

Submit the built document, then start a run from the hash it returns:

```ts
const { graph: graphHash } = await client.submitGraph(graph); // -> GraphSubmitted
const runId = await client.startGraphRun(graphHash, { topic: "..." });
const projection = await client.getRunGraph(runId); // -> GraphProjection
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
`state: "suspended"` and the schema (`reason`, `inputSchema`) the approval must
satisfy; `resume` continues it with that approval, the same call an ordinary
agent run's park uses:

```ts
const result = await client.resume(runId, { approved: true }); // -> ResumeResult
```

A parked graph run continues through that same call. The run's log recorded
only the graph's hash, not the document itself, so resume takes the document
again: the server looks it back up by that hash before it can re-drive the
walk, which means resuming depends on the document still being resolvable in
memory, the same restart caveat submission carries above.

Forking continues a run from a node boundary into a new run without touching
the origin: `client.forkRun(runId, "review")`, previewed first with
`client.previewFork`, and listed per run with `client.listForks`. See the
`forkRun` doc comment in `src/client.ts` for the write-replay-hazard refusal a
fork guards against.

## Runnable example

`example/agent.toml` is a model-only agent that answers one question. It is the
TypeScript mirror of `examples/web-research`, driven over the control plane
instead of the CLI. Start a server with a key on its environment, build the
package, then run the script:

```sh
npm install -g @salvor-run/cli  # or: cargo install salvor-cli

ANTHROPIC_API_KEY=sk-ant-... \
    salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db &

cd sdks/typescript
npm install && npm run build
node --experimental-strip-types example/answer.ts http://127.0.0.1:8080
```

It registers the agent, starts a run, streams every event to completion, and
prints the final answer, the event count, and the token usage.
