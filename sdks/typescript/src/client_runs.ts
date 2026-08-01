/**
 * The client-driven run driver: Salvor's second mode.
 *
 * In the server-driven mode ({@link SalvorClient}) the server owns the agent
 * loop and drives it in a background task. This module inverts who owns the loop
 * while keeping who owns the log. The client (this driver, or a browser folding
 * a run's log in a wasm cursor) owns the loop and streams the events it
 * produces; the server owns the durable log and, on every append, re-folds it
 * with the pure append-guard to confirm the incoming event is the one legal next
 * event.
 *
 * The generic append carries only the control and deterministic-context events
 * the client emits itself, which hold no secret and no side effect
 * (`RunStarted`, `NowObserved`, `RandomObserved`, `Suspended`, `Resumed`,
 * `BudgetExceeded`, `RunCompleted`, `RunFailed`). The side-effecting steps, which
 * the server must perform because it holds the key or the binary, have their own
 * methods: {@link ClientRunDriver.modelStep} and {@link ClientRunDriver.toolStep}.
 *
 * This module is browser-safe as written: it uses `fetch` and the SDK's
 * hand-written SSE parser only, with no Node-only API, so the same driver runs
 * in a browser tab and in a Node backend.
 *
 * ```ts
 * const run = await openClientRun("http://127.0.0.1:8080");
 * await run.append([run.envelope(0, "RunStarted", { agent_def_hash: agent, input: task })]);
 * const { response, usage } = await run.modelStep(1, request);
 * await run.append([run.envelope(3, "RunCompleted", { output: answer })]);
 * ```
 *
 * Unlike {@link SalvorClient.startRun}, this driver has no dedicated `labels`
 * option: the client builds `RunStarted` itself, so correlation tags simply
 * ride in that payload, e.g. `run.envelope(0, "RunStarted", { agent_def_hash:
 * agent, input: task, labels: { build: "42" } })`. The server enforces the
 * same bounds on append (see `API.md`) as it does for a server-driven start.
 */

import { SalvorStreamError, errorFrom } from "./errors.js";
import { readSseFrames } from "./sse.js";
import { type SalvorEvent, type Usage, parseEvent, parseUsage } from "./types.js";

/** Options for opening a client-driven run. */
export interface OpenClientRunOptions {
  /** The agent hash, forwarded for compatibility; not recorded by the server. */
  agent?: string;
  /** The run input, forwarded for compatibility; not recorded by the server. */
  input?: unknown;
  /** A client-chosen run id. Passing an existing one re-opens (resumes) it. */
  runId?: string;
  /** Record each model step's request body on its intent. Default false. */
  recordPrompts?: boolean;
  /** An optional shared-secret bearer token, sent as `Authorization: Bearer`. */
  token?: string;
  /** Per-request timeout in milliseconds for non-streaming calls. Default 30000. */
  timeoutMs?: number;
}

/** The completion of a server-performed model step. */
export interface ModelStepResult {
  /** The provider's `MessageResponse` as decoded JSON. */
  response: unknown;
  /** The token counts folded from the response. */
  usage?: Usage;
  /** The full decoded body. */
  raw: Record<string, unknown>;
}

/**
 * One live ticker delta from a streaming model step. Text and thinking deltas
 * carry the incremental content; the final `usage` delta carries the output
 * token count as the call closes.
 */
export type ModelStepDelta =
  | { type: "text_delta"; index: number; text: string }
  | { type: "thinking_delta"; index: number; thinking: string }
  | { type: "usage"; output_tokens: number };

/**
 * The live ticker of a streaming model step, an async iterable of
 * {@link ModelStepDelta}. Iterate it with `for await` to paint each delta;
 * iteration stops when the assembled completion arrives, after which
 * {@link completion} holds the {@link ModelStepResult}. A mid-stream provider
 * failure throws {@link SalvorStreamError}.
 */
export interface ModelStepStream extends AsyncIterable<ModelStepDelta> {
  /** The assembled completion, set once the stream reaches its `complete` frame. */
  readonly completion: ModelStepResult | undefined;
}

function parseModelStepResult(obj: Record<string, unknown>): ModelStepResult {
  return { response: obj.response, usage: parseUsage(obj.usage), raw: obj };
}

