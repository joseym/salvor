/** The errors this middleware raises on its own account. */

import { SalvorError } from "../errors.js";
import { canonicalJson } from "./hash.js";

/**
 * The stable token on every {@link SalvorMiddlewareError}, for a caller that
 * branches on what happened rather than on a sentence.
 *
 * The first nine are the ones an application acts on:
 *
 * - `lease_held`: another driver holds this thread's run right now, and this
 *   invoke never started. Carries {@link SalvorMiddlewareError.lapsesInSeconds},
 *   the whole seconds until that hold lapses if the other driver goes quiet:
 *   back off for that long and invoke again.
 * - `lease_lost`: the run was taken mid-invoke, so a second driver is live on
 *   the thread now. Do not retry into it; find the other driver.
 * - `reopen_refused`: this invoke's token stopped working and taking the run
 *   up again was refused too, so the run cannot be driven from here at all.
 * - `thread_finished`: `finishThread` closed this thread's run. Give the next
 *   task a new thread id.
 * - `thread_id_missing`: the invoke carried no `configurable.thread_id`.
 * - `thread_id_invalid`: it carried one that is not a non-empty string.
 * - `tool_undeclared`: the tool the model called has no client-tool
 *   declaration on this server, so its call cannot be recorded.
 * - `tool_needs_resolution`: a `trust_completion = false` tool ran and its
 *   result is waiting on a person ({@link ToolNeedsResolution}, which carries
 *   the unrecorded output).
 * - `open_intent`: the run's log ends at a call that was requested and never
 *   completed. Settle it, then invoke again.
 *
 * The rest name conditions an application cannot usually do anything about
 * except read the message: `run_exists` (the thread id already names a
 * server-driven run), `thread_never_invoked`, `tool_returned_command` (a tool
 * returned graph control flow rather than a result), `call_unranked` (a tool
 * call the last recorded model turn does not list) and `unreadable_record` (the
 * log holds something at a position this middleware cannot read back).
 *
 * A fork is deliberately not in this list: leaving the recorded path is not an
 * error, it is reported through `onFork` and the messages' own markers.
 *
 * None of the above is the control plane's own refusal reaching you unnamed.
 * A `SalvorApiError` that escapes a hook without being translated into one of
 * the codes above keeps its own code here instead: `bad_request` (a call's
 * input, or a reported output, failed its declared schema),
 * `client_completion_refused` (a `require_equal` field was reported
 * differently than the intent recorded, or the declaration refuses
 * self-completion outright), `divergence`, `unknown_tool`, or whatever else
 * the server answers with. `cause` is always the `SalvorApiError` itself, so
 * an application that already matches on the server's own vocabulary does not
 * have to learn a second one. This union cannot enumerate that whole
 * vocabulary, so it stays open to any string rather than pretending to be
 * closed.
 */
export type SalvorErrorCode =
  | "lease_held"
  | "lease_lost"
  | "reopen_refused"
  | "thread_finished"
  | "thread_id_missing"
  | "thread_id_invalid"
  | "tool_undeclared"
  | "tool_needs_resolution"
  | "open_intent"
  | "run_exists"
  | "thread_never_invoked"
  | "tool_returned_command"
  | "call_unranked"
  | "unreadable_record"
  // Deliberately open, not a closed set: see the paragraph above.
  | (string & {});

/** What a {@link SalvorMiddlewareError} is told about itself when it is raised. */
export interface SalvorMiddlewareErrorDetails {
  /** The stable token a caller branches on. */
  code: SalvorErrorCode;
  /** The control-plane refusal underneath, when this error stands for one. */
  cause?: unknown;
  /** For `lease_held`: whole seconds until the other driver's hold lapses. */
  lapsesInSeconds?: number;
}

/**
 * Something the middleware itself refuses, as opposed to something the control
 * plane refused (which stays a `SalvorApiError`). Every message names the thread
 * or the tool it is about and what would fix it, because these all surface
 * inside somebody else's agent loop, far from this file.
 *
 * Every one carries a {@link SalvorErrorCode} on `code`, so a caller matches a
 * token rather than a sentence, and the refusal it stands for on `cause` when
 * there is one (the `SalvorApiError` the server answered with), so the
 * server's own words are never lost in the translation to this one's.
 *
 * Reach it with {@link salvorError}: `createAgent` wraps what a middleware
 * throws, and that helper covers the wrapped case and the bare one alike.
 */
export class SalvorMiddlewareError extends SalvorError {
  /** The stable token a caller branches on. */
  readonly code: SalvorErrorCode;
  /** The control-plane refusal underneath, when this error stands for one. */
  readonly cause?: unknown;
  /** For `lease_held`: whole seconds until the other driver's hold lapses. */
  readonly lapsesInSeconds?: number;

  constructor(message: string, details: SalvorMiddlewareErrorDetails) {
    super(message);
    this.code = details.code;
    this.cause = details.cause;
    this.lapsesInSeconds = details.lapsesInSeconds;
  }
}

