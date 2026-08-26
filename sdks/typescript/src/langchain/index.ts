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
import { LeaseHeldError, SalvorApiError } from "../errors.js";
import type { ClientToolDecl, Usage } from "../types.js";
import { runWithToolCall } from "./current_call.js";
import { SalvorMiddlewareError, ToolNeedsResolution, salvorError } from "./errors.js";
import { canonicalJson, hashValue, runIdForThread } from "./hash.js";
import { ReplayChatModel } from "./replay_model.js";
import { canonicalRequest, requestHash } from "./request.js";
import { RunTape } from "./tape.js";

export { currentToolCall } from "./current_call.js";
export type { CurrentToolCall } from "./current_call.js";
export { SalvorMiddlewareError, ToolNeedsResolution, salvorError } from "./errors.js";
export type {
  SalvorErrorCode,
  SalvorMiddlewareErrorDetails,
  ToolNeedsResolutionDetails,
} from "./errors.js";
export { finishThread } from "./finish.js";
export type { FinishedThread } from "./finish.js";
export { canonicalJson, hashValue, isUuid, runIdForThread } from "./hash.js";
export { ReplayChatModel } from "./replay_model.js";
export { canonicalRequest, requestHash } from "./request.js";
export { RunTape } from "./tape.js";
export type { ModelOutcome, RunTapeOptions, ToolOutcome, TurnPosition } from "./tape.js";

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
  /**
   * Called once, at most, per invoke that leaves its recorded path.
   *
   * The default warns on `console`. Replace it to route the notice wherever
   * this application's other operational surprises go, or pass a no-op to
   * silence it; the marker on the messages themselves (see
   * {@link SalvorForkMark}) is there either way, so silencing the warning
   * loses the announcement and not the evidence.
   */
  onFork?: (notice: SalvorForkNotice) => void;
}

/** What the middleware reports when an invoke leaves its recorded path. */
export interface SalvorForkNotice {
  /** The log position the invoke asked for something the log does not hold at. */
  at: number;
  /** The LangGraph thread it happened on. */
  thread: string;
  /** The run behind that thread. */
  run: string;
  /** The sentence the default handler warns with. */
  message: string;
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
 * The marker a message this invoke actually performed carries, while the
 * invoke is still on its recorded path. It exists so that the absence of a
 * marker never has to be interpreted: a message from this middleware always
 * says which of the three things it is.
 */
export interface SalvorLiveMark {
  /** Always true: this call was performed now, not read back. */
  live: true;
  /** The log position it was recorded at. */
  seq: number;
  /** The run it was recorded in. */
  run: string;
}

/**
 * The marker every message carries after its invoke left the recorded path.
 *
 * `at` is the seq the tape asked for something the log does not hold at, which
 * is the position to look at when working out why. Everything after it is a
 * live call appended past the recorded history, so the marker stays on every
 * later message of the invoke rather than only the first.
 */
export interface SalvorForkMark {
  forked: {
    /** The log position the invoke diverged at. */
    at: number;
    /** The LangGraph thread it diverged on. */
    thread: string;
    /** The run behind that thread. */
    run: string;
  };
}

/** What `response_metadata.salvor` holds on a message this middleware returned. */
export type SalvorMark = SalvorReplayMark | SalvorLiveMark | SalvorForkMark;

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
  const onFork =
    options.onFork ?? ((notice: SalvorForkNotice) => console.warn(notice.message));

  /** One tape per live invocation, keyed by run id. */
  const tapes = new Map<string, RunTape>();
  /** In-flight opens, so a turn's parallel tool calls share one open. */
  const opening = new Map<string, Promise<RunTape>>();

  /**
   * The client-tool declarations this server holds, by name, fetched once and
   * shared by every thread this middleware instance drives. `trust_completion`
   * is the operator's call, not each tool call's, so one listing serves the
   * whole instance rather than one per run.
   */
  let decls: Map<string, ClientToolDecl> | undefined;
  /** An in-flight refresh, so two tools missing from the cache at once share one fetch. */
  let refreshingDecls: Promise<Map<string, ClientToolDecl>> | undefined;