/**
 * The receipt from opening a client-performed tool call: the position, the
 * DERIVED idempotency key the client must perform under, the
 * operator-declared effect the intent was recorded with, and whether this
 * position's completion is already recorded.
 *
 * `settled` is `true` when the intent at `seq` already has its completion
 * recorded, `false` otherwise. A payments caller retrying `clientToolIntent`
 * after a dropped response gets back the same key either way; `settled` is
 * what lets it tell "safe to perform the call" from "already done, do not
 * perform it again" without separately reading the log.
 */
export interface ClientToolIntentResult {
  seq: number;
  idempotencyKey: string;
  effect: string;
  settled: boolean;
  raw: Record<string, unknown>;
}

function parseClientToolIntentResult(
  obj: Record<string, unknown>,
): ClientToolIntentResult {
  return {
    seq: obj.seq as number,
    idempotencyKey: obj.idempotency_key as string,
    effect: obj.effect as string,
    settled: obj.settled as boolean,
    raw: obj,
  };
}

/**
 * Open a fresh client-driven run, or re-open (resume) an existing one.
 *
 * Passing a `runId` this server already holds re-opens it: the recorded log
 * comes back on {@link ClientRunDriver.logEnvelopes} and a fresh lease is
 * minted, so a resuming client always holds the current one and the superseded
 * lease stops working. Omitting it opens a fresh run the server mints an id for.
 */
export async function openClientRun(
  baseUrl: string,
  options: OpenClientRunOptions = {},
): Promise<ClientRunDriver> {
  const base = baseUrl.replace(/\/+$/, "");
  const headers: Record<string, string> = {};
  if (options.token) headers["Authorization"] = `Bearer ${options.token}`;
  const timeoutMs = options.timeoutMs ?? 30_000;

  const body: Record<string, unknown> = {
    record_prompts: options.recordPrompts ?? false,
  };
  if (options.agent !== undefined) body.agent = options.agent;
  if (options.input !== undefined) body.input = options.input;
  if (options.runId !== undefined) body.run_id = options.runId;

  const obj = await requestJson(base, headers, timeoutMs, "POST", "/v1/client-runs", body);
  const log = ((obj.log as Record<string, unknown>[]) ?? []).map(parseEvent);
  return new ClientRunDriver(base, headers, timeoutMs, {
    runId: obj.run as string,
    driveToken: obj.drive_token as string,
    log,
  });
}

/** Drives one client-driven run against a Salvor control plane. */
export class ClientRunDriver {
  /** The run id the server minted or the client chose. */
  readonly runId: string;
  /** The current single-writer lease every append presents. */
  readonly driveToken: string;
  /**
   * The envelopes returned when this run was opened: empty for a fresh run, the
   * full recorded log for a re-open, ready to rebuild a cursor.
   */
  readonly logEnvelopes: SalvorEvent[];

  private readonly base: string;
  private readonly headers: Record<string, string>;
  private readonly timeoutMs: number;

  constructor(
    base: string,
    headers: Record<string, string>,
    timeoutMs: number,
    opened: { runId: string; driveToken: string; log: SalvorEvent[] },
  ) {
    this.base = base;
    this.headers = headers;
    this.timeoutMs = timeoutMs;
    this.runId = opened.runId;
    this.driveToken = opened.driveToken;
    this.logEnvelopes = opened.log;
  }

  /**
   * Build one event-envelope for {@link append} at `seq`. Wraps `kind` and
   * `payload` in the pinned envelope shape the log and event stream use, filling
   * `run_id` and a fixed `schema_version`. `recorded_at` is a client-side
   * placeholder; the server stamps the authoritative time when it records.
   */
  envelope(
    seq: number,
    kind: string,
    payload: Record<string, unknown> = {},
  ): Record<string, unknown> {
    return {
      run_id: this.runId,
      seq,
      schema_version: 1,
      recorded_at: "1970-01-01T00:00:00Z",
      event: { kind, payload },
    };
  }

  /**
   * Read the recorded log back, for a refreshed client to rebuild its cursor.
   * Returns every recorded envelope at or after `fromSeq`. The read needs no
   * drive token.
   */
  async log(fromSeq = 0): Promise<SalvorEvent[]> {
    const obj = await this.get(`/v1/client-runs/${this.runId}/log?from_seq=${fromSeq}`);
    return ((obj.log as Record<string, unknown>[]) ?? []).map(parseEvent);
  }

