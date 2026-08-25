/**
 * Proves the LangChain middleware against the real `salvor serve`, with a
 * scripted model and no provider key anywhere.
 *
 * Every case here drives an ordinary `createAgent` app. Nothing in the app
 * knows about salvor: the graph, the tools and the model are what a team would
 * already have written, and the middleware is the one line added to them. What
 * the cases check is what that line buys. A first invoke pays for the model
 * calls and runs the tools; a second invoke of the same thread pays for none of
 * it, executes none of it, and returns the same final message. A crash between
 * a tool's intent and its completion leaves the log saying exactly that, and the
 * next invoke picks the call up where it stopped, under the same derived key.
 *
 * Three of the cases here are about what a resume gets wrong when nobody looks.
 * A tool result whose keys are not in alphabetical order has to reach the model
 * as the same text live and replayed, or the model call that reads it forks the
 * thread on key order alone. A fork, when one does happen, has to be visible:
 * every message from it carries a marker saying so, and the invoke says it once
 * out loud. And the drive lease a run is opened with is not something an invoke
 * owns for its own lifetime, so a run taken by another driver mid-invoke is
 * taken back once, a run taken twice is refused by name, a server that restarts
 * under an invoke is survived under a fresh lease the restarted server mints,
 * and a run id that was never client-driven to begin with is refused by name
 * instead of adopted.
 *
 * The model is a small `BaseChatModel` scripted turn by turn rather than one of
 * the fakes in `@langchain/core/utils/testing`. Those cannot script a
 * multi-turn tool-calling agent (`FakeStreamingChatModel` answers every turn
 * with its first response, so an agent loops on the same tool forever), and
 * `FakeToolCallingModel`'s `bindTools` rebuilds itself and drops any counter or
 * callback attached to it, which is precisely the thing these cases have to
 * count. Both facts are checked in this file's own script: no key, no network,
 * one counter that survives binding.
 *
 * The suite skips when `target/debug/salvor` is not built.
 */

import { deepStrictEqual, match, notStrictEqual, ok, rejects, strictEqual } from "node:assert";
import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer as netServer } from "node:net";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { existsSync, mkdtempSync, rmSync } from "node:fs";

import { createAgent, createMiddleware, tool } from "langchain";
import { AIMessage, type BaseMessage } from "@langchain/core/messages";
import { BaseChatModel } from "@langchain/core/language_models/chat_models";
import type { ChatResult } from "@langchain/core/outputs";
import { z } from "zod";

import { ClientRunDriver, SalvorClient, type SalvorEvent } from "../dist/index.js";
import {
  RunTape,
  SalvorMiddlewareError,
  type SalvorForkNotice,
  ToolNeedsResolution,
  currentToolCall,
  finishThread,
  runIdForThread,
  salvorMiddleware,
} from "../dist/langchain/index.js";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");
const SALVOR = resolve(repoRoot, "target", "debug", "salvor");
/**
 * An `[llm] base_url_env` name for the one test that starts a real
 * server-driven run. Pointed at a loopback port nothing listens on, so the
 * background driver's model call fails at once, on no network, instead of
 * reaching the public Anthropic endpoint with no key.
 */
const SERVER_DRIVEN_MODEL_BASE_URL_ENV = "SALVOR_TS_TEST_MODEL_BASE_URL";
const DECLS = [
  resolve(here, "client-tools", "lookup-order.toml"),
  resolve(here, "client-tools", "stamp-ledger.toml"),
  resolve(here, "client-tools", "track-parcel.toml"),
  resolve(here, "client-tools", "record-delivery.toml"),
  resolve(here, "client-tools", "wire-payout.toml"),
];

// -- the scripted model ------------------------------------------------------

interface Turn {
  content: string;
  toolCalls?: { name: string; args: Record<string, unknown>; id: string }[];
}

/**
 * A model that answers turn by turn from a script and counts how often it was
 * actually asked. The turn is chosen from the history (how many AI messages the
 * conversation already holds) rather than from the counter, so a replayed
 * invoke that skips the model entirely still lines up with the script.
 */
class ScriptedModel extends BaseChatModel {
  readonly turns: Turn[];
  readonly calls: { count: number };
  private bound: unknown[] = [];

  constructor(turns: Turn[], calls: { count: number } = { count: 0 }) {
    super({});
    this.turns = turns;
    this.calls = calls;
  }

  _llmType(): string {
    return "scripted-fake";
  }

  _combineLLMOutput(): never[] {
    return [];
  }

  bindTools(tools: unknown[]): ScriptedModel {
    const next = new ScriptedModel(this.turns, this.calls);
    next.bound = [...this.bound, ...tools];
    return next;
  }

  async _generate(messages: BaseMessage[]): Promise<ChatResult> {
    const index = Math.min(
      messages.filter((m) => m.getType() === "ai").length,
      this.turns.length - 1,
    );
    const turn = this.turns[index];
    this.calls.count += 1;
    const message = new AIMessage({
      content: turn.content,
      id: `scripted-${index}`,
      tool_calls: turn.toolCalls?.map((call) => ({ ...call, type: "tool_call" as const })),
      usage_metadata: { input_tokens: 11, output_tokens: 5, total_tokens: 16 },
    });
    return { generations: [{ text: turn.content, message }], llmOutput: {} };
  }
}

// -- the tools ---------------------------------------------------------------

/** How often each tool body actually ran, and how many ran at once. */
const ran = {
  lookup: 0,
  stamp: 0,
  track: 0,
  deliver: 0,
  payout: 0,
  concurrent: 0,
  peakConcurrent: 0,
};
/** Set to make the next `stamp_ledger` body throw, standing in for a crash. */
let stampCrashes = false;
/** What `currentToolCall()` reported the last time `lookup_order`'s body ran. */
let capturedCall: ReturnType<typeof currentToolCall> | undefined;
/**
 * Run once, from inside the next `lookup_order` body, then cleared.
 *
 * This is how the lease cases reach in mid-invoke: a tool body runs while the
 * tape holds an open intent, so whatever happens here happens between the
 * intent and the completion, which is exactly where a lost lease hurts.
 */
let midToolCall: (() => Promise<void>) | undefined;

function enter(): void {
  ran.concurrent += 1;
  ran.peakConcurrent = Math.max(ran.peakConcurrent, ran.concurrent);
}

const lookupOrder = tool(
  async ({ order_id }: { order_id: string }) => {
    enter();
    try {
      capturedCall = currentToolCall();
      await new Promise((r) => setTimeout(r, 15));
      ran.lookup += 1;
      const interrupt = midToolCall;
      midToolCall = undefined;
      if (interrupt) await interrupt();
      return { order_id, status: "paid", total_cents: 4200 };
    } finally {
      ran.concurrent -= 1;
    }
  },
  {
    name: "lookup_order",
    description: "Look up an order that has already been placed.",
    schema: z.object({ order_id: z.string() }),
  },
);

