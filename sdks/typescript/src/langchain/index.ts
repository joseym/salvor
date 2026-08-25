/**
 * `@salvor-run/client/langchain`: durability for an agent you already wrote.
 *
 * Add one middleware to `createAgent` and every model call and every tool call
 * the agent makes is recorded in a salvor client-driven run, keyed by the
 * LangGraph `thread_id`. Nothing else about the app changes: the same graph,
 * the same provider, the same key, the same tools. What changes is what a
 * second invoke of the same thread costs. The first invoke pays the provider
 * and runs the tools; a second invoke, after a crash or a redeploy or a retry,
 * meets the recorded answers at the positions the first one wrote them and
 * returns those instead.
 *
 * ```ts
 * import { createAgent } from "langchain";
 * import { SalvorClient } from "@salvor-run/client";
 * import { salvorMiddleware } from "@salvor-run/client/langchain";
 *
 * const agent = createAgent({
 *   model, tools,
 *   middleware: [salvorMiddleware({ client: new SalvorClient("http://127.0.0.1:8080") })],
 * });
 *
 * await agent.invoke({ messages }, { configurable: { thread_id: "order-7781" } });
 * ```
 *
 * # What this is, and what it is not
 *
 * It is a recorded effect ledger with exactly-once writes, under LangGraph's
 * orchestration. Every call that leaves the process goes through salvor's log
 * first, so nothing is paid for twice and no keyed write lands twice.
 *
 * It is not replay of the graph. LangGraph still owns the clock, the randomness
 * and the branch order; salvor sees only the calls, not the decisions between
 * them. A graph that branches on `Date.now()` will take a different branch on
 * the second invoke, and the middleware will honestly tell you so rather than
 * pretend the recorded answers still apply.
 *
 * # What the operator has to declare
 *
 * A tool's effect class, its input schema, its output schema and its
 * idempotency key are the operator's, not this middleware's. They come from a
 * client-tool declaration loaded by the server (`salvor serve --client-tool
 * <FILE>`; see `examples/client-tools/refund-card.toml`). The middleware sends
 * the tool's name and the model's arguments and nothing else, which is why a
 * tool nobody declared is refused by name instead of quietly recorded as a
 * harmless read.
 */

import { createMiddleware } from "langchain";
import type { AIMessage } from "@langchain/core/messages";
import {
  ToolMessage,
  mapStoredMessageToChatMessage,
} from "@langchain/core/messages";
import type { SalvorClient } from "../client.js";
import type { ClientRunDriver } from "../client_runs.js";
import { SalvorApiError } from "../errors.js";
import type { Usage } from "../types.js";
import { SalvorMiddlewareError } from "./errors.js";
import { hashValue, runIdForThread } from "./hash.js";
import { ReplayChatModel } from "./replay_model.js";
import { canonicalRequest, requestHash } from "./request.js";
import { RunTape } from "./tape.js";

export { SalvorMiddlewareError } from "./errors.js";
export { canonicalJson, hashValue, isUuid, runIdForThread } from "./hash.js";
export { ReplayChatModel } from "./replay_model.js";
export { canonicalRequest, requestHash } from "./request.js";
export { RunTape } from "./tape.js";
export type { ModelOutcome, ToolOutcome } from "./tape.js";

/** How this middleware is wired to a control plane. */
export interface SalvorMiddlewareOptions {
  /** The control plane every thread's run is opened against. */
  client: SalvorClient;
  /**
   * The run id for a LangGraph `thread_id`. The default is
   * {@link runIdForThread}: a thread id that is already a UUID is used as the
   * run id unchanged, anything else is hashed into one. Replace it when your
   * thread ids and your run ids are kept in a table somewhere.
   */
  threadIdToRunId?: (threadId: string) => string | Promise<string>;
  /**
   * Record each model request's body on its intent, so an inspector can show
   * the exact prompt. Off by default, because the body carries user data.
   * Replay never reads it: the correlation key is the request hash alone.
   */
  recordPrompts?: boolean;
}

/** The marker a replayed message carries on its `response_metadata`. */
export interface SalvorReplayMark {
  /** Always true: the marker exists only on a message salvor replayed. */
  replayed: true;
  /** The log position the recorded call was written at. */
  seq: number;
  /** The run the answer was read from. */
  run: string;
}

/**
 * A LangChain middleware that records this agent's model and tool calls in a
 * salvor run, and replays them on a re-invoke of the same thread.
 *
 * `wrapToolCall` exists only inside `createAgent`. A hand-built `StateGraph`
 * calling tools in its own node has no hook for the middleware to sit in, so
 * such a graph gets model recording only, and its tool calls stay outside the
 * ledger.
 */