  /** Fetch the listing again and replace the cache with it. */
  function refreshDecls(): Promise<Map<string, ClientToolDecl>> {
    if (!refreshingDecls) {
      refreshingDecls = client
        .listClientTools()
        .then((list) => {
          decls = new Map(list.map((decl) => [decl.name, decl]));
          return decls;
        })
        .finally(() => {
          refreshingDecls = undefined;
        });
    }
    return refreshingDecls;
  }

  /**
   * Whether `tool`'s declaration lets a client close its own call.
   *
   * The cache is refreshed once, lazily, the first time a name it does not
   * hold is asked for: a tool declared after this middleware started, or the
   * very first call this instance ever makes. A name still missing after that
   * refresh is left to the server's own `unknown_tool` refusal (see
   * `undeclaredToolError`) rather than guessed at here; `false` is returned
   * in the meantime because a call this middleware cannot vouch for is safer
   * left for a person than reported on trust it was never given.
   */
  async function trustCompletionFor(tool: string): Promise<boolean> {
    if (!decls?.has(tool)) await refreshDecls();
    return decls?.get(tool)?.trustCompletion ?? false;
  }

  /** The thread this hook is running for, and the run id it maps to. */
  async function identify(
    runtime: unknown,
  ): Promise<{ threadId: string; runId: string }> {
    const threadId = (runtime as { configurable?: { thread_id?: unknown } } | undefined)
      ?.configurable?.thread_id;
    if (threadId === undefined || threadId === null) {
      throw new SalvorMiddlewareError(
        "salvorMiddleware needs a thread id: invoke the agent with " +
          '`{ configurable: { thread_id: "..." } }`. The thread id is the run id, ' +
          "so without one there is nothing for a later invoke to resume.",
        { code: "thread_id_missing" },
      );
    }
    // A thread id that is there but is not a usable one gets its own refusal,
    // naming what arrived. The two are different mistakes with different
    // fixes: nothing was passed, or something was, and it came out of a
    // template, a database column or a counter as the wrong sort of value.
    // Told apart here rather than downstream, because the downstream symptom
    // of a numeric thread id is a run id hashed from `String(7)`, which
    // resumes nothing and looks like a durability bug.
    if (typeof threadId !== "string" || threadId.length === 0) {
      throw new SalvorMiddlewareError(
        `salvorMiddleware needs a thread id that is a non-empty string; this invoke ` +
          `passed ${describeThreadId(threadId)}. Pass ` +
          '`{ configurable: { thread_id: "order-7781" } }`, converting whatever names ' +
          "the task (an order number, a job id) to a string yourself, so the id salvor " +
          "records is the id your application means.",
        { code: "thread_id_invalid" },
      );
    }
    return { threadId, runId: await toRunId(threadId) };
  }

  async function openTape(runId: string, threadId: string): Promise<RunTape> {
    let driver: ClientRunDriver;
    try {
      driver = await client.openClientRun({ runId, recordPrompts });
    } catch (error) {
      throw openRefusalError(error, threadId, runId);
    }
    const tail = driver.logEnvelopes.at(-1);
    if (tail?.kind === "RunCompleted") {
      throw new SalvorMiddlewareError(
        `thread \`${threadId}\` (run ${runId}) is finished: \`finishThread\` recorded ` +
          "its `RunCompleted`, and a completed run cannot be appended to. Give the " +
          "next task a new thread id.",
        { code: "thread_finished" },
      );
    }
    return RunTape.open(
      driver,
      {
        agent_def_hash: await hashValue({
          middleware: "@salvor-run/client/langchain",
        }),
        input: { thread_id: threadId },
      },
      {
        threadId,
        recordPrompts,
        // Called only when the server has forgotten the run entirely
        // (`unknown_run`, a salvor restart): the lease registry does not
        // survive the process, so re-opening with the same run id adopts it
        // back off its recorded log and mints a fresh lease, which is all the
        // tape needs to carry on from where it stood. A step refused because
        // another driver actively holds the lease is not retried this way at
        // all; see `tape.ts`'s own `lease`.
        reopen: async () => {
          try {
            return await client.openClientRun({ runId, recordPrompts });
          } catch (error) {
            throw openRefusalError(error, threadId, runId);
          }
        },
      },
    );
  }