const stampLedger = tool(
  async ({ order_id, note }: { order_id: string; note: string }) => {
    enter();
    try {
      ran.stamp += 1;
      if (stampCrashes) throw new Error("the ledger writer died mid-call");
      return { order_id, entry_id: `entry-${note.length}` };
    } finally {
      ran.concurrent -= 1;
    }
  },
  {
    name: "stamp_ledger",
    description: "Write one line into the order's ledger.",
    schema: z.object({ order_id: z.string(), note: z.string() }),
  },
);

const sendEmail = tool(async () => ({ sent: true }), {
  name: "send_email",
  description: "Send an email. Deliberately never declared to salvor.",
  schema: z.object({ to: z.string() }),
});

/**
 * The one tool in this suite declared `trust_completion = false`: the body
 * runs and returns a real result, but the middleware may never report it.
 */
const wirePayout = tool(
  async ({ payee, amount_cents }: { payee: string; amount_cents: number }) => {
    ran.payout += 1;
    return { provider_transfer_id: `ptx-${payee}`, status: "succeeded", amount_cents };
  },
  {
    name: "wire_payout",
    description: "Send a bank transfer to a payee whose card refund is not available.",
    schema: z.object({ payee: z.string(), amount_cents: z.number() }),
  },
);

/**
 * A read whose result's keys are NOT in alphabetical order.
 *
 * The order matters and is the whole point: LangChain turns this object into
 * the tool message's text in the order written here, and salvor hands the same
 * result back from the log with its keys sorted. A middleware that let those
 * two texts differ would fork the thread at the model call that reads the
 * result, which is precisely what two people reported and what neither the
 * alphabetical `lookup_order` nor a single-tool script could ever show.
 */
const trackParcel = tool(
  async ({ parcel_id }: { parcel_id: string }) => {
    ran.track += 1;
    return { tracking_number: `TRK-${parcel_id}`, status: "in_transit", eta: "2026-03-04" };
  },
  {
    name: "track_parcel",
    description: "Track a parcel that has already shipped.",
    schema: z.object({ parcel_id: z.string() }),
  },
);

/** The keyed write behind that read, its own result also out of order. */
const recordDelivery = tool(
  async ({ tracking_number, signed_by }: { tracking_number: string; signed_by: string }) => {
    ran.deliver += 1;
    return {
      tracking_number,
      entry_id: `entry-${signed_by.length}`,
      recorded_at: "2026-03-04T09:00:00Z",
    };
  },
  {
    name: "record_delivery",
    description: "Record that a parcel was delivered.",
    schema: z.object({ tracking_number: z.string(), signed_by: z.string() }),
  },
);

// -- the server --------------------------------------------------------------

function freePort(): Promise<number> {
  return new Promise((resolvePort) => {
    const srv = netServer();
    srv.listen(0, "127.0.0.1", () => {
      const port = (srv.address() as AddressInfo).port;
      srv.close(() => resolvePort(port));
    });
  });
}

let serve: ChildProcess | undefined;
let base: string | undefined;
let client: SalvorClient | undefined;
let port = 0;
/**
 * The suite's own directory under the system temp dir, holding the one store
 * every case shares. It is made with `mkdtemp` and removed whole in `after`,
 * pass or fail, so a suite that ran leaves nothing behind: a store file named
 * after a port is world-readable, outlives the run, and accumulates one triple
 * (db, wal, shm) per invocation forever.
 */
let storeDir: string | undefined;

/**
 * Start `salvor serve` on this suite's port and store, and wait until it
 * answers. Separate from `before` because the restart case starts it again:
 * the same port, the same store, a new process, which is the shape of a
 * redeploy and of a crash-and-restart alike.
 */
async function startServe(): Promise<ChildProcess> {
  const child = spawn(
    SALVOR,
    [
      "--store",
      join(storeDir!, "langchain.db"),
      "serve",
      "--bind",
      `127.0.0.1:${port}`,
      ...DECLS.flatMap((path) => ["--client-tool", path]),
    ],
    {
      stdio: "ignore",
      env: {
        PATH: "/usr/bin:/bin",
        [SERVER_DRIVEN_MODEL_BASE_URL_ENV]: "http://127.0.0.1:1",
      },
    },
  );
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`http://127.0.0.1:${port}/v1/client-tools`);
      if (resp.ok) return child;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill();
  throw new Error("salvor serve did not come up");
}

/**
 * Kill the running server and bring a fresh one up on the same port and store.
 *
 * The wait on `exit` is not politeness: the new process binds the same port,
 * and starting it before the old one has released it is a race the suite would
 * lose intermittently.
 */
async function restartServe(): Promise<void> {
  const old = serve;
  serve = undefined;
  if (old) {
    old.kill("SIGKILL");
    await once(old, "exit");
  }
  serve = await startServe();
}

before(async () => {
  if (!existsSync(SALVOR)) return;
  storeDir = mkdtempSync(join(tmpdir(), "salvor-ts-"));
  port = await freePort();
  base = `http://127.0.0.1:${port}`;
  try {
    serve = await startServe();
  } catch {
    base = undefined;
    return;
  }
  client = new SalvorClient(base);
});

after(() => {
  serve?.kill();
  if (storeDir) rmSync(storeDir, { recursive: true, force: true });
});

function reset(): void {
  ran.lookup = 0;
  ran.stamp = 0;
  ran.track = 0;
  ran.deliver = 0;
  ran.payout = 0;
  ran.concurrent = 0;
  ran.peakConcurrent = 0;
  stampCrashes = false;
  capturedCall = undefined;
  midToolCall = undefined;
}

/**
 * Read a run's recorded log straight off the log-read endpoint, which needs no
 * drive token (see `GET /v1/client-runs/{id}/log` in API.md). A bare
 * `openClientRun` would try to mint or take the lease, which a run another
 * driver still holds refuses with `409 lease_held`; these assertions only want
 * to read what is recorded, so they never touch the lease at all.
 */
async function readLog(threadId: string): Promise<SalvorEvent[]> {
  const runId = await runIdForThread(threadId);
  const driver = new ClientRunDriver(client!.baseUrl, {}, 5000, {
    runId,
    driveToken: "",
    log: [],
  });
  return driver.log();
}

async function kindsOf(threadId: string): Promise<string[]> {
  return (await readLog(threadId)).map((event) => event.kind);
}

function agentFor(
  turns: Turn[],
  tools: unknown[] = [lookupOrder, stampLedger],
  options: Partial<Parameters<typeof salvorMiddleware>[0]> = {},
): { agent: ReturnType<typeof createAgent>; model: ScriptedModel } {
  const model = new ScriptedModel(turns);
  const agent = createAgent({
    model: model as never,
    tools: tools as never,
    middleware: [salvorMiddleware({ client: client!, ...options })],
  });
  return { agent, model };
}

/** Every recorded event of a thread's run, by kind, read straight from the log. */
async function eventsOf(threadId: string): Promise<
  { kind: string; seq: number; payload: Record<string, unknown> }[]
