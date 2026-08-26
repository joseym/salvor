#!/usr/bin/env node
/**
 * A support desk, in TypeScript, made durable by one middleware.
 *
 * The agent is an ordinary LangChain `createAgent`: a model, three tools, and
 * a thread id. The only salvor-shaped line in it is the middleware in
 * `createAgent`'s `middleware` array. Everything else here is what any
 * LangChain app already has, plus the printing this example's `run.sh` reads
 * its proofs out of.
 *
 * `app.py` next door is the same desk, tool for tool and line for line, so a
 * reader can hold the two side by side and see that the durability is the
 * middleware's and not the language's.
 *
 * It is driven by `run.sh`, which passes everything as flags:
 *
 *     node app.ts --server http://127.0.0.1:18401 \
 *                 --thread orders-7781 \
 *                 --ask "Refund ORD-7781, the item arrived damaged."
 *
 * Flags:
 *   --server URL         the control plane to record against (required)
 *   --thread ID          the LangGraph thread id, which is also the run
 *   --ask TEXT           the customer's question
 *   --crash-in TOOL      die with exit 9 inside TOOL, after its ledger write
 *                        and before it returns: a crash between a call
 *                        happening and salvor hearing about it
 *   --slow-tool TOOL=N   make TOOL take N seconds, so a second copy of this
 *                        app can try the same thread while this one holds it
 *   --finish             close the thread with `finishThread` and exit
 *
 * Ledgers land under SALVOR_EXAMPLE_SCRATCH (or the system temp directory).
 * They are this desk's own records, on the desk's side of the reference: the
 * refund identifiers and amounts live there, not in salvor's log.
 *
 * No API key is needed. The model is a scripted stand-in that reads the
 * conversation so far and answers the way a real one would for this desk. Set
 * ANTHROPIC_API_KEY and it uses `ChatAnthropic` instead, with nothing else in
 * this file changing.
 */

import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createAgent, tool } from "langchain";
import { AIMessage, type BaseMessage } from "@langchain/core/messages";
import { BaseChatModel } from "@langchain/core/language_models/chat_models";
import type { ChatResult } from "@langchain/core/outputs";
import { z } from "zod";

import { SalvorClient } from "@salvor-run/client";
import {
  currentToolCall,
  finishThread,
  runIdForThread,
  salvorError,
  salvorMiddleware,
  ToolNeedsResolution,
} from "@salvor-run/client/langchain";

// --- the desk's flags -------------------------------------------------------

interface Options {
  server: string;
  thread: string;
  ask: string;
  crashIn: string | undefined;
  slowTool: string | undefined;
  slowSeconds: number;
  finish: boolean;
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    server: process.env.SALVOR_LC_SERVER ?? "http://127.0.0.1:18401",
    thread: "",
    ask: "",
    crashIn: undefined,
    slowTool: undefined,
    slowSeconds: 0,
    finish: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const value = argv[i + 1];
    switch (flag) {
      case "--server":
        options.server = value;
        i += 1;
        break;
      case "--thread":
        options.thread = value;
        i += 1;
        break;
      case "--ask":
        options.ask = value;
        i += 1;
        break;
      case "--crash-in":
        options.crashIn = value;
        i += 1;
        break;
      case "--slow-tool": {
        const [name, seconds] = value.split("=");
        options.slowTool = name;
        options.slowSeconds = Number(seconds ?? "5");
        i += 1;
        break;
      }
      case "--finish":
        options.finish = true;
        break;
      default:
        throw new Error(`unknown flag \`${flag}\``);
    }
  }
  if (!options.thread) throw new Error("--thread is required");
  return options;
}

const options = parseArgs(process.argv.slice(2));

/** One line of the desk's own narration. */
function say(line: string): void {
  console.log(`[desk] ${line}`);
}

// --- the desk's ledgers -----------------------------------------------------
//
// Ordinary files, appended to by the tool bodies, exactly as `follow-up`'s MCP
// server keeps its reminders. They are the billing system's records rather than
// salvor's, which is the point: salvor holds the log of what was asked for and
// what came back, and the money lives on the far side of that reference.

const SCRATCH = process.env.SALVOR_EXAMPLE_SCRATCH ?? tmpdir();
mkdirSync(SCRATCH, { recursive: true });

const LEDGERS = {
  lookups: join(SCRATCH, "salvor-langchain-ts-lookups.jsonl"),
  refunds: join(SCRATCH, "salvor-langchain-ts-refunds.jsonl"),
  large: join(SCRATCH, "salvor-langchain-ts-large-refunds.jsonl"),
};

function append(path: string, row: unknown): void {
  appendFileSync(path, `${JSON.stringify(row)}\n`);
}