  /**
   * Hand the thread's run back, because this invoke is over: the next process
   * to invoke this thread then takes it on its very next request instead of
   * being refused `lease_held` for the rest of the TTL. That is the whole
   * difference between a short-lived process that hands over cleanly and one
   * that locks its successor out for a minute for nothing.
   *
   * The tape itself is deliberately left in the map on an error path. Nothing
   * should still be stepping this run once the invoke has ended, but if
   * something is (one of a turn's parallel calls, still in flight while
   * another threw), meeting the released lease through the tape it already
   * holds is a `unknown_run` its own `lease()` takes the run back up from,
   * with its cursor intact. Dropping the tape here would instead hand that
   * straggler a fresh one, whose cursor starts back at the top of the log.
   * `beforeAgent` clears the map at the start of every invoke, so nothing is
   * inherited across one.
   *
   * `error` is what ended the invoke, when something did. Two of them mean the
   * lease is NOT this invoke's to hand back, and both are left strictly alone:
   * `lease_held` (this invoke never took the run, another driver has it) and
   * `lease_lost` (the run was taken mid-invoke, so the lease being held now is
   * that other driver's). Releasing on either would be this process ending
   * somebody else's hold. Every other ending releases, the ordinary success
   * included: a thrown tool body, a `ToolNeedsResolution` stop, a LangChain
   * error on its way through. A fork is not an ending at all and never reaches
   * here.
   */
  async function letGo(tape: RunTape, error?: unknown): Promise<void> {
    if (error !== undefined) {
      const code = salvorError(error)?.code;
      if (code === "lease_held" || code === "lease_lost") return;
    }
    await tape.release();
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

    /**
     * Let go of the run: the log is the durable part, the cursor is not, and
     * the lease belongs to whoever is driving, which after this hook is
     * nobody.
     */
    afterAgent: async (_state: unknown, runtime: unknown) => {
      const { runId } = await identify(runtime);
      const tape = tapes.get(runId);
      tapes.delete(runId);
      if (tape) await letGo(tape);
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
      try {
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
        announceFork(tape, onFork);
        if (!outcome.replayed && live) {
          tape.noteTurn(live);
          // Marked only now, after the answer was recorded: what the log holds
          // is the model's own message, and the provenance of a message is
          // this middleware's note to the reader, not part of the recorded
          // answer.
          return mark(live, markFor(tape, outcome.seq, false));
        }
        // The recorded answer goes back through LangChain's own handler, with a
        // stand-in model in the provider's place, so a streaming caller sees the
        // replayed turn arrive whole instead of seeing nothing at all. See
        // `replay_model.ts` for why that indirection is worth having.
        const recorded = mark(
          storedToAiMessage(outcome.response, tape.runId),
          markFor(tape, outcome.seq, true),
        );
        tape.noteTurn(recorded);
        return await handler({ ...request, model: new ReplayChatModel(recorded) });
      } catch (error) {
        // An error here leaves the invoke: LangGraph does not catch what a
        // model wrapper throws, so this hook is one of the two places an
        // invoke actually ends when it ends badly. A raw refusal from the
        // control plane is named before it leaves, so it reaches the caller
        // through `salvorError` instead of as a bare `SalvorApiError` nobody
        // can catch by code. Hand the lease back on the way out, unless the
        // error IS the lease being somebody else's.
        const refusal = serverRefusalError(error, tape);
        await letGo(tape, refusal);
        throw refusal;
      }
    },

    /**
     * Record the tool call, or return the recorded result.
     *
     * The intent goes in before the tool runs, which is the write-ahead rule:
     * a call that was asked for and never reported is visible in the log as
     * exactly that, rather than being indistinguishable from a call nobody
     * made. `tape.positionOf` finds this call's rank in the AI message that
     * listed it, and the tape's turnstile admits a turn's calls in that rank
     * order, so several tools asked for at once are recorded one after
     * another in the order the model listed them, and none of them is
     * refused.
     *
     * `trustCompletionFor` says whether THIS tool's own declaration lets a
     * client close its call by reporting on it. When it does not, the tool
     * still runs (its effect already happened; refusing to run it fixes
     * nothing), but `tape.toolCall` throws {@link ToolNeedsResolution}
     * instead of recording a completion salvor would refuse anyway, and the
     * intent is left for a person to settle.
     */
    wrapToolCall: async (request: any, handler: any) => {
      const tape = await tapeFor(request.runtime);
      try {
        return await recordToolCall(tape, request, handler);
      } catch (error) {
        // The other place an invoke ends badly: a tool body that threw, a
        // `ToolNeedsResolution` stop, an undeclared tool, or a completion the
        // control plane refused outright (a schema violation, a
        // `require_equal` mismatch). All of them leave the invoke, and all of
        // them should leave the run free for the next process. The exception,
        // as ever, is a lease that was never ours.
        const refusal = serverRefusalError(error, tape);
        await letGo(tape, refusal);
        throw refusal;
      }
    },
  });

  /**
   * The body of `wrapToolCall`, lifted out of its own error handling so the
   * hook above reads as the one sentence it is: record the call, and whatever
   * happens, do not walk out of the invoke still holding the run.
   */
  async function recordToolCall(
    tape: RunTape,
    request: any,
    handler: any,
  ): Promise<ToolMessage> {
    const name: string = request.toolCall.name;
    const args = request.toolCall.args ?? {};
    const callId: string = request.toolCall.id ?? "";
    const position = tape.positionOf(callId, name);
    const trustCompletion = await trustCompletionFor(name);
    let live: ToolMessage | undefined;

    const outcome = await tape
      .toolCall(
        name,
        args,
        async ({ seq, idempotencyKey }) =>
          runWithToolCall(
            { key: idempotencyKey, seq, runId: tape.runId, tool: name },
            async () => {
              const result = await handler(request);
              if (!ToolMessage.isInstance(result)) {
                throw new SalvorMiddlewareError(
                  `the tool \`${name}\` returned a LangGraph Command rather than a ` +
                    "tool message. A Command is graph control flow, not a recorded " +
                    "result, so this middleware cannot put it in the log. Return a " +
                    "value or a ToolMessage from tools you want recorded.",
                  { code: "tool_returned_command" },
                );
              }
              live = result;
              return toolOutput(result);
            },
          ),
        position,
        trustCompletion,
      )
      .catch((error: unknown) => {
        throw undeclaredToolError(error, name);
      });

    announceFork(tape, onFork);
    // One serialisation, used by both branches on purpose. The model reads a
    // tool result as text, and the text a replay produces is the text the
    // live call produced, byte for byte, or the next model call's request
    // hash misses and the thread forks on nothing but key order. See
    // `toolContent`.
    const content = toolContent(outcome.output);
    const marker = markFor(tape, outcome.seq, outcome.replayed);
    if (!outcome.replayed && live) {
      return mark(canonicalize(live, content), marker);
    }
    return mark(
      new ToolMessage({
        content,
        tool_call_id: request.toolCall.id ?? "",
        name,
        // A recorded completion is, by construction, a call that reported a
        // result: salvor refuses to record one any other way.
        status: "success",
      }),
      marker,
    );
  }
}