  /**
   * Append control and context events, guarded against the durable log, and
   * return the sequence numbers recorded. The whole batch is validated before
   * anything is written. Re-appending byte-identical events at recorded
   * positions is a no-op that still reports those seqs and does not grow the log
   * (the retry-safe case after a network blip); different bytes there, or an
   * event that is not the legal next one, throws {@link DivergenceError}; a
   * model or tool event throws a `SalvorApiError` with code
   * `unsupported_event_kind`.
   */
  async append(events: Record<string, unknown>[]): Promise<number[]> {
    const obj = await this.send("POST", `/v1/client-runs/${this.runId}/events`, {
      events,
    });
    return (obj.appended as number[]) ?? [];
  }

  /**
   * Perform and record a model call the server makes (it holds the key). `seq`
   * is the log position the client's cursor reserved for the model intent;
   * `request` is the canonical model request. Retry identity is
   * `(seq, requestHash)`: a step already completed at `seq` with the same
   * request returns the recorded completion without calling the provider again
   * (the no-re-pay case); a different request there throws
   * {@link DivergenceError}.
   */
  async modelStep(seq: number, request: unknown): Promise<ModelStepResult> {
    const obj = await this.send("POST", `/v1/client-runs/${this.runId}/model-step`, {
      seq,
      request,
    });
    return parseModelStepResult(obj);
  }