/**
 * The middleware error behind whatever `agent.invoke` threw, or undefined when
 * it threw something else.
 *
 * Two shapes reach a caller and only one of them is documented anywhere.
 * `createAgent` wraps an error a middleware throws inside a graph node in its
 * own `MiddlewareError`, copying the name and the message but keeping the real
 * instance only on `.cause`; an error thrown from `beforeAgent` (the one-driver
 * refusal, a finished thread, a missing thread id) arrives bare, because there
 * is no node around it to wrap it. A caller that unwraps only one of the two
 * misses half the cases, silently, and usually the half it most wanted.
 *
 * So this walks the `cause` chain and hands back the first
 * {@link SalvorMiddlewareError} on it, the error itself included:
 *
 * ```ts
 * try {
 *   await agent.invoke(input, { configurable: { thread_id } });
 * } catch (e) {
 *   const refusal = salvorError(e);
 *   if (refusal?.code === "lease_held") {
 *     await sleep((refusal.lapsesInSeconds ?? 1) * 1000);
 *   } else if (!refusal) {
 *     throw e; // not salvor's: the app's own error, unchanged
 *   }
 * }
 * ```
 */
export function salvorError(error: unknown): SalvorMiddlewareError | undefined {
  let seen = error;
  for (let depth = 0; depth < 8 && seen !== undefined && seen !== null; depth += 1) {
    if (seen instanceof SalvorMiddlewareError) return seen;
    seen = (seen as { cause?: unknown }).cause;
  }
  return undefined;
}

/** What {@link ToolNeedsResolution} carries about the call it stopped on. */
export interface ToolNeedsResolutionDetails {
  /** The run stopped waiting on a person. */
  run: string;
  /** The log position the tool's intent was recorded at; its completion goes at `seq + 1`. */
  seq: number;
  /** The LangGraph thread behind that run. */
  thread: string;
  /** The tool's name, as the model invoked it. */
  tool: string;
  /** What the tool body returned, not yet recorded: what a person confirms before it counts. */
  output: unknown;
  /** The idempotency key salvor derived for this call. */
  key: string;
}

/**
 * Thrown after a tool declared `trust_completion = false` has run its body, in
 * place of reporting the result to salvor.
 *
 * The operator's declaration says a client's own report of what this tool did
 * is not enough to record: salvor refuses a client completion for such a tool
 * outright (`403 client_completion_refused`). Posting it anyway and letting
 * that refusal tear through LangGraph would surface a bare HTTP error after
 * the tool has already run and possibly moved money, naming neither the run
 * nor what to do about it. This error exists so the invoke stops cleanly
 * instead: the intent is already recorded, its completion deliberately is
 * not, and the run sits exactly where a crash mid-write would leave it, until
 * a person looks at `output` and records it by hand.
 *
 * That hand-off is `POST /v1/runs/{id}/resolve` against the running server,
 * `salvor resolve <run> --store <path> --output <json>` against its store, or
 * {@link ClientRunDriver.resolve}, any of which append the one completion
 * this middleware would not. Both are named in the message, because
 * whoever reads it may have neither: a container running this agent often has
 * no store path at all, only the server's URL, and an operator at a shell often
 * has the store and not a live server. They differ in one way worth knowing:
 * the HTTP resolve clears the run's lease along with the resolution, so the
 * thread re-opens at once, while the CLI writes the store directly and cannot
 * reach a live server's memory, so a lease held there survives it and lapses on
 * its own (at most the TTL, 60 seconds by default). `--store` names the
 * SERVER's store file, which this middleware has no way to know (it only ever
 * speaks HTTP to the server, never opens the store itself), so the printed
 * command carries a placeholder for it rather than a path that would just be
 * wrong.
 * Re-invoking the thread afterwards meets that recorded completion at `seq`
 * and replays it like any other settled call; re-invoking before it is
 * resolved meets the same open intent and is refused by the tape's own
 * "never completed" check instead, naming the same command.
 */
export class ToolNeedsResolution extends SalvorMiddlewareError {
  /** The run stopped at `seq`, waiting on a person. */
  readonly run: string;
  /** The log position the tool's intent was recorded at; its completion goes at `seq + 1`. */
  readonly seq: number;
  /** The LangGraph thread behind that run. */
  readonly thread: string;
  /** The tool's name, as the model invoked it. */
  readonly tool: string;
  /** What the tool body returned, not yet recorded: what a person confirms before it counts. */
  readonly output: unknown;
  /** The idempotency key salvor derived for this call. */
  readonly key: string;

  constructor(details: ToolNeedsResolutionDetails) {
    super(
      `the tool \`${details.tool}\` ran and returned a result, but its declaration on this ` +
        "salvor server sets `trust_completion = false`, so this middleware may not report " +
        "that result on the tool's own say-so (salvor would refuse the completion anyway, " +
        "`403 client_completion_refused`). Run " +
        `${details.run} (thread \`${details.thread}\`) is stopped at seq ${details.seq} until ` +
        "a person confirms the tool's output and records it by hand: " +
        `\`POST /v1/runs/${details.run}/resolve\` on the server with body ` +
        `{"output": ${canonicalJson(details.output)}} (which also frees the run's lease ` +
        `at once), or \`salvor resolve ${details.run} --store <the server's store> ` +
        `--output '${canonicalJson(details.output)}'\` on the store (after which the lease ` +
        "lapses on its own), or `driver.resolve(...)`. " +
        "Then invoke the thread again: the resolved output replays in this call's place.",
      { code: "tool_needs_resolution" },
    );
    this.run = details.run;
    this.seq = details.seq;
    this.thread = details.thread;
    this.tool = details.tool;
    this.output = details.output;
    this.key = details.key;
  }
}