export function salvorMiddleware(options: SalvorMiddlewareOptions) {
  const { client, recordPrompts = false } = options;
  const toRunId = options.threadIdToRunId ?? runIdForThread;

  /** One tape per live invocation, keyed by run id. */
  const tapes = new Map<string, RunTape>();
  /** In-flight opens, so a turn's parallel tool calls share one open. */
  const opening = new Map<string, Promise<RunTape>>();

  /** The thread this hook is running for, and the run id it maps to. */
  async function identify(
    runtime: unknown,
  ): Promise<{ threadId: string; runId: string }> {
    const threadId = (runtime as { configurable?: { thread_id?: unknown } } | undefined)
      ?.configurable?.thread_id;
    if (typeof threadId !== "string" || threadId.length === 0) {
      throw new SalvorMiddlewareError(
        "salvorMiddleware needs a thread id: invoke the agent with " +
          '`{ configurable: { thread_id: "..." } }`. The thread id is the run id, ' +
          "so without one there is nothing for a later invoke to resume.",
      );
    }
    return { threadId, runId: await toRunId(threadId) };
  }

  async function openTape(runId: string, threadId: string): Promise<RunTape> {
    const driver: ClientRunDriver = await client.openClientRun({
      runId,
      recordPrompts,
    });
    return RunTape.open(
      driver,
      {
        agent_def_hash: await hashValue({
          middleware: "@salvor-run/client/langchain",
        }),
        input: { thread_id: threadId },
      },
      recordPrompts,
    );
  }

  async function tapeFor(runtime: unknown): Promise<RunTape> {
    const { threadId, runId } = await identify(runtime);
    const existing = tapes.get(runId);
    if (existing) return existing;
    const inFlight = opening.get(runId);
    if (inFlight) return inFlight;
    const started = openTape(runId, threadId)
      .then((tape) => {
        tapes.set(runId, tape);
        return tape;
      })
      .finally(() => {
        opening.delete(runId);
      });
    opening.set(runId, started);
    return started;
  }

  return createMiddleware({
    name: "SalvorMiddleware",

    /**
     * Take up the thread's run for this invocation. Opening here rather than
     * lazily is what makes a second invoke start from a clean cursor: even an
     * invocation that failed halfway and never reached `afterAgent` leaves
     * nothing behind that the next one would inherit.
     */
    beforeAgent: async (_state: unknown, runtime: unknown) => {
      const { runId } = await identify(runtime);
      tapes.delete(runId);
      await tapeFor(runtime);
      return undefined;
    },

    /** Let go of the run. The log is the durable part; the cursor is not. */
    afterAgent: async (_state: unknown, runtime: unknown) => {
      const { runId } = await identify(runtime);
      tapes.delete(runId);
      return undefined;
    },

    /**
     * Record the model call, or return the recorded answer.
     *
     * The live call is LangChain's: the intent is opened with a hash of the
     * request, `handler` sends it with whatever provider and key the app
     * configured, and the answer is recorded. Salvor never sees the request and
     * never holds the key.
     */
    wrapModelCall: async (request: any, handler: any): Promise<AIMessage> => {
      const tape = await tapeFor(request.runtime);
      const hash = await requestHash(request);
      let live: AIMessage | undefined;
      const outcome = await tape.modelCall(
        hash,
        canonicalRequest(request),
        async () => {
          const answer: AIMessage = await handler(request);
          live = answer;
          return { response: answer.toDict(), usage: usageOf(answer) };
        },
      );
      if (!outcome.replayed && live) return live;
      // The recorded answer goes back through LangChain's own handler, with a
      // stand-in model in the provider's place, so a streaming caller sees the
      // replayed turn arrive whole instead of seeing nothing at all. See
      // `replay_model.ts` for why that indirection is worth having.
      const recorded = mark(
        storedToAiMessage(outcome.response, tape.runId),
        outcome.seq,
        tape.runId,
      );
      return handler({ ...request, model: new ReplayChatModel(recorded) });
    },

    /**
     * Record the tool call, or return the recorded result.
     *
     * The intent goes in before the tool runs, which is the write-ahead rule:
     * a call that was asked for and never reported is visible in the log as
     * exactly that, rather than being indistinguishable from a call nobody
     * made. The turnstile inside the tape is what lets a model turn ask for
     * several tools at once: they are recorded one after another, in the order
     * the model listed them, and none of them is refused.
     */
    wrapToolCall: async (request: any, handler: any) => {
      const tape = await tapeFor(request.runtime);
      const name: string = request.toolCall.name;
      const args = request.toolCall.args ?? {};
      let live: ToolMessage | undefined;

      const outcome = await tape
        .toolCall(name, args, async () => {
          const result = await handler(request);
          if (!ToolMessage.isInstance(result)) {
            throw new SalvorMiddlewareError(
              `the tool \`${name}\` returned a LangGraph Command rather than a ` +
                "tool message. A Command is graph control flow, not a recorded " +
                "result, so this middleware cannot put it in the log. Return a " +
                "value or a ToolMessage from tools you want recorded.",
            );
          }
          live = result;
          return toolOutput(result);
        })
        .catch((error: unknown) => {
          throw undeclaredToolError(error, name);
        });

      if (!outcome.replayed && live) {
        return live;
      }
      const content = outcome.output;
      return mark(
        new ToolMessage({
          content: typeof content === "string" ? content : JSON.stringify(content),
          tool_call_id: request.toolCall.id ?? "",
          name,
          // A recorded completion is, by construction, a call that reported a
          // result: salvor refuses to record one any other way.
          status: "success",
        }),
        outcome.seq,
        tape.runId,
      );
    },
  });
}