/**
 * What arrived where a thread id should have been, for the message that says
 * so. A string is quoted (an empty one is the case worth naming outright);
 * anything else is named by its type and its value, both, because "a number"
 * without the number is one round trip short of useful.
 */
function describeThreadId(threadId: unknown): string {
  if (typeof threadId === "string") return "an empty string";
  const value =
    typeof threadId === "object" ? JSON.stringify(threadId) : String(threadId);
  return `a ${typeof threadId} (${value})`;
}

/**
 * Which of the three things this message is.
 *
 * A fork wins over "live", because after a fork "live" is no longer the
 * interesting fact about a call: everything is live once the recorded path is
 * behind you, and what a reader needs to know is that the thread stopped
 * matching its own history and where.
 */
function markFor(tape: RunTape, seq: number, replayed: boolean): SalvorMark {
  if (replayed) return { replayed: true, seq, run: tape.runId };
  const at = tape.forkedAt;
  if (at !== undefined) {
    return { forked: { at, thread: tape.threadId, run: tape.runId } };
  }
  return { live: true, seq, run: tape.runId };
}

/**
 * Say once, per invoke, that this one left its recorded path.
 *
 * A fork is not an error: the run carries on, appended past its recorded
 * history, and everything from here is performed for real. It is worth saying
 * out loud all the same, because the usual cause is something the operator can
 * fix and would otherwise never see: a tool whose result is not the same twice,
 * or a graph that branches on the clock. The middleware cannot tell which, so
 * it names the position and the two things to look at.
 */