  /**
   * Perform a model step with a live ticker, over a server-sent stream. Same
   * recording and retry semantics as {@link modelStep}, but the response is a
   * stream: iterate the returned {@link ModelStepStream} to paint each delta as
   * it arrives, then read the assembled completion from its `completion`. The
   * recorded completion is byte-identical to the non-streaming path.
   */
  modelStepStream(seq: number, request: unknown): ModelStepStream {
    const self = this;
    const state = { completion: undefined as ModelStepResult | undefined };

    async function* generate(): AsyncGenerator<ModelStepDelta> {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), self.timeoutMs);
      let resp: Response;
      try {
        resp = await fetch(`${self.base}/v1/client-runs/${self.runId}/model-step`, {
          method: "POST",
          headers: {
            ...self.headers,
            "Content-Type": "application/json",
            "X-Drive-Token": self.driveToken,
            Accept: "text/event-stream",
          },
          body: JSON.stringify({ seq, request }),
          signal: controller.signal,
        });
      } finally {
        clearTimeout(timer);
      }
      if (!resp.ok || !resp.body) {
        throw errorFrom(resp.status, await resp.text());
      }
      for await (const frame of readSseFrames(resp.body)) {
        if (frame.event === "delta") {
          yield JSON.parse(frame.data) as ModelStepDelta;
        } else if (frame.event === "complete") {
          state.completion = parseModelStepResult(
            JSON.parse(frame.data) as Record<string, unknown>,
          );
          return;
        } else if (frame.event === "error") {
          const message =
            (JSON.parse(frame.data).message as string) ?? "model step failed";
          throw new SalvorStreamError(
            `model step for run ${self.runId}: ${message}`,
          );
        }
      }
    }

    return {
      get completion() {
        return state.completion;
      },
      [Symbol.asyncIterator]: generate,
    };
  }

  /**
   * Perform and record a tool call the server makes (it holds the binary).
   * `seq` is the reserved log position; `tool` names a tool the server's
   * registry holds; `input` is recorded on the intent verbatim.
   * `idempotencyKey` is optional; for an idempotent tool draw it from a recorded
   * `RandomObserved` so it reproduces on replay. The recorded effect is the
   * tool's operator-declared one. Returns the tool's output.
   *
   * A completed step returns the recorded output without re-dispatching; a
   * dangling `Write` intent throws {@link NeedsReconciliationError} carrying the
   * recorded intent, and only {@link resolve} may record its completion.
   */
  async toolStep(
    seq: number,
    tool: string,
    input: unknown,
    options: { idempotencyKey?: string } = {},
  ): Promise<unknown> {
    const body: Record<string, unknown> = { seq, tool, input };
    if (options.idempotencyKey !== undefined) {
      body.idempotency_key = options.idempotencyKey;
    }
    const obj = await this.send("POST", `/v1/client-runs/${this.runId}/tool-step`, body);
    return obj.output;
  }

  /**
   * Open a tool call the CLIENT performs, in its own process, with its own
   * secrets. `seq` is the log position the client's cursor reserved for the
   * intent; `tool` names a tool an operator declared with `salvor serve
   * --client-tool <FILE>` (never registered over HTTP; see
   * {@link SalvorClient.listClientTools} to fetch what is declared); `input`
   * is checked against the declaration's input schema before anything is
   * written.
   *
   * The returned `idempotencyKey` comes FROM the server, not from the caller.
   * It is a derived hash of `(run, seq, tool)`, and the client must perform
   * its call under that exact key. This is why: it is what stops a retry
   * becoming a second charge, so the party who would benefit from a duplicate
   * landing does not get to choose the key that lets one through. This is the
   * one place this driver differs from {@link toolStep} on purpose: there the
   * caller supplies the key, because salvor performs the call itself and
   * handing it the key is safe; here the client both performs the call and
   * stands to gain from a duplicate, so the server derives the key instead of
   * accepting one.
   *
   * The returned `settled` is `true` when the intent at `seq` already has its
   * completion recorded, `false` otherwise. A payments caller retrying this
   * call after a dropped response gets back the same key either way;
   * `settled` is what lets it tell "safe to perform the call" from "already
   * done, do not perform it again" without a separate log read.
   *
   * Throws `SalvorApiError` with code `unknown_tool` for an undeclared tool,
   * or `bad_request` when `input` fails the declaration's schema; a `seq` the
   * log is not ready for, or a different event already recorded there, throws
   * {@link DivergenceError}.
   */
  async clientToolIntent(
    seq: number,
    tool: string,
    input: unknown,
  ): Promise<ClientToolIntentResult> {
    const obj = await this.send(
      "POST",
      `/v1/client-runs/${this.runId}/client-tool-intent`,
      { seq, tool, input },
    );
    return parseClientToolIntentResult(obj);
  }

  /**
   * Report what a client-performed tool call returned. `seq` must name the
   * pending intent at the end of the log; `output` is checked against the
   * declaration's output schema before it is recorded.
   *
   * Refused, recording nothing, as a `SalvorApiError` with code
   * `client_completion_refused` when: the declaration was written with
   * `trust_completion = false`, or it carries no output schema at all. Either
   * way there is nothing this call can trust, so settle it by hand instead
   * with {@link resolve} once you have verified the result externally. A
   * reported `output` that fails the declared schema is `bad_request`; there
   * the fix is the output, not the call.
   */
  async clientToolCompletion(seq: number, output: unknown): Promise<void> {
    await this.send(
      "POST",
      `/v1/client-runs/${this.runId}/client-tool-completion`,
      { seq, output },
    );
  }

  /**
   * Record a dangling write's completion by hand, unsticking the run. Legal only
   * when the run's log ends at a dangling `Write` intent: it correlates `output`
   * to that intent and dispatches nothing. After it records the completion the
   * run drives again, so re-fetch {@link log} and the cursor sails past the
   * once-dangling intent. Throws a `SalvorApiError` with code `wrong_state` when
   * there is no dangling write.
   */
  async resolve(output: unknown): Promise<void> {
    await this.send("POST", `/v1/client-runs/${this.runId}/resolve`, { output });
  }

  // -- helpers --------------------------------------------------------------

  private get(path: string): Promise<Record<string, unknown>> {
    return requestJson(this.base, this.headers, this.timeoutMs, "GET", path);
  }

  private send(
    method: string,
    path: string,
    body: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    return requestJson(this.base, this.headers, this.timeoutMs, method, path, body, {
      "X-Drive-Token": this.driveToken,
    });
  }
}

/** One JSON request with a timeout, decoding the error envelope on a non-2xx. */
async function requestJson(
  base: string,
  headers: Record<string, string>,
  timeoutMs: number,
  method: string,
  path: string,
  body?: Record<string, unknown>,
  extraHeaders: Record<string, string> = {},
): Promise<Record<string, unknown>> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const merged: Record<string, string> = { ...headers, ...extraHeaders };
    if (body !== undefined) merged["Content-Type"] = "application/json";
    const resp = await fetch(`${base}${path}`, {
      method,
      headers: merged,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
    const text = await resp.text();
    if (!resp.ok) throw errorFrom(resp.status, text);
    return text ? (JSON.parse(text) as Record<string, unknown>) : {};
  } finally {
    clearTimeout(timer);
  }
}