/**
 * Put the replay marker on a message. It goes on `response_metadata`, which is
 * the one place a message carries provenance rather than content, and it is
 * deliberately excluded from the request hash so that a replayed message fed
 * back into the next model call hashes exactly as the live one did.
 *
 * A replayed answer arrives whole. Under streaming that means one message
 * event with the full content, not a re-tokenised imitation of the original
 * stream: the tokens happened once, and nothing here pretends otherwise.
 */
function mark<T extends { response_metadata?: Record<string, unknown> }>(
  message: T,
  seq: number,
  run: string,
): T {
  const salvor: SalvorReplayMark = { replayed: true, seq, run };
  const metadata = { ...(message.response_metadata ?? {}), salvor };
  message.response_metadata = metadata;
  // The serialization form is written too, not just the field. A message that
  // LangGraph checkpoints and reads back is rebuilt from `lc_kwargs`, and a
  // marker only half of the message carried would be a marker that disappears
  // on the walk that most needs it.
  const kwargs = (message as { lc_kwargs?: Record<string, unknown> }).lc_kwargs;
  if (kwargs) kwargs.response_metadata = metadata;
  return message;
}

/** The token counts a run's budgets are held to, from wherever the model put them. */
function usageOf(message: AIMessage): Usage {
  const metadata = message.usage_metadata;
  if (metadata) {
    return {
      inputTokens: Number(metadata.input_tokens ?? 0),
      outputTokens: Number(metadata.output_tokens ?? 0),
    };
  }
  const reported = (message.response_metadata?.tokenUsage ??
    message.response_metadata?.usage) as Record<string, unknown> | undefined;
  if (reported) {
    return {
      inputTokens: Number(
        reported.promptTokens ?? reported.input_tokens ?? reported.prompt_tokens ?? 0,
      ),
      outputTokens: Number(
        reported.completionTokens ??
          reported.output_tokens ??
          reported.completion_tokens ??
          0,
      ),
    };
  }
  return { inputTokens: 0, outputTokens: 0 };
}

/**
 * The recorded response, back as the message LangChain returned. What goes into
 * the log is `AIMessage.toDict()`, LangChain's own storage form, so the answer
 * comes back with its content, its tool calls, its ids and its usage intact.
 */
function storedToAiMessage(stored: unknown, run: string): AIMessage {
  const record = stored as { type?: unknown; data?: unknown } | null;
  if (!record || record.type !== "ai" || typeof record.data !== "object") {
    throw new SalvorMiddlewareError(
      `run ${run} recorded a model response this middleware cannot read back. ` +
        "It expects a LangChain stored message (`{ type: \"ai\", data: {...} }`), " +
        "which is what it writes; a run driven by other code records other shapes.",
    );
  }
  return mapStoredMessageToChatMessage(stored as never) as AIMessage;
}

/**
 * What a tool call returned, as the value the operator's `output_schema`
 * describes.
 *
 * LangChain turns a tool's result into a tool message by stringifying it, so
 * the result is recovered by parsing the content back when the parse round
 * trips exactly. When it does not, the content is recorded as the string it is:
 * better a completion the operator's schema refuses, and says so, than a
 * silently reshaped result that replays as different bytes than the live call
 * produced.
 */
function toolOutput(message: ToolMessage): unknown {
  const content = message.content;
  if (typeof content !== "string") return JSON.parse(JSON.stringify(content));
  try {
    const parsed: unknown = JSON.parse(content);
    if (JSON.stringify(parsed) === content) return parsed;
  } catch {
    /* not JSON; the content is the result */
  }
  return content;
}

/**
 * Turn the server's `unknown_tool` refusal into the sentence that fixes it. The
 * middleware cannot declare the tool itself, and should not want to: a
 * declaration fixes whether a call is a write, and code that performs the write
 * must not be the code that decides that.
 */
function undeclaredToolError(error: unknown, tool: string): unknown {
  if (!(error instanceof SalvorApiError) || error.code !== "unknown_tool") {
    return error;
  }
  return new SalvorMiddlewareError(
    `the tool \`${tool}\` has no client-tool declaration on this salvor server, ` +
      "so its call cannot be recorded. Write a declaration for it: a TOML file " +
      `with \`name = "${tool}"\`, an \`effect\` (\`read\`, \`idempotent\` or ` +
      "`write`), an `[input_schema]` matching the tool's parameters, and, so the " +
      "middleware may record what the tool returned, `trust_completion = true` " +
      "with an `[output_schema]`. Then start the server with `salvor serve " +
      "--client-tool <FILE>`. See examples/client-tools/refund-card.toml.",
  );
}