function announceFork(tape: RunTape, onFork: (notice: SalvorForkNotice) => void): void {
  if (!tape.announceFork()) return;
  const at = tape.forkedAt as number;
  onFork({
    at,
    thread: tape.threadId,
    run: tape.runId,
    message:
      `salvor: thread \`${tape.threadId}\` (run ${tape.runId}) left its recorded ` +
      `path at seq ${at}. Nothing from there replays: every model call and every ` +
      "tool call for the rest of this invoke is being performed and recorded " +
      "afresh, and the messages carry `response_metadata.salvor.forked` saying so. " +
      "If this thread was meant to resume, look for a tool whose result differs " +
      "between invokes, or a graph that branches on the clock or on randomness.",
  });
}

/**
 * A tool result as the text the model reads, the same text on both paths.
 *
 * The live call and the replay have to produce identical bytes here, and there
 * is only one serialisation both of them can reach: the canonical one. Salvor
 * stores a recorded output as JSON with its object keys sorted, so a tool that
 * returned `{ tracking_number, status, eta }` comes back `{ eta, status,
 * tracking_number }`, and a live message built from the tool's own key order
 * would not match the replayed one. The next model call hashes the tool result
 * it was given, so that mismatch is a thread that forks at the first model call
 * after a tool call and re-runs every write after it, on every invoke, forever.
 * A result that is not JSON is a string, and stays the string it is.
 */
function toolContent(output: unknown): string {
  return typeof output === "string" ? output : canonicalJson(output);
}

/**
 * The live tool message, carrying the canonical text rather than LangChain's
 * own stringification of the tool's return value. The serialization form is
 * rewritten too, for the same reason `mark` rewrites it: a message LangGraph
 * checkpoints and reads back is rebuilt from `lc_kwargs`, and content that
 * only half the message carried would come back as the half that forks.
 *
 * A message whose content is not text at all is left exactly as it is. That is
 * a tool that returned content blocks rather than a value, and rewriting those
 * into a JSON string would change what the model is shown in order to fix a
 * hash, which is the wrong way round.
 */
function canonicalize(message: ToolMessage, content: string): ToolMessage {
  if (typeof message.content !== "string") return message;
  message.content = content;
  const kwargs = (message as { lc_kwargs?: Record<string, unknown> }).lc_kwargs;
  if (kwargs) kwargs.content = content;
  return message;
}

