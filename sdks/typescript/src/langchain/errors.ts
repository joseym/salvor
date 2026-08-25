/** The errors this middleware raises on its own account. */

import { SalvorError } from "../errors.js";
import { canonicalJson } from "./hash.js";

/**
 * Something the middleware itself refuses, as opposed to something the control
 * plane refused (which stays a `SalvorApiError`). Every message names the thread
 * or the tool it is about and what would fix it, because these all surface
 * inside somebody else's agent loop, far from this file.
 */
export class SalvorMiddlewareError extends SalvorError {}

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
 * That hand-off is `salvor resolve <run> --output <json>`, the Inspector, or
 * {@link ClientRunDriver.resolve}, any of which append the one completion
 * this middleware would not. Re-invoking the thread afterwards meets that
 * recorded completion at `seq` and replays it like any other settled call;
 * re-invoking before it is resolved meets the same open intent and is refused
 * by the tape's own "never completed" check instead, naming the same command.
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
        `\`salvor resolve ${details.run} --output '${canonicalJson(details.output)}'\`, the ` +
        "Inspector, or `driver.resolve(...)`. Then invoke the thread again: the resolved " +
        "output replays in this call's place.",
    );
    this.run = details.run;
    this.seq = details.seq;
    this.thread = details.thread;
    this.tool = details.tool;
    this.output = details.output;
    this.key = details.key;
  }
}
