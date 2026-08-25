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
import { createServer as netServer } from "node:net";
import type { AddressInfo } from "node:net";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { existsSync } from "node:fs";

import { createAgent, createMiddleware, tool } from "langchain";
import { AIMessage, type BaseMessage } from "@langchain/core/messages";
import { BaseChatModel } from "@langchain/core/language_models/chat_models";
import type { ChatResult } from "@langchain/core/outputs";
import { z } from "zod";

import { SalvorClient } from "../dist/index.js";
import {
  currentToolCall,
  finishThread,
  runIdForThread,
  salvorMiddleware,
} from "../dist/langchain/index.js";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");
const SALVOR = resolve(repoRoot, "target", "debug", "salvor");
const DECLS = [
  resolve(here, "client-tools", "lookup-order.toml"),
  resolve(here, "client-tools", "stamp-ledger.toml"),
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
const ran = { lookup: 0, stamp: 0, concurrent: 0, peakConcurrent: 0 };
/** Set to make the next `stamp_ledger` body throw, standing in for a crash. */
let stampCrashes = false;
/** What `currentToolCall()` reported the last time `lookup_order`'s body ran. */
let capturedCall: ReturnType<typeof currentToolCall> | undefined;

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

before(async () => {
  if (!existsSync(SALVOR)) return;
  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  serve = spawn(
    SALVOR,
    [
      "--store",
      `/tmp/salvor-ts-langchain-${port}.db`,
      "serve",
      "--bind",
      `127.0.0.1:${port}`,
      ...DECLS.flatMap((path) => ["--client-tool", path]),
    ],
    { stdio: "ignore", env: { PATH: "/usr/bin:/bin" } },
  );
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${base}/v1/client-tools`);
      if (resp.ok) {
        client = new SalvorClient(base);
        return;
      }
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  base = undefined;
});

after(() => {
  serve?.kill();
});

function reset(): void {
  ran.lookup = 0;
  ran.stamp = 0;
  ran.concurrent = 0;
  ran.peakConcurrent = 0;
  stampCrashes = false;
  capturedCall = undefined;
}

async function kindsOf(threadId: string): Promise<string[]> {
  const run = await client!.openClientRun({ runId: await runIdForThread(threadId) });
  return run.logEnvelopes.map((event) => event.kind);
}

function agentFor(
  turns: Turn[],
  tools: unknown[] = [lookupOrder, stampLedger],
): { agent: ReturnType<typeof createAgent>; model: ScriptedModel } {
  const model = new ScriptedModel(turns);
  const agent = createAgent({
    model: model as never,
    tools: tools as never,
    middleware: [salvorMiddleware({ client: client! })],
  });
  return { agent, model };
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
  const run = await client!.openClientRun({ runId: await runIdForThread(threadId) });
  const inputs = run.logEnvelopes
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
    const run = await client!.openClientRun({ runId: await runIdForThread(threadId) });
    return run.logEnvelopes
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
  // of its log rather than pretending the old answers are still answers.
  reset();
  const second = agentFor([{ content: "ORD-9999 is not one of ours." }]);
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

  const run = await client!.openClientRun({ runId: await runIdForThread(threadId) });
  const intent = run.logEnvelopes.find((event) => event.kind === "ToolCallRequested")!;
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

  const replayedRun = await client!.openClientRun({ runId: await runIdForThread(threadId) });
  const replayedIntent = replayedRun.logEnvelopes.find(
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