/**
 * Put the salvor marker on a message. It goes on `response_metadata`, which is
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
  salvor: SalvorMark,
): T {
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
      { code: "unreadable_record" },
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
 * Turn every refusal an open (or re-open) can hit into the named error this
 * middleware surfaces, trying each of the two open-time refusals in turn and
 * returning anything else unchanged.
 *
 * `lease_held` is the one-driver case: another driver's lease on this run is
 * still current, live, right now. There is nothing to retry here, and this
 * open never mints a lease or records anything, so the invoke stops before a
 * single tool has run. `run_exists` is the older, unrelated refusal handled
 * by {@link serverDrivenRunError}.
 */
function openRefusalError(error: unknown, threadId: string, runId: string): unknown {
  if (error instanceof LeaseHeldError) {
    return new SalvorMiddlewareError(
      `thread \`${threadId}\` (run ${runId}) cannot be opened: another driver holds its ` +
        `lease right now, and it lapses in ${error.lapsesInSeconds}s if that driver goes ` +
        "quiet (or as soon as the run finishes). One driver per thread at a time. Wait " +
        "for the lease to lapse and invoke again, or confirm no other process is already " +
        "driving this thread.",
      {
        code: "lease_held",
        cause: error,
        lapsesInSeconds: error.lapsesInSeconds,
      },
    );
  }
  return serverDrivenRunError(error, threadId, runId);
}

/**
 * Turn the server's refusal to open a server-driven run for client-driven use
 * into a message naming the thread this middleware was asked to drive.
 *
 * A thread id maps to a run id (see `runIdForThread`), and nothing stops that
 * id from already naming a run someone started through the server-driven
 * `/v1/runs` path. Salvor refuses to adopt such a run as client-driven rather
 * than become a second writer on its log, and this middleware has no thread
 * name to offer back for it: the caller has to give this task a thread id
 * that has not already started a run the other way.
 */
function serverDrivenRunError(error: unknown, threadId: string, runId: string): unknown {
  if (!(error instanceof SalvorApiError) || error.code !== "run_exists") {
    return error;
  }
  return new SalvorMiddlewareError(
    `thread \`${threadId}\` (run ${runId}) cannot be opened for a client-driven run: ` +
      `${error.message}. Give this task a thread id that has never named a ` +
      "server-driven run.",
    { code: "run_exists", cause: error },
  );
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
    { code: "tool_undeclared", cause: error },
  );
}

/**
 * The catch-all for a `SalvorApiError` that reaches the edge of a hook without
 * having already been translated into one of this middleware's own codes: a
 * `bad_request` from an intent's input or a reported output that failed its
 * declared schema, a `client_completion_refused` from a `require_equal`
 * mismatch or a declaration that refuses self-completion outright, a
 * `divergence`, or any other code the server answers with.
 *
 * Left alone, that refusal would tear through `wrapToolCall` or
 * `wrapModelCall` as a bare `SalvorApiError`: `salvorError(e)` would return
 * `undefined` for it, and an application catching by code would never see it
 * coming, even though the server said exactly what went wrong. Wrapping it
 * here, at the one place every hook's error passes through on its way out, is
 * what makes it reachable the same way every other refusal is. The server's
 * own code is kept unchanged on `.code` and the `SalvorApiError` itself on
 * `.cause`, so an application that already matches on the server's vocabulary
 * (`bad_request`, `client_completion_refused`, ...) does not have to learn a
 * second one.
 *
 * Every refusal this middleware already gives its own name to is a
 * `SalvorMiddlewareError` (not a `SalvorApiError`) by the time it reaches
 * here (`lease_held`, `lease_lost`, `reopen_refused`, `run_exists`,
 * `tool_undeclared`, `open_intent`; `invalid_drive_token` and `unknown_run`
 * are resolved inside the tape's own `lease()` before either surfaces at
 * all), so this never overrides one of those; it only ever fires on the codes
 * nothing above has a name for yet.
 */
function serverRefusalError(error: unknown, tape: RunTape): unknown {
  if (!(error instanceof SalvorApiError)) return error;
  return new SalvorMiddlewareError(
    `thread \`${tape.threadId}\` (run ${tape.runId}): ${error.message}`,
    { code: error.code, cause: error },
  );
}
