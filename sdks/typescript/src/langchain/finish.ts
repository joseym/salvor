/**
 * `finishThread`: close a thread's run for good.
 *
 * A thread's run stays open by default. `salvorMiddleware` never appends
 * `RunCompleted` on its own, because there is no point in an agent's life
 * where LangGraph tells this middleware "this thread will never be invoked
 * again": a thread that looks done today may get one more turn tomorrow.
 * Deciding that a thread is actually finished is the operator's call, so it
 * gets its own function rather than something the middleware infers.
 *
 * Once `finishThread` has recorded `RunCompleted`, the run is closed the way
 * every salvor run is closed: nothing may be appended to it again. An
 * `agent.invoke` on that thread meets this in `beforeAgent` (see
 * `index.ts`), which opens the run, finds the log already ends at
 * `RunCompleted`, and throws {@link SalvorMiddlewareError} naming the thread
 * rather than letting the append fail somewhere less legible.
 */

import { mapStoredMessageToChatMessage } from "@langchain/core/messages";
import type { AIMessage } from "@langchain/core/messages";
import type { SalvorClient } from "../client.js";
import type { SalvorEvent } from "../types.js";
import { SalvorMiddlewareError, threadAbandonedError } from "./errors.js";
import { runIdForThread } from "./hash.js";

/** The receipt from finishing a thread: the run it closed and the seq `RunCompleted` landed at. */
export interface FinishedThread {
  runId: string;
  seq: number;
}

/**
 * Append `RunCompleted` to the run behind `threadId`, closing it.
 *
 * `threadIdToRunId` defaults to {@link runIdForThread}, the same mapping
 * {@link salvorMiddleware} uses by default; pass the same function an
 * application gave `salvorMiddleware` when it overrode the default, so
 * `finishThread` closes the run the middleware actually opened.
 *
 * Refused, appending nothing, as {@link SalvorMiddlewareError} when:
 * - the thread has never been invoked (its run holds no events at all);
 * - the run is already finished (its log already ends at `RunCompleted` or
 *   `RunFailed`);
 * - the run was abandoned (its log ends at `RunAbandoned`), which is
 *   `thread_abandoned` rather than `thread_finished`: an abandoned run was
 *   retired by hand, often on top of an open intent nobody is going to
 *   settle, so it is not the open-intent refusal either;
 * - the log ends at an open intent: a model or tool call salvor recorded as
 *   requested but never recorded as completed. That call has to be settled
 *   first (`salvor resolve <run> --store <path> --output <output>`, or
 *   `POST /v1/runs/{id}/resolve` on the server), because a `RunCompleted`
 *   appended past it would silently abandon whatever that call was doing.
 *
 * Closing a thread takes its lease to write the `RunCompleted`, and hands it
 * back on the way out, refusal or not. Nothing here holds a run any longer
 * than the one append it came to make.
 *
 * `output` defaults to the content of the last recorded AI message, read
 * back from the run's own log the same way a replayed model call is (see
 * `storedToAiMessage` in `index.ts`); when the log holds no such message, or
 * holds one this SDK cannot read back, the default is `null` rather than a
 * thrown error, because a thread is worth closing even when its last answer
 * cannot be recovered from the log.
 */
export async function finishThread(
  client: SalvorClient,
  threadId: string,
  output?: unknown,
  threadIdToRunId: (threadId: string) => string | Promise<string> = runIdForThread,
): Promise<FinishedThread> {
  const runId = await threadIdToRunId(threadId);
  const driver = await client.openClientRun({ runId });
  // Opening took the run's lease, and every path out of here is done with it:
  // the run is closed, or it was refused and nothing was written. Either way
  // the next caller should not wait out a TTL for a lease this function is no
  // longer using. A failure to hand it back is swallowed for the same reason
  // the middleware swallows one: the lapse is the safety net, and a goodbye
  // that did not arrive is not worth losing a recorded `RunCompleted` over.
  try {
    const log = driver.logEnvelopes;

    if (log.length === 0) {
      throw new SalvorMiddlewareError(
        `thread \`${threadId}\` (run ${runId}) has never been invoked, so there is ` +
          "no run to finish.",
        { code: "thread_never_invoked" },
      );
    }

    const tail = log[log.length - 1];
    // An abandoned run is finished too, and says so in its own words: an
    // operator retired it, and the open intent it was probably retired on top
    // of is not something anybody is going to resolve. Checked before the
    // open-intent rule below for exactly that reason.
    if (tail.kind === "RunAbandoned") {
      throw threadAbandonedError(threadId, runId);
    }
    if (tail.kind === "RunCompleted" || tail.kind === "RunFailed") {
      throw new SalvorMiddlewareError(
        `thread \`${threadId}\` (run ${runId}) is already finished.`,
        { code: "thread_finished" },
      );
    }
    if (tail.kind === "ModelCallRequested" || tail.kind === "ToolCallRequested") {
      const what = tail.kind === "ModelCallRequested" ? "a model call" : "a tool call";
      throw new SalvorMiddlewareError(
        `run ${runId} (thread \`${threadId}\`) ends at ${what} (seq ${tail.seq}) that ` +
          "was requested and never completed. Settle it first (`salvor resolve " +
          `${runId} --store <the server's store> --output '<json the call returned>'\`, ` +
          `or \`POST /v1/runs/${runId}/resolve\` on the server) and finish the thread ` +
          "again.",
        { code: "open_intent" },
      );
    }

    const resolvedOutput = output !== undefined ? output : lastAiMessageContent(log);
    const seq = tail.seq + 1;
    const appended = await driver.append([
      driver.envelope(seq, "RunCompleted", { output: resolvedOutput ?? null }),
    ]);
    return { runId, seq: appended[0] ?? seq };
  } finally {
    await driver.release().catch(() => undefined);
  }
}

/**
 * The content of the most recently recorded AI message, or `null` when the
 * log holds none or holds one shaped some other way than the LangChain
 * stored form this middleware writes.
 */
function lastAiMessageContent(log: SalvorEvent[]): unknown {
  for (let i = log.length - 1; i >= 0; i--) {
    if (log[i].kind !== "ModelCallCompleted") continue;
    const stored = log[i].payload.response as { type?: unknown; data?: unknown } | undefined;
    if (!stored || stored.type !== "ai" || typeof stored.data !== "object") return null;
    try {
      return (mapStoredMessageToChatMessage(stored as never) as AIMessage).content;
    } catch {
      return null;
    }
  }
  return null;
}