/** Every row a ledger holds, oldest first. A missing ledger holds nothing. */
function rows(path: string): Record<string, unknown>[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line) as Record<string, unknown>);
}

// --- the desk's order book --------------------------------------------------
//
// The stand-in for a real order system. Every tool resolves the amount here,
// keyed by the order id, rather than trusting an amount from the caller.

const ORDER_BOOK: Record<string, { status: string; total_cents: number }> = {
  "ORD-7781": { status: "paid", total_cents: 4200 },
  "ORD-8120": { status: "paid", total_cents: 15900 },
  "ORD-3050": { status: "paid", total_cents: 2500 },
  "ORD-9002": { status: "paid", total_cents: 1500 },
  "ORD-4400": { status: "paid", total_cents: 240000 },
  "ORD-5150": { status: "paid", total_cents: 3300 },
};

/**
 * The desk's own limit, matching the `maximum` on `refund-order.toml` and the
 * `minimum` on `refund-large.toml`. The model routes by it; the operator's
 * schemas are what actually enforce it.
 */
const LARGE_REFUND_CENTS = 100_000;

function dollars(cents: number): string {
  return `$${(cents / 100).toFixed(2)}`;
}

// --- the tools --------------------------------------------------------------

/** How many tool bodies this process actually ran. Replay leaves it at zero. */
let toolBodies = 0;

async function pause(seconds: number): Promise<void> {
  await new Promise((resume) => setTimeout(resume, seconds * 1000));
}

/** The slow-tool flag, so `run.sh` can hold a thread long enough to contest it. */
async function maybeSlow(name: string): Promise<void> {
  if (options.slowTool !== name) return;
  say(`SLOW TOOL: ${name} is holding the thread for ${options.slowSeconds}s`);
  await pause(options.slowSeconds);
}

const lookupOrder = tool(
  async ({ order_id }: { order_id: string }) => {
    toolBodies += 1;
    await maybeSlow("lookup_order");
    const order = ORDER_BOOK[order_id];
    if (!order) throw new Error(`no order named ${order_id}`);
    append(LEDGERS.lookups, { order_id, ...order });
    say(`lookup_order ran: ${order_id} is ${order.status}, ${order.total_cents} cents`);
    return { order_id, status: order.status, total_cents: order.total_cents };
  },
  {
    name: "lookup_order",
    description: "Look up an order that has already been placed.",
    schema: z.object({ order_id: z.string() }),
  },
);

/**
 * The money, for both refund tools.
 *
 * The idempotency key comes from salvor: `currentToolCall()` hands back the key
 * it derived for this call, and what it derived it from is the operator's
 * choice, not the desk's. `refund_large` names no key fields, so its key is
 * positional, a hash of `(run, seq, tool)`: an attempt identifier, the same
 * string on every attempt at that one call. `refund_order` declares
 * `idempotency_key = ["order_id"]`, so its key is a hash of
 * `(run, tool, order_id)` with no position in it, and the same order refunded
 * twice in one run derives one key both times.
 *
 * A real desk passes that key to its payment provider as the provider's own
 * idempotency token. This one has no provider, so the ledger IS the provider: a
 * key already on file returns the refund that key produced, and no second line
 * is written. That is what makes the crash proof in `run.sh` cost one refund
 * rather than two.
 */
async function performRefund(
  toolName: string,
  ledger: string,
  args: { order_id: string; amount_cents: number },
): Promise<Record<string, unknown>> {
  toolBodies += 1;
  await maybeSlow(toolName);

  const call = currentToolCall();
  const key = call?.key ?? "no-key";

  const onFile = rows(ledger).find((row) => row.idempotency_key === key);
  if (onFile) {
    say(`${toolName}: key ${key.slice(0, 20)}... is already on file; no second refund`);
    return {
      order_id: onFile.order_id,
      amount_cents: onFile.amount_cents,
      refund_id: onFile.refund_id,
      status: "succeeded",
    };
  }

  const refund = {
    order_id: args.order_id,
    amount_cents: args.amount_cents,
    refund_id: `re_${key.slice(-12)}`,
    status: "succeeded",
  };
  append(ledger, { ...refund, tool: toolName, idempotency_key: key });
  say(`${toolName} moved money: ${dollars(args.amount_cents)} on ${args.order_id} as ${refund.refund_id}`);

  // The crash the whole design is for: the refund has happened and the ledger
  // says so, and this process dies before salvor is told. The log is left
  // ending at this call's intent.
  if (options.crashIn === toolName) {
    say(`crashing inside ${toolName}, after the money moved and before salvor heard`);
    process.exit(9);
  }

  return refund;
}

