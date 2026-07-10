# @salvor/client (TypeScript)

A thin TypeScript client for the Salvor control plane.

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
npm install      # only a dev dependency: typescript
npm run build    # tsc -> dist/
```

Then in your project:

```ts
import { SalvorClient } from "@salvor/client";
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
import { NeedsReconciliationError } from "@salvor/client";

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

## Runnable example

`example/agent.toml` is a model-only agent that answers one question. It is the
TypeScript mirror of `examples/web-research`, driven over the control plane
instead of the CLI. Start a server with a key on its environment, build the
package, then run the script:

```sh
# from the repository root
cargo build --bin salvor

ANTHROPIC_API_KEY=sk-ant-... \
    ./target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db &

cd sdks/typescript
npm install && npm run build
node --experimental-strip-types example/answer.ts http://127.0.0.1:8080
```

It registers the agent, starts a run, streams every event to completion, and
prints the final answer, the event count, and the token usage.
