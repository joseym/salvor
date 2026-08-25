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

### Try it without a key

The client-driven tool below needs a declaration before the model can call
it: its effect class, its schemas and its idempotency key are the operator's,
never the middleware's, and they come from a client-tool declaration the
server was started with. Skip this and the first call fails with
`unknown_tool: no client-performed tool named "lookup_order" is declared on
this server`.

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
uses.

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
completion. One intent, one completion, no second charge.

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

The middleware sends the tool's name and the arguments the model produced, and
nothing else. A tool with no declaration is refused, and the error names the
tool and the declaration it needs rather than quietly recording the call as a
harmless read. `trust_completion = true` with an `[output_schema]` is what lets
the middleware record what the tool returned; a declaration without them leaves
every call for that tool to be settled by hand with `resolve`, once someone has
verified it externally. `examples/client-tools/refund-card.toml` is the fully
commented version of the same file.

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
what the tool actually did: `salvor resolve <run> --output '<json the tool
returned>'`, the Inspector, or `driver.resolve(output)`. Invoke the thread again
and the resolved output replays in the call's place; invoke it again before
resolving and it meets the same open intent, refused by name, naming the same
fix.

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
calls `onFork` once per invoke, naming the thread, the run and the seq it
forked at; the default is `console.warn`, and it is the hook to point at your
own logging instead.

The one case it refuses is a log whose last event is a call that never
completed: settle that first, then invoke again.

A thread is one task. Re-invoking it replays it; sending a genuinely new turn
down the same thread is a fork by the rule above, and pays for the calls the new
turn makes. Give a new task a new thread id.

The run's lease lives in the server's memory, not on disk. If salvor itself
restarts mid-invoke, the middleware notices its open run is gone, re-opens it
once, and continues from the log as if the restart had not happened. If a
second instance of your app invokes the same thread at the same time, the
later one takes the lease and the earlier one fails, naming the thread: one
driver per thread at a time, and the newest caller wins it.

`wrapToolCall` exists only inside `createAgent`. A hand-built `StateGraph` that
calls tools from its own node has no hook for the middleware to sit in, so such
a graph gets model recording only and its tool calls stay outside the ledger.

Changing a tool's schema or a model's settings mid-flight changes the request
hash, which is the same fork as above and is meant to be: the question is not
the one the recorded answer was an answer to.

A tool body can read its own recorded idempotency key with `currentToolCall()`,
returning `{ key, seq, runId, tool }` for the call `wrapToolCall` is recording
right now. `key` is the value salvor already derived for `(run, seq, tool)`,
the same one sitting on the intent; hand it straight to the tool's own
provider as that provider's idempotency token, so a retried write and the
first attempt present the same one. It works only from inside a tool body a
live `wrapToolCall` is running, and only in Node; called from anywhere else it
returns `undefined`, and the middleware keeps recording and replaying exactly
as it does without it.

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