const refundOrder = tool(
  async (args: { order_id: string; amount_cents: number }) =>
    performRefund("refund_order", LEDGERS.refunds, args),
  {
    name: "refund_order",
    description: "Refund an order in full, up to the desk's own limit.",
    schema: z.object({ order_id: z.string(), amount_cents: z.number().int() }),
  },
);

const refundLarge = tool(
  async (args: { order_id: string; amount_cents: number }) =>
    performRefund("refund_large", LEDGERS.large, args),
  {
    name: "refund_large",
    description: "Refund an order too large for the desk to close on its own say-so.",
    schema: z.object({ order_id: z.string(), amount_cents: z.number().int() }),
  },
);

// --- the model --------------------------------------------------------------

interface Turn {
  content: string;
  toolCalls?: { name: string; args: Record<string, unknown>; id: string }[];
}

/** The last tool result in the conversation, parsed back from its message. */
function lastToolResult(messages: BaseMessage[]): Record<string, unknown> {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (messages[i].getType() === "tool") {
      return JSON.parse(String(messages[i].content)) as Record<string, unknown>;
    }
  }
  return {};
}

/**
 * What the desk's model says next, decided entirely by the conversation so far.
 *
 * Turn 0 looks the order up. Turn 1 reads that lookup out of the tool message
 * and either refunds (through the tool the amount calls for) or answers.
 * Turn 2 reads the refund out of its tool message and closes out. A real model
 * would decide the same three things from the same three inputs, which is why
 * swapping `ChatAnthropic` in below changes nothing else.
 *
 * One question takes a shorter path: a ticket that names its own amount and
 * says the refund is on it twice. There is nothing to look up, and a model
 * reading a duplicated line item asks for the refund twice in the one turn.
 * That is the shape `refund_order`'s declared `idempotency_key` exists for.
 */
function nextTurn(messages: BaseMessage[]): Turn {
  const question = String(messages.find((m) => m.getType() === "human")?.content ?? "");
  const orderId = question.match(/ORD-\d+/)?.[0] ?? "ORD-0000";
  const wantsRefund = /refund/i.test(question);
  const listedTwice = /\btwice\b/i.test(question);
  const statedCents = Number(question.match(/(\d+) cents/)?.[1] ?? 0);
  const turn = messages.filter((m) => m.getType() === "ai").length;

  if (turn === 0 && wantsRefund && listedTwice && statedCents > 0) {
    return {
      content: `Refunding ${orderId}; the ticket lists it twice.`,
      toolCalls: [
        {
          name: "refund_order",
          args: { order_id: orderId, amount_cents: statedCents },
          id: "call-refund-first",
        },
        // The same arguments, a second time. The two calls need distinct ids
        // because that is how LangChain tells one tool call from another, and
        // how the middleware ranks them within the turn.
        {
          name: "refund_order",
          args: { order_id: orderId, amount_cents: statedCents },
          id: "call-refund-again",
        },
      ],
    };
  }

  if (turn === 0) {
    return {
      content: `Looking up ${orderId}.`,
      toolCalls: [{ name: "lookup_order", args: { order_id: orderId }, id: "call-lookup" }],
    };
  }

  if (turn === 1 && !listedTwice) {
    const order = lastToolResult(messages);
    const total = Number(order.total_cents ?? 0);
    if (!wantsRefund) {
      return { content: `${orderId} is ${order.status}, ${dollars(total)}. Nothing to refund.` };
    }
    const name = total >= LARGE_REFUND_CENTS ? "refund_large" : "refund_order";
    return {
      content: `Refunding ${orderId}.`,
      toolCalls: [
        { name, args: { order_id: orderId, amount_cents: total }, id: "call-refund" },
      ],
    };
  }

  const refund = lastToolResult(messages);
  return {
    content:
      `Refunded ${dollars(Number(refund.amount_cents ?? 0))} on ${orderId};` +
      ` the provider has it as ${refund.refund_id}.`,
  };
}

/**
 * A hand-rolled model, not one of `@langchain/core/utils/testing`'s fakes:
 * `FakeStreamingChatModel` answers every turn with its first response, so a
 * tool-calling agent loops on the same tool forever, and `FakeToolCallingModel`'s
 * `bindTools` rebuilds itself on every call, which silently drops anything
 * attached to the instance.
 */
class ScriptedModel extends BaseChatModel {
  /** How many times this process actually called a model. Replay leaves it at zero. */
  calls = 0;

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
    this.calls += 1;
    const step = nextTurn(messages);
    const message = new AIMessage({
      content: step.content,
      tool_calls: step.toolCalls?.map((call) => ({ ...call, type: "tool_call" as const })),
    });
    return { generations: [{ text: step.content, message }] };
  }
}