> {
  const log = await readLog(threadId);
  return log.map((event) => ({
    kind: event.kind,
    seq: event.seq,
    payload: event.payload,
  }));
}

/** What `response_metadata.salvor` says about a message, whatever it says. */
function markerOf(message: BaseMessage): Record<string, any> | undefined {
  return (message as AIMessage).response_metadata?.salvor as
    | Record<string, any>
    | undefined;
}

function textOf(message: BaseMessage): string {
  return typeof message.content === "string"
    ? message.content
    : JSON.stringify(message.content);
}

// -- (a) and (b): record a run, then replay it -------------------------------

const ONE_TOOL_SCRIPT: Turn[] = [
  {
    content: "looking that up",
    toolCalls: [{ name: "lookup_order", args: { order_id: "ORD-7781" }, id: "call-1" }],
  },
  { content: "Order ORD-7781 is paid, 4200 cents." },
];

test("a run records one model call and one tool call, and a second invoke replays both", async (t) => {
  if (!base) return t.skip("salvor serve not available (build with cargo build)");
  reset();
  const threadId = "thread-record-and-replay";

  // (a) The first invoke pays for everything, and the log says so.
  const first = agentFor(ONE_TOOL_SCRIPT);
  const answer = await first.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(first.model.calls.count, 2, "two model calls: the tool turn and the answer");
  strictEqual(ran.lookup, 1, "the tool body ran once");

  deepStrictEqual(await kindsOf(threadId), [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);

  const finalMessage = answer.messages.at(-1)!;
  strictEqual(textOf(finalMessage), "Order ORD-7781 is paid, 4200 cents.");

  // A live message says it is live. Without this the absence of a marker would
  // have to be read as "live, probably", and a middleware that silently stopped
  // marking anything would look exactly the same.
  const runId = await runIdForThread(threadId);
  deepStrictEqual(markerOf(finalMessage), { live: true, seq: 5, run: runId });
  deepStrictEqual(markerOf(answer.messages.find((m) => m.getType() === "tool")!), {
    live: true,
    seq: 3,
    run: runId,
  });

  // (b) The second invoke of the same thread pays for nothing at all.
  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  const again = await second.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(second.model.calls.count, 0, "zero model calls on the replay");
  strictEqual(ran.lookup, 0, "zero tool executions on the replay");
  strictEqual(
    textOf(again.messages.at(-1)!),
    textOf(finalMessage),
    "the same final message, from the log",
  );

  // Replayed messages say so, and the log did not grow.
  const replayed = again.messages.at(-1) as AIMessage;
  const marker = replayed.response_metadata.salvor as { replayed: boolean; seq: number };
  strictEqual(marker.replayed, true);
  strictEqual(marker.seq, 5, "the second model call sat at seq 5");
  strictEqual((await kindsOf(threadId)).length, 7, "the replay wrote nothing");
});

// -- (b2) a tool result whose keys are not in alphabetical order --------------

/**
 * read, then model, then keyed write, then model: the shortest chain in which
 * a tool result's key order can fork a thread.
 *
 * The tool message a live call produces is LangChain's stringification of the
 * tool's own object, keys in the order the tool wrote them. The tool message a
 * replay produces is built from the log, and salvor stores an output as a map,
 * so its keys come back sorted. The next model call hashes the messages it is
 * given, tool results included, so if those two texts differ the second invoke
 * misses at that model call, forks, and appends everything after it again,
 * `record_delivery` included, under a fresh key, on every invoke forever.
 *
 * Nothing in the older cases could catch it: `lookup_order` returns
 * `{ order_id, status, total_cents }`, which is already alphabetical, and no
 * script chained a tool, a model call and another tool.
 */
const PARCEL_SCRIPT: Turn[] = [
  {
    content: "tracking it",
    toolCalls: [{ name: "track_parcel", args: { parcel_id: "PCL-31" }, id: "call-track" }],
  },
  {
    content: "recording the delivery",
    toolCalls: [
      {
        name: "record_delivery",
        args: { tracking_number: "TRK-PCL-31", signed_by: "R. Diaz" },
        id: "call-deliver",
      },
    ],
  },
  { content: "Parcel PCL-31 is delivered." },
];

test("a tool whose result keys are not alphabetical replays through a following model call and a following write", async (t) => {
  if (!base) return t.skip("salvor serve not available (build with cargo build)");
  reset();
  const threadId = "thread-key-order";
  const tools = [trackParcel, recordDelivery];
  const input = { messages: [{ role: "user", content: "where is PCL-31?" }] };

  const first = agentFor(PARCEL_SCRIPT, tools);
  const answer = await first.agent.invoke(input, {
    configurable: { thread_id: threadId },
  });
  strictEqual(first.model.calls.count, 3, "three model calls: track, record, answer");
  strictEqual(ran.track, 1);
  strictEqual(ran.deliver, 1);
  strictEqual(textOf(answer.messages.at(-1)!), "Parcel PCL-31 is delivered.");

  // The live tool message carries the same text the log will hand back: keys
  // sorted, no spaces. Without this the next model call's hash is taken over
  // bytes no replay can reproduce.
  const liveToolMessage = answer.messages.find((m) => m.getType() === "tool")!;
  strictEqual(
    textOf(liveToolMessage),
    '{"eta":"2026-03-04","status":"in_transit","tracking_number":"TRK-PCL-31"}',
    "the live tool message is the canonical serialisation, not the tool's key order",
  );

  const before = await eventsOf(threadId);
  const writeKey = before.find(
    (event) =>
      event.kind === "ToolCallRequested" && event.payload.tool === "record_delivery",
  )!.payload.idempotency_key;
  ok(writeKey, "the write's key was derived and recorded");

  // The second invoke pays for nothing and runs nothing, which is only true if
  // the model call after the tool call re-derived the identical request hash.
  reset();
  const second = agentFor(PARCEL_SCRIPT, tools);
  const again = await second.agent.invoke(input, {
    configurable: { thread_id: threadId },
  });
  strictEqual(second.model.calls.count, 0, "zero model calls on the replay");
  strictEqual(ran.track, 0, "zero tool runs on the replay");
  strictEqual(ran.deliver, 0, "the write did not run a second time");
  strictEqual(textOf(again.messages.at(-1)!), "Parcel PCL-31 is delivered.");
  deepStrictEqual(markerOf(again.messages.at(-1)!), {
    replayed: true,
    seq: 9,
    run: await runIdForThread(threadId),
  });

  const after = await eventsOf(threadId);
  strictEqual(after.length, before.length, "the replay appended nothing");
  strictEqual(
    after.filter(
      (event) =>
        event.kind === "ToolCallRequested" && event.payload.tool === "record_delivery",
    ).length,
    1,
    "one recorded write, not one per invoke",
  );
  strictEqual(
    after.find(
      (event) =>
        event.kind === "ToolCallRequested" && event.payload.tool === "record_delivery",
    )!.payload.idempotency_key,
    writeKey,
    "the write's recorded key is identical across invokes",
  );
});

// -- (c) a crash between a tool's intent and its completion ------------------

test("a crash between a write's intent and its completion leaves a dangling intent the next invoke picks up", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-crash-mid-write";
  const script: Turn[] = [
    {
      content: "stamping the ledger",
      toolCalls: [
        {
          name: "stamp_ledger",
          args: { order_id: "ORD-9001", note: "seen" },
          id: "call-stamp",
        },
      ],
    },
    { content: "Stamped ORD-9001." },
  ];

  // The tool dies after salvor recorded the intent and before anything could
  // report a result, which is the shape of every real mid-write crash.
  stampCrashes = true;
  const crashed = agentFor(script);
  await rejects(
    () =>
      crashed.agent.invoke(
        { messages: [{ role: "user", content: "stamp ORD-9001" }] },
        { configurable: { thread_id: threadId } },
      ),
    (error: unknown) => /ledger writer died/.test(String(error)),
  );
  strictEqual(ran.stamp, 1, "the tool body ran once and threw");
  deepStrictEqual(
    await kindsOf(threadId),
    ["RunStarted", "ModelCallRequested", "ModelCallCompleted", "ToolCallRequested"],
    "the log ends at the intent: a write asked for and never reported",
  );

  // The next invoke replays the model call for free, meets the dangling intent,
  // performs the call once more under the same derived key, and closes it.
  reset();
  stampCrashes = false;
  const recovered = agentFor(script);
  const answer = await recovered.agent.invoke(
    { messages: [{ role: "user", content: "stamp ORD-9001" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(recovered.model.calls.count, 1, "only the answer turn was live");
  strictEqual(ran.stamp, 1, "the unfinished write ran once more");
  strictEqual(textOf(answer.messages.at(-1)!), "Stamped ORD-9001.");

  const kinds = await kindsOf(threadId);
  deepStrictEqual(kinds, [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);
  strictEqual(
    kinds.filter((kind) => kind === "ToolCallRequested").length,
    1,
    "exactly one intent",
  );
  strictEqual(
    kinds.filter((kind) => kind === "ToolCallCompleted").length,
    1,
    "exactly one completion",
  );
});

// -- (d) two tool calls in one model turn ------------------------------------

test("two tool calls in one model turn are serialised by the turnstile and both recorded", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-parallel-tools";
  const script: Turn[] = [
    {
      content: "looking both up",
      toolCalls: [
        { name: "lookup_order", args: { order_id: "ORD-1" }, id: "call-a" },
        { name: "lookup_order", args: { order_id: "ORD-2" }, id: "call-b" },
      ],
    },
    { content: "Both orders are paid." },
  ];

  const { agent, model } = agentFor(script);
  const answer = await agent.invoke(
    { messages: [{ role: "user", content: "check ORD-1 and ORD-2" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(model.calls.count, 2);
  strictEqual(ran.lookup, 2, "both tool calls executed");
  strictEqual(ran.peakConcurrent, 1, "never two at once: the turnstile held the second");
  strictEqual(textOf(answer.messages.at(-1)!), "Both orders are paid.");

  deepStrictEqual(await kindsOf(threadId), [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);

  // The order the model asked for is the order the log recorded, which is what
  // makes the pair replayable rather than merely serialized.
  const loggedTools = await readLog(threadId);
  const inputs = loggedTools
    .filter((event) => event.kind === "ToolCallRequested")
    .map((event) => (event.payload.input as { order_id: string }).order_id);
  deepStrictEqual(inputs, ["ORD-1", "ORD-2"]);

  // And a replay of the whole turn touches neither the model nor the tools.
  reset();
  const second = agentFor(script);
  await second.agent.invoke(
    { messages: [{ role: "user", content: "check ORD-1 and ORD-2" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(second.model.calls.count, 0);
  strictEqual(ran.lookup, 0);
});

// -- (d2) hook entry order is adversarial, but the model's order still wins --

/**
 * A middleware that sits ahead of salvor's in the list and, for one named
 * tool call, awaits a macrotask before calling `handler`. Every other call
 * passes straight through. Composed ahead of `salvorMiddleware`, this delays
 * only when *that* call's turn reaches salvor's own `wrapToolCall`, so the
 * calls behind it in `tool_calls` order are entered there first: the exact
 * shape of the out-of-order arrival the Python port measured, forced instead
 * of hoped for.
 */
function delayEntry(toolCallId: string, ms: number) {
  return createMiddleware({
    name: "DelayEntryMiddleware",
    wrapToolCall: async (request: any, handler: any) => {
      if (request.toolCall.id === toolCallId) {
        await new Promise((r) => setTimeout(r, ms));
      }
      return handler(request);
    },
  });
}

test("three tool calls in one model turn are recorded in the model's order even when a middleware ahead of salvor's reorders hook entry", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-adversarial-entry-order";
  const script: Turn[] = [
    {
      content: "looking all three up",
      toolCalls: [
        { name: "lookup_order", args: { order_id: "ORD-A" }, id: "call-a" },
        { name: "lookup_order", args: { order_id: "ORD-B" }, id: "call-b" },
        { name: "lookup_order", args: { order_id: "ORD-C" }, id: "call-c" },
      ],
    },
    { content: "All three are paid." },
  ];

  async function recordedInputs(): Promise<string[]> {
    const log = await readLog(threadId);
    return log
      .filter((event) => event.kind === "ToolCallRequested")
      .map((event) => (event.payload.input as { order_id: string }).order_id);
  }

  // rank 0 (`call-a`) is entered into salvor's own `wrapToolCall` last, well
  // after rank 1 and rank 2 have already been entered synchronously ahead of
  // its timer firing. If the turnstile admitted on arrival, the log would
  // read ORD-B, ORD-C, ORD-A instead of the model's own order.
  const first = new ScriptedModel(script);
  const firstAgent = createAgent({
    model: first as never,
    tools: [lookupOrder, stampLedger] as never,
    middleware: [delayEntry("call-a", 30), salvorMiddleware({ client: client! })] as never,
  });
  await firstAgent.invoke(
    { messages: [{ role: "user", content: "check all three" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(first.calls.count, 2, "the tool turn and the answer turn");
  strictEqual(ran.lookup, 3, "all three tool bodies executed");
  strictEqual(ran.peakConcurrent, 1, "never two at once: the turnstile held the others");
  deepStrictEqual(
    await recordedInputs(),
    ["ORD-A", "ORD-B", "ORD-C"],
    "recorded in tool_calls order, not entry order",
  );

  // Two more invokes of the same thread replay the whole turn: same order,
  // zero model calls, zero tool runs, on each of them.
  for (let i = 0; i < 2; i += 1) {
    reset();
    const replay = new ScriptedModel(script);
    const replayAgent = createAgent({
      model: replay as never,
      tools: [lookupOrder, stampLedger] as never,
      middleware: [delayEntry("call-a", 30), salvorMiddleware({ client: client! })] as never,
    });
    await replayAgent.invoke(
      { messages: [{ role: "user", content: "check all three" }] },
      { configurable: { thread_id: threadId } },
    );
    strictEqual(replay.calls.count, 0, `replay ${i}: zero model calls`);
    strictEqual(ran.lookup, 0, `replay ${i}: zero tool runs`);
    deepStrictEqual(await recordedInputs(), ["ORD-A", "ORD-B", "ORD-C"], `replay ${i}: same order`);
  }
});

// -- (e) a replayed answer under streaming -----------------------------------

test("a replayed answer streams as one whole chunk, marked replayed", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-streaming-replay";
  const input = { messages: [{ role: "user", content: "how is ORD-7781?" }] };

  const first = agentFor(ONE_TOOL_SCRIPT);
  await first.agent.invoke(input, { configurable: { thread_id: threadId } });
  strictEqual(first.model.calls.count, 2);

  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  const aiChunks: AIMessage[] = [];
  for await (const [message] of await second.agent.stream(input, {
    streamMode: "messages",
    configurable: { thread_id: threadId },
  })) {
    if ((message as BaseMessage).getType() === "ai") aiChunks.push(message as AIMessage);
  }

  strictEqual(second.model.calls.count, 0, "the stream paid for nothing");
  strictEqual(ran.lookup, 0);
  strictEqual(
    aiChunks.length,
    2,
    "one whole chunk per recorded model call, never re-tokenised",
  );
  for (const chunk of aiChunks) {
    const marker = chunk.response_metadata.salvor as { replayed: boolean; seq: number };
    ok(marker, "every replayed answer says it was replayed");
    strictEqual(marker.replayed, true);
    ok(typeof marker.seq === "number");
  }
  deepStrictEqual(
    aiChunks.map((chunk) => chunk.response_metadata.salvor.seq),
    [1, 5],
  );
  strictEqual(textOf(aiChunks[1]), "Order ORD-7781 is paid, 4200 cents.");
});

// -- (f) a tool nobody declared ----------------------------------------------

test("a tool with no client-tool declaration is refused by name", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const script: Turn[] = [
    {
      content: "emailing",
      toolCalls: [{ name: "send_email", args: { to: "ops@example.com" }, id: "call-mail" }],
    },
    { content: "Sent." },
  ];
  const { agent } = agentFor(script, [lookupOrder, stampLedger, sendEmail]);

  await rejects(
    () =>
      agent.invoke(
        { messages: [{ role: "user", content: "email ops" }] },
        { configurable: { thread_id: "thread-undeclared-tool" } },
      ),
    (error: unknown) => {
      const text = String((error as Error).message ?? error);
      match(text, /send_email/, "the error names the tool");
      match(text, /client-tool declaration/, "and the declaration it needs");
      match(text, /--client-tool/, "and how to load it");
      return true;
    },
  );
});

// -- leaving the recorded path ------------------------------------------------

test("an invoke that asks for something the log does not hold appends instead of replaying", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-second-question";

  const first = agentFor(ONE_TOOL_SCRIPT);
  await first.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual((await kindsOf(threadId)).length, 7);

  // A different question down the same thread is a different first model call,
  // so nothing at the recorded positions applies. The run carries on at the end
  // of its log rather than pretending the old answers are still answers, and it
  // says out loud that it did.
  reset();
  const notices: SalvorForkNotice[] = [];
  const second = agentFor([{ content: "ORD-9999 is not one of ours." }], undefined, {
    onFork: (notice) => notices.push(notice),
  });
  const answer = await second.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-9999?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(second.model.calls.count, 1, "the new question was asked for real");
  strictEqual(textOf(answer.messages.at(-1)!), "ORD-9999 is not one of ours.");
  deepStrictEqual((await kindsOf(threadId)).slice(7), [
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);
  strictEqual(notices.length, 1, "one notice for the invoke, not one per step");
  strictEqual(notices[0].at, 1, "it diverged at the very first model call");
  deepStrictEqual(markerOf(answer.messages.at(-1)!), {
    forked: { at: 1, thread: threadId, run: await runIdForThread(threadId) },
  });
});

// -- a fork after the recorded path was walked partway -----------------------

/**
 * A middleware that stamps a per-invoke nonce into the system prompt of every
 * model call that has a tool result to read.
 *
 * This is a graph branching on something outside the log, which is the honest
 * cause of most forks and the one the warning tells an operator to look for.
 * It is applied only to the model call AFTER a tool call so that the fork lands
 * partway down a recorded path rather than at its first step: the messages
 * before it replay, the messages after it do not, and a marker that only
 * appeared on the first message after a fork would be caught here.
 */
function stampAfterTools(nonce: string) {
  return createMiddleware({
    name: "StampMiddleware",
    wrapModelCall: async (request: any, handler: any) => {
      const readsATool = request.messages.some(
        (message: BaseMessage) => message.getType() === "tool",
      );
      if (!readsATool) return handler(request);
      return handler({ ...request, systemPrompt: `stamped ${nonce}` });
    },
  });
}

test("an invoke that forks partway marks every later message and warns exactly once", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-fork-partway";
  const input = { messages: [{ role: "user", content: "how is ORD-7781?" }] };
  const runId = await runIdForThread(threadId);

  function agentStamped(turns: Turn[], nonce: string, onFork: (n: SalvorForkNotice) => void) {
    const model = new ScriptedModel(turns);
    const agent = createAgent({
      model: model as never,
      tools: [lookupOrder, stampLedger] as never,
      middleware: [
        stampAfterTools(nonce),
        salvorMiddleware({ client: client!, onFork }),
      ] as never,
    });
    return { agent, model };
  }

  const first = agentStamped(ONE_TOOL_SCRIPT, "one", () => {
    throw new Error("the first invoke has no recorded path to leave");
  });
  await first.agent.invoke(input, { configurable: { thread_id: threadId } });
  strictEqual(first.model.calls.count, 2);
  strictEqual((await kindsOf(threadId)).length, 7);

  // The same question, the same tool result, a different stamp on the second
  // model call, and a model that now answers something else.
  reset();
  const notices: SalvorForkNotice[] = [];
  const second = agentStamped(
    [ONE_TOOL_SCRIPT[0], { content: "Order ORD-7781 was refunded after all." }],
    "two",
    (notice) => notices.push(notice),
  );
  const answer = await second.agent.invoke(input, {
    configurable: { thread_id: threadId },
  });

  strictEqual(second.model.calls.count, 1, "only the diverged model call was live");
  strictEqual(ran.lookup, 0, "the tool call before the fork still replayed");
  strictEqual(textOf(answer.messages.at(-1)!), "Order ORD-7781 was refunded after all.");

  // One warning for the invoke, naming where it happened and what to look at.
  strictEqual(notices.length, 1, "exactly one warning per invoke");
  strictEqual(notices[0].at, 5, "the fork is at the second model call's position");
  strictEqual(notices[0].thread, threadId);
  strictEqual(notices[0].run, runId);
  match(notices[0].message, new RegExp(threadId), "the message names the thread");
  match(notices[0].message, new RegExp(runId), "and the run");
  match(notices[0].message, /seq 5/, "and the seq");
  match(notices[0].message, /branches on the clock/, "and what to check");

  // Everything before the fork still says it was replayed; everything from the
  // fork on says it forked, and where.
  const ai = answer.messages.filter((m) => m.getType() === "ai");
  deepStrictEqual(markerOf(ai[0]), { replayed: true, seq: 1, run: runId });
  deepStrictEqual(markerOf(answer.messages.find((m) => m.getType() === "tool")!), {
    replayed: true,
    seq: 3,
    run: runId,
  });
  deepStrictEqual(markerOf(ai.at(-1)!), { forked: { at: 5, thread: threadId, run: runId } });

  // The fork was appended, not lost: the recorded path is still there, with the
  // new answer after it.
  deepStrictEqual((await kindsOf(threadId)).slice(7), [
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);
});

// -- (g) finishThread closes a thread's run -----------------------------------

test("finishThread appends RunCompleted, GET /v1/runs/{id} shows completed, and a further invoke is refused", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-finish";

  const first = agentFor(ONE_TOOL_SCRIPT);
  const answer = await first.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  const finalText = textOf(answer.messages.at(-1)!);

  const runId = await runIdForThread(threadId);
  const finished = await finishThread(client!, threadId);
  strictEqual(finished.runId, runId);

  deepStrictEqual((await kindsOf(threadId)).slice(-1), ["RunCompleted"]);

  const state = await client!.getRun(runId);
  strictEqual(state.status.state, "completed");
  strictEqual(state.status.output, finalText, "the default output is the last AI message");

  // A further invoke on the finished thread is refused, clearly, rather than
  // failing somewhere inside the append.
  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  await rejects(
    () =>
      second.agent.invoke(
        { messages: [{ role: "user", content: "how is ORD-7781?" }] },
        { configurable: { thread_id: threadId } },
      ),
    (error: unknown) => {
      const text = String((error as Error).message ?? error);
      match(text, /thread-finish/, "the error names the thread");
      match(text, /finish/i, "and says it is finished");
      return true;
    },
  );
});

// -- (h) currentToolCall() inside a tool body ---------------------------------

test("a tool body reads currentToolCall(), and the key matches the recorded intent on both the live and the replayed invoke", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-current-tool-call";

  const first = agentFor(ONE_TOOL_SCRIPT);
  await first.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  ok(capturedCall, "the tool body read a current call");
  strictEqual(capturedCall!.tool, "lookup_order");
  strictEqual(capturedCall!.runId, await runIdForThread(threadId));

  const intent = (await readLog(threadId)).find((event) => event.kind === "ToolCallRequested")!;
  strictEqual(capturedCall!.seq, intent.seq, "the seq matches the recorded intent");
  strictEqual(
    capturedCall!.key,
    intent.payload.idempotency_key,
    "the key is the one salvor recorded on the intent",
  );

  // A replayed invoke never runs the tool body, so nothing new is captured,
  // but the log's own recorded key is unchanged.
  capturedCall = undefined;
  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  await second.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(capturedCall, undefined, "the replay never ran the tool body");
  strictEqual(ran.lookup, 0);

  const replayedIntent = (await readLog(threadId)).find(
    (event) => event.kind === "ToolCallRequested",
  )!;
  strictEqual(
    replayedIntent.payload.idempotency_key,
    intent.payload.idempotency_key,
    "the recorded key is identical on replay",
  );
});

// -- (i) finishThread refuses a thread with an open intent --------------------

test("finishThread on a thread whose log ends at an open intent is refused, naming the run", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-finish-open-intent";
  const script: Turn[] = [
    {
      content: "stamping the ledger",
      toolCalls: [
        { name: "stamp_ledger", args: { order_id: "ORD-4242", note: "seen" }, id: "call-stamp" },
      ],
    },
    { content: "Stamped ORD-4242." },
  ];

  stampCrashes = true;
  const crashed = agentFor(script);
  await rejects(() =>
    crashed.agent.invoke(
      { messages: [{ role: "user", content: "stamp ORD-4242" }] },
      { configurable: { thread_id: threadId } },
    ),
  );
  stampCrashes = false;

  const runId = await runIdForThread(threadId);
  await rejects(
    () => finishThread(client!, threadId),
    (error: unknown) => {
      const text = String((error as Error).message ?? error);
      match(text, new RegExp(runId), "the error names the run");
      match(text, /never completed/, "and says the call was never completed");
      return true;
    },
  );

  // Nothing was appended: the log still ends at the same open intent.
  deepStrictEqual(
    (await kindsOf(threadId)).slice(-1),
    ["ToolCallRequested"],
    "finishThread wrote nothing",
  );
});

// -- (j) a tool declared trust_completion = false stops for a person ---------

const PAYOUT_SCRIPT: Turn[] = [
  {
    content: "sending the payout",
    toolCalls: [
      {
        name: "wire_payout",
        args: { payee: "acct-9001", amount_cents: 250000 },
        id: "call-payout",
      },
    ],
  },
  { content: "Payout sent." },
];

async function invokeRejection(
  agent: ReturnType<typeof createAgent>,
  threadId: string,
): Promise<unknown> {
  try {
    await agent.invoke(
      { messages: [{ role: "user", content: "wire the payout" }] },
      { configurable: { thread_id: threadId } },
    );
  } catch (error) {
    return error;
  }
  throw new Error("expected the invoke to reject");
}

/**
 * `createAgent` wraps every error a middleware throws in its own
 * `MiddlewareError`, copying the original's `name` and `message` but keeping
 * the actual instance only as `.cause`. Unwrap it to reach the typed error
 * salvor's own middleware threw.
 */
function causeOf(error: unknown): unknown {
  return (error as { cause?: unknown } | null | undefined)?.cause ?? error;
}

test("a tool declared trust_completion = false runs once, then stops the invoke for a person to resolve", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-untrusted-completion";
  const runId = await runIdForThread(threadId);

  // The first invoke pays for the model call, runs the tool body exactly
  // once, and then stops with the typed error rather than reporting a
  // completion salvor would refuse.
  const first = agentFor(PAYOUT_SCRIPT, [wirePayout]);
  const stopped = causeOf(await invokeRejection(first.agent, threadId));
  ok(stopped instanceof ToolNeedsResolution, "throws ToolNeedsResolution");
  const stop = stopped as ToolNeedsResolution;
  strictEqual(stop.run, runId);
  strictEqual(stop.thread, threadId);
  strictEqual(stop.tool, "wire_payout");
  strictEqual(typeof stop.key, "string");
  ok(stop.key.length > 0, "carries the derived idempotency key");
  const output = { provider_transfer_id: "ptx-acct-9001", status: "succeeded", amount_cents: 250000 };
  deepStrictEqual(stop.output, output, "carries what the tool body returned");
  match(stop.message, /trust_completion = false/);
  match(stop.message, new RegExp(`salvor resolve ${runId}`), "names the resolve command");

  strictEqual(ran.payout, 1, "the tool body ran exactly once");
  deepStrictEqual(
    await kindsOf(threadId),
    ["RunStarted", "ModelCallRequested", "ModelCallCompleted", "ToolCallRequested"],
    "the log ends at the intent: no completion was ever reported",
  );

  // Re-invoking before anyone resolves it does not run the tool again: it
  // meets the same open intent and is refused by name, naming the same fix.
  reset();
  const impatient = agentFor(PAYOUT_SCRIPT, [wirePayout]);
  const refusal = await invokeRejection(impatient.agent, threadId);
  const refusalText = String((refusal as Error).message ?? refusal);
  match(refusalText, /never completed/, "the same refusal a stuck intent always gets");
  match(refusalText, new RegExp(`salvor resolve ${runId}`), "names the resolve step");
  strictEqual(ran.payout, 0, "refused before the tool ran again");
  deepStrictEqual(
    (await kindsOf(threadId)).slice(-1),
    ["ToolCallRequested"],
    "still nothing appended",
  );

  // A person resolves it by hand, through the driver: the same surface
  // `salvor resolve` and the Inspector both go through.
  const driver = await client!.openClientRun({ runId });
  await driver.resolve(stop.output);

  // The next invoke meets a settled call at that seq and replays it: zero
  // tool executions, the resolved output on the tool message and in the
  // final answer.
  reset();
  const second = agentFor(PAYOUT_SCRIPT, [wirePayout]);
  const answer = await second.agent.invoke(
    { messages: [{ role: "user", content: "wire the payout" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(ran.payout, 0, "the write never ran again");
  strictEqual(second.model.calls.count, 1, "only the answer turn was live");

  const toolMessage = answer.messages.find((m) => m.getType() === "tool")!;
  deepStrictEqual(JSON.parse(textOf(toolMessage)), output, "the resolved output reached the model");
  strictEqual(textOf(answer.messages.at(-1)!), "Payout sent.");

  deepStrictEqual(await kindsOf(threadId), [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);
});

// -- the lease ---------------------------------------------------------------

test("a second instance on a held thread is refused with lease_held before running anything", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-lease-held";
  const runId = await runIdForThread(threadId);

  // A genuinely independent SalvorClient: no memory of any lease `client!`
  // has ever held, exactly what a second instance of this application would
  // be. Sharing `client!` here would let the "rival" ride the same
  // instance's own remembered token (see `SalvorClient.openClientRun`) and
  // succeed as if it were the very driver it is supposed to be contesting.
  const rival = new SalvorClient(base!);

  // The rival tries to open the SAME thread while the first invoke is inside
  // a tool body, holding an open intent: the moment a second driver's own
  // open is refused, not the moment it would have written something.
  midToolCall = async () => {
    const second = createAgent({
      model: new ScriptedModel(ONE_TOOL_SCRIPT) as never,
      tools: [lookupOrder, stampLedger] as never,
      middleware: [salvorMiddleware({ client: rival })],
    });
    let refusal: unknown;
    try {
      await second.invoke(
        { messages: [{ role: "user", content: "how is ORD-7781?" }] },
        { configurable: { thread_id: threadId } },
      );
      throw new Error("expected the second instance's invoke to be refused");
    } catch (error) {
      refusal = causeOf(error);
    }
    ok(refusal instanceof SalvorMiddlewareError, "the middleware itself named the refusal");
    const text = (refusal as Error).message;
    match(text, new RegExp(threadId), "the error names the thread");
    match(text, new RegExp(runId), "and the run");
    match(text, /lapses in \d+s/, "and when the hold lapses");
  };

  const { agent, model } = agentFor(ONE_TOOL_SCRIPT);
  const answer = await agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );

  strictEqual(model.calls.count, 2, "both model calls happened");
  strictEqual(ran.lookup, 1, "the tool body ran exactly once: the rival never got in");
  strictEqual(textOf(answer.messages.at(-1)!), "Order ORD-7781 is paid, 4200 cents.");

  // Nothing the rival tried to do landed: one intent, one completion, per step.
  deepStrictEqual(await kindsOf(threadId), [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);

  // And the thread still replays afterwards, `client!`'s own remembered
  // lease intact throughout.
  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  await second.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(second.model.calls.count, 0);
  strictEqual(ran.lookup, 0);
});

/**
 * `invalid_drive_token`, the other one-driver refusal, needs a lease that is
 * first lapsed (so a second driver can legitimately take the run) and then
 * taken while the first driver is still mid-step. `salvor serve --help`
 * carries no `--client-lease-ttl` flag: the TTL is `SALVOR_CLIENT_LEASE_TTL_SECS`
 * only, an environment variable a server reads at startup, so this spins up
 * its own short-TTL server rather than waiting out the suite's default 60s.
 *
 * It also drives `RunTape` directly instead of through a full `createAgent`
 * app: the point under test is `tape.ts`'s own `lease()`, and the server-side
 * mechanics (a lapsed lease, a fresh open, a stale token) are the same either
 * way. This is the driver-API fallback the case calls for in the absence of a
 * CLI flag to shrink the TTL from inside a `createAgent` run.
 */
test("a lease that lapses and is taken mid-step surfaces invalid_drive_token by name, never retried", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  const dir = mkdtempSync(join(tmpdir(), "salvor-ts-lease-"));
  const shortPort = await freePort();
  const shortBase = `http://127.0.0.1:${shortPort}`;
  const shortTtlSecs = 1;
  const child = spawn(
    SALVOR,
    ["--store", join(dir, "short.db"), "serve", "--bind", `127.0.0.1:${shortPort}`],
    {
      stdio: "ignore",
      env: { PATH: "/usr/bin:/bin", SALVOR_CLIENT_LEASE_TTL_SECS: String(shortTtlSecs) },
    },
  );
  try {
    const deadline = Date.now() + 15000;
    for (;;) {
      try {
        if ((await fetch(`${shortBase}/v1/client-tools`)).ok) break;
      } catch {
        /* not up yet */
      }
      if (Date.now() > deadline) throw new Error("short-TTL salvor serve did not come up");
      await new Promise((r) => setTimeout(r, 100));
    }

    const shortClient = new SalvorClient(shortBase);
    const driver = await shortClient.openClientRun({});
    const threadId = "thread-lease-lapsed";

    let reopenCalls = 0;
    const tape = await RunTape.open(
      driver,
      { agent_def_hash: "sha256:agent", input: {} },
      {
        threadId,
        recordPrompts: false,
        reopen: () => {
          reopenCalls += 1;
          throw new Error("must never be asked to re-open: invalid_drive_token is not a restart");
        },
      },
    );

    // Nothing drives the run again for longer than the TTL, so its lease
    // lapses; a second, independent client then bare-opens the idle run and
    // is handed a fresh lease rather than refused, exactly as an idle run
    // going quiet is supposed to work.
    await new Promise((r) => setTimeout(r, (shortTtlSecs + 0.5) * 1000));
    await new SalvorClient(shortBase).openClientRun({ runId: driver.runId });

    // The tape's next step still presents the token it opened with, now
    // superseded, and salvor answers `invalid_drive_token`. Refused by name
    // at once, not retried: a lease taken while this tape was mid-step is a
    // live second driver, not a restart, and re-opening could only hand the
    // run to one of two live drivers picked by timing.
    await rejects(
      () =>
        tape.modelCall("sha256:req-1", { model: "m" }, async () => {
          throw new Error("must not be called: the step never reaches the provider");
        }),
      (error: unknown) => {
        ok(error instanceof SalvorMiddlewareError, "a named middleware refusal");
        const text = (error as Error).message;
        match(text, new RegExp(threadId), "the error names the thread");
        match(text, new RegExp(driver.runId), "and the run");
        match(text, /one driver per thread at a time/i, "and the rule");
        return true;
      },
    );
    strictEqual(reopenCalls, 0, "never re-opened: invalid_drive_token is not the restart case");
  } finally {
    child.kill();
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A salvor restart mid-invoke, survived.
 *
 * A server keeps its client-driven leases in memory, but a restarted
 * `salvor serve` reads a run's `driven_by: client` marker straight off its
 * recorded `RunStarted` and adopts it back, minting a fresh lease for whoever
 * asks. The tape's own `lease()` already retries a step exactly once against
 * whatever driver `reopen` hands back, so the one thing this case has to prove
 * is that the retry lands on a server that now says yes: the invoke completes,
 * the interrupted tool call is not performed twice, and the log holds exactly
 * one intent and one completion for it, in place, not doubled and not
 * abandoned.
 */
test("a salvor restart mid-invoke is survived: the run resumes under a fresh lease", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();
  const threadId = "thread-server-restarted";

  // Between the tool call's intent and its completion, the server this invoke
  // has been driving goes away and a fresh one comes up on the same port and
  // the same store.
  midToolCall = () => restartServe();

  const { agent } = agentFor(ONE_TOOL_SCRIPT);
  const answer = await agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );

  strictEqual(ran.lookup, 1, "the tool body ran exactly once, despite the restart");
  strictEqual(
    textOf(answer.messages.at(-1)!),
    "Order ORD-7781 is paid, 4200 cents.",
    "the invoke finished under the fresh lease the restarted server minted",
  );

  // One intent, one completion for the interrupted call, nothing doubled and
  // nothing left dangling.
  deepStrictEqual(await kindsOf(threadId), [
    "RunStarted",
    "ModelCallRequested",
    "ModelCallCompleted",
    "ToolCallRequested",
    "ToolCallCompleted",
    "ModelCallRequested",
    "ModelCallCompleted",
  ]);

  // A further re-invoke of the same thread pays for none of it: the restart is
  // history the log carries now, not a cost the next invoke bears again.
  reset();
  const second = agentFor(ONE_TOOL_SCRIPT);
  const again = await second.agent.invoke(
    { messages: [{ role: "user", content: "how is ORD-7781?" }] },
    { configurable: { thread_id: threadId } },
  );
  strictEqual(second.model.calls.count, 0, "zero model calls on the replay");
  strictEqual(ran.lookup, 0, "zero tool executions on the replay");
  strictEqual(textOf(again.messages.at(-1)!), textOf(answer.messages.at(-1)!));
  strictEqual((await kindsOf(threadId)).length, 7, "the replay wrote nothing");
});

/**
 * The refusal a lost lease's re-open can still hit: not a restart (adopted
 * since 3d0f051) but a run id that was never client-driven to begin with.
 *
 * A thread id maps to a run id, and nothing stops that id from already naming
 * a run started through the server-driven `/v1/runs` path. Salvor will not
 * adopt such a run for client-driven use, and this middleware turns that
 * refusal into a message naming the thread and the reason rather than letting
 * a bare `run_exists` surface from inside somebody else's agent loop.
 */
test("a server-driven run's id refuses a client-driven open, naming the thread and why", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  reset();

  const agentHash = await client!.registerAgent({
    model: "test-model",
    llm: { base_url_env: SERVER_DRIVEN_MODEL_BASE_URL_ENV },
  });

  // A UUID thread id is its own run id (see the thread-id-rule tests below),
  // which is what lets a server-driven run started under it collide with the
  // run a LangChain invoke on that same thread would open.
  const threadId = randomUUID();
  const runId = await runIdForThread(threadId);
  strictEqual(runId, threadId, "the chosen thread id already is its run id");
  await client!.startRun(agentHash, null, { runId });

  // Wait for `RunStarted` to land: opening before it would find an empty log,
  // which salvor is free to adopt rather than refuse.
  const deadline = Date.now() + 5000;
  while ((await client!.getRun(runId)).eventCount < 1) {
    if (Date.now() > deadline) {
      throw new Error("the server-driven run never recorded its RunStarted");
    }
    await new Promise((r) => setTimeout(r, 20));
  }

  const { agent } = agentFor(ONE_TOOL_SCRIPT);
  await rejects(
    () =>
      agent.invoke(
        { messages: [{ role: "user", content: "how is ORD-7781?" }] },
        { configurable: { thread_id: threadId } },
      ),
    (error: unknown) => {
      const text = String((error as Error).message ?? error);
      match(text, new RegExp(threadId), "the error names the thread");
      match(text, /server-driven run/, "and the reason");
      return true;
    },
  );
  strictEqual(ran.lookup, 0, "the middleware never reached the tool");
});

// -- the thread-id rule ------------------------------------------------------

test("a UUID thread id is the run id; anything else is hashed into one", async () => {
  const uuid = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
  strictEqual(await runIdForThread(uuid), uuid);
  strictEqual(await runIdForThread(uuid.toUpperCase()), uuid);

  const derived = await runIdForThread("order-7781");
  match(derived, /^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  strictEqual(await runIdForThread("order-7781"), derived, "the mapping is stable");
  notStrictEqual(await runIdForThread("order-7782"), derived);
});

// -- the plain entry must not pull LangChain in ------------------------------

test("importing the plain SDK loads no LangChain module", async () => {
  const { readFileSync } = await import("node:fs");
  const dist = resolve(here, "..", "dist");
  const seen = new Set<string>();
  const queue = [resolve(dist, "index.js")];
  while (queue.length > 0) {
    const file = queue.pop()!;
    if (seen.has(file)) continue;
    seen.add(file);
    const source = readFileSync(file, "utf8");
    ok(
      !/langchain/i.test(source),
      `${file} reaches LangChain; the plain entry must not`,
    );
    for (const [, specifier] of source.matchAll(/from\s+"(\.[^"]+)"/g)) {
      queue.push(resolve(dirname(file), specifier));
    }
  }
  ok(seen.size > 1, "the walk actually followed the barrel's imports");
  ok(!seen.has(resolve(dist, "langchain", "index.js")));
});