const scripted = new ScriptedModel();

/**
 * The real provider, when there is a key for one. Nothing else in this file
 * changes: the tools, the middleware, the thread id and every proof `run.sh`
 * makes are the same, because salvor records the call and never the provider.
 */
async function chooseModel(): Promise<{ model: unknown; name: string }> {
  if (!process.env.ANTHROPIC_API_KEY) {
    return { model: scripted, name: "scripted (no ANTHROPIC_API_KEY set)" };
  }
  const { ChatAnthropic } = await import("@langchain/anthropic");
  const name = process.env.SALVOR_LC_MODEL ?? "claude-opus-5";
  return { model: new ChatAnthropic({ model: name }), name: `ChatAnthropic ${name}` };
}

// --- what a message says about itself ---------------------------------------

/**
 * The marker the middleware puts on every AI message it returns:
 * `replayed` when the answer came out of the log, `live` when this invoke
 * really called the model on a path the log still agrees with, and `forked`
 * from the point the invoke left the recorded path onward.
 */
function markerOf(message: BaseMessage): string {
  const mark = (message.response_metadata as Record<string, any> | undefined)?.salvor;
  if (!mark) return "none";
  if (mark.replayed) return `replayed@${mark.seq}`;
  if (mark.live) return `live@${mark.seq}`;
  if (mark.forked) return `forked@${mark.forked.at}`;
  return "unknown";
}

// --- the run ----------------------------------------------------------------

let forks = 0;
let markers: string[] = [];

/** The counts every path prints, so a refused invoke says what it did not do. */
function printCounts(modelCalls: string): void {
  console.log(`MODEL CALLS: ${modelCalls}`);
  console.log(`TOOL BODIES: ${toolBodies}`);
  console.log(`MARKERS: ${markers.join(",") || "none"}`);
  console.log(`FORKS: ${forks}`);
}

/** A message flattened to one line, so a shell can grep a sentence out of it. */
function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

async function main(): Promise<void> {
  const client = new SalvorClient(options.server);
  const runId = await runIdForThread(options.thread);
  console.log(`RUN: ${runId}`);
  console.log(`THREAD: ${options.thread}`);

  // Closing the thread out. A thread's run stays open until something says it
  // is over, because a task that looks finished today may get one more turn
  // tomorrow.
  if (options.finish) {
    const finished = await finishThread(client, options.thread);
    console.log(`FINISHED: run=${finished.runId} seq=${finished.seq}`);
    return;
  }

  const { model, name } = await chooseModel();
  say(`model: ${name}`);

  const agent = createAgent({
    model: model as never,
    tools: [lookupOrder, refundOrder, refundLarge] as never,
    middleware: [
      salvorMiddleware({
        client,
        // A fork is not an error: the invoke carries on and appends to the log.
        // This is where an application routes the notice; the default warns on
        // the console.
        onFork: (notice) => {
          forks += 1;
          say(`FORK at seq ${notice.at}: ${oneLine(notice.message)}`);
        },
      }),
    ],
  });

  const modelCalls = () =>
    process.env.ANTHROPIC_API_KEY ? "unavailable (real provider)" : String(scripted.calls);

  try {
    const answer = await agent.invoke(
      { messages: [{ role: "user", content: options.ask }] },
      { configurable: { thread_id: options.thread } },
    );
    markers = answer.messages.filter((m) => m.getType() === "ai").map(markerOf);
    printCounts(modelCalls());
    console.log(`ANSWER: ${oneLine(String(answer.messages.at(-1)?.content ?? ""))}`);
  } catch (error) {
    const refusal = salvorError(error);
    if (!refusal) throw error; // the app's own error, unchanged

    if (refusal instanceof ToolNeedsResolution) {
      // A `trust_completion = false` tool ran and salvor will not take this
      // process's word for what it did. The run holds the intent; a person
      // confirms the refund and records it.
      printCounts(modelCalls());
      console.log(
        `NEEDS RESOLUTION: ${JSON.stringify({
          run: refusal.run,
          seq: refusal.seq,
          tool: refusal.tool,
          key: refusal.key,
          output: refusal.output,
        })}`,
      );
      say(oneLine(refusal.message));
      process.exit(4);
    }

    printCounts(modelCalls());
    console.log(`REFUSED ${refusal.code}: ${oneLine(refusal.message)}`);
    if (refusal.lapsesInSeconds !== undefined) {
      console.log(`LAPSES IN: ${refusal.lapsesInSeconds}`);
    }
    process.exit(3);
  }
}

main().catch((error) => {
  console.error(`[desk] unhandled: ${error?.stack ?? error}`);
  process.exit(1);
});
