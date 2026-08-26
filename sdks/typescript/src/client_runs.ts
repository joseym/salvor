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
 * `SleepStarted`, `SleepCompleted`, `BudgetExceeded`, `RunCompleted`,
 * `RunFailed`). The side-effecting steps, which the server must perform because
 * it holds the key or the binary, have their own methods:
 * {@link ClientRunDriver.modelStep} and {@link ClientRunDriver.toolStep}. A
 * call the CLIENT performs in its own process, with its own secrets, has its
 * own pair as well: {@link ClientRunDriver.clientToolIntent} with
 * {@link ClientRunDriver.clientToolCompletion}, and
 * {@link ClientRunDriver.clientModelIntent} with
 * {@link ClientRunDriver.clientModelCompletion}.
 *
 * A client-driven run may park on a durable timer, and the client is what wakes
 * it. Nothing on the server waits for the deadline: the wake sweeper leaves
 * every client-driven run alone, because re-driving one there would be a second
 * writer racing this driver's lease. So {@link ClientRunDriver.sleepFor} and
 * {@link ClientRunDriver.sleepUntil} record the park, and
 * {@link ClientRunDriver.awaitWake} on a later drive reads the clock and either
 * stops (still asleep, nothing appended) or closes the pair and carries on. The
 * methods carry the runtime's names because they carry the runtime's rules.
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
  /**
   * The held lease's own token, presented on a re-open so it comes back
   * under the SAME token instead of being refused.
   *
   * Re-opening a run whose driver still holds a current lease is refused
   * (`409 lease_held`, see {@link LeaseHeldError}) unless the request
   * presents that lease's own token in `X-Drive-Token`: the run's own driver
   * rebuilding its cursor after losing local state, not a second writer.
   * Omit it for a fresh run, or when the run is not currently held (a lapsed
   * lease or a finished run re-opens under a fresh lease regardless of what,
   * if anything, is presented here).
   */
  driveToken?: string;
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
 * perform it again" without separately reading the log. `output` rides along
 * on a settled answer, the same recorded value {@link ClientRunDriver.log}
 * would show at this position's completion: a settled reply already carries
 * everything a caller needs, so nothing here has to read the log a second
 * time to learn what the call returned. `output` is present only when
 * `settled` is `true`; it is the failure sentinel, not a thrown error, when
 * the call this position recorded is the one a tool body raised (see
 * {@link ClientRunDriver.clientToolFailure}).
 */
export interface ClientToolIntentResult {
  seq: number;
  idempotencyKey: string;
  effect: string;
  settled: boolean;
  output?: unknown;
  raw: Record<string, unknown>;
}

function parseClientToolIntentResult(
  obj: Record<string, unknown>,
): ClientToolIntentResult {
  const settled = obj.settled as boolean;
  return {
    seq: obj.seq as number,
    idempotencyKey: obj.idempotency_key as string,
    effect: obj.effect as string,
    settled,
    output: settled ? obj.output : undefined,
    raw: obj,
  };
}

/** The dispatch layer a reported client-tool failure names, on the wire in
 * {@link ClientRunDriver.clientToolFailure}'s `error.kind`. Absent means
 * `"handler"`: a tool that ran and threw, the ordinary case. */
export type ClientToolFailureKind = "invalid_input" | "handler" | "output_serialization";

/** What {@link ClientRunDriver.clientToolFailure} reports about a call that
 * did not return a value because it failed. */
export interface ClientToolFailure {
  /** Recorded verbatim, in full. */
  message: string;
  /** The dispatch layer that failed. Defaults to `"handler"` on the wire when omitted. */
  kind?: ClientToolFailureKind;
}

/**
 * The receipt from opening a client-performed model call: the position, and
 * whether this position's completion is already recorded.
 *
 * `settled` is `false` on a fresh intent and on a re-post of one still
 * awaiting its answer: the call has to be made. `settled` is `true` when the
 * completion is already in the log, and then `response` and `usage` carry it,
 * which is the whole reason to record the call at all. A middleware calls the
 * provider only on the `false` branch and returns the recorded answer on the
 * `true` one, so a resumed run never pays twice for the same request.
 */
export interface ClientModelIntentResult {
  seq: number;
  settled: boolean;
  /** The recorded response, present only when `settled`. */
  response?: unknown;
  /** The recorded token usage, present only when `settled`. */
  usage?: Usage;
  raw: Record<string, unknown>;
}

function parseClientModelIntentResult(
  obj: Record<string, unknown>,
): ClientModelIntentResult {
  const settled = obj.settled as boolean;
  return {
    seq: obj.seq as number,
    settled,
    response: settled ? obj.response : undefined,
    usage: settled ? parseUsage(obj.usage) : undefined,
    raw: obj,
  };
}

/**
 * What a check on a durable timer found.
 *
 * `woken` is true when the sleep is over: either the log already held the
 * `SleepCompleted` (a replay) or the deadline had passed and the call recorded
 * it. False means the run is still asleep and nothing was appended; stop
 * driving and come back later.
 *
 * `wakeAt` is the deadline this drive measured against, the instant
 * {@link ClientRunDriver.sleepUntil} or {@link ClientRunDriver.sleepFor}
 * recorded earlier in the same drive. It is undefined when this drive set no
 * deadline at all, which is also why such a drive always reports still asleep:
 * a wake nobody asked for has not arrived, and no clock reading makes it so.
 */
export interface Waking {
  woken: boolean;
  wakeAt?: Date;
}

/**
 * Open a fresh client-driven run, or re-open (resume) an existing one.
 *
 * Passing a `runId` this server has no current lease on (never opened, a
 * lapsed lease, or a finished run) re-opens it: the recorded log comes back
 * on {@link ClientRunDriver.logEnvelopes} and a fresh lease is minted, so a
 * resuming client always holds the current one and any superseded lease
 * stops working. Passing a `runId` whose driver still holds a current lease
 * is refused as {@link LeaseHeldError} UNLESS `options.driveToken` is that
 * lease's own token, in which case the recorded log comes back under the
 * SAME token rather than a fresh one: the run's own driver rebuilding its
 * cursor, not a second writer taking the run over. Omitting `runId` entirely
 * opens a fresh run the server mints an id for.
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

  const extraHeaders: Record<string, string> = {};
  if (options.driveToken !== undefined) extraHeaders["X-Drive-Token"] = options.driveToken;

  const obj = await requestJson(
    base,
    headers,
    timeoutMs,
    "POST",
    "/v1/client-runs",
    body,
    extraHeaders,
  );
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

  /**
   * The clock the durable-timer methods read. Replaceable, the way the runtime
   * injects its own clock, so a test can drive a deadline past without waiting
   * for it.
   */
  clock: () => Date = () => new Date();

  /**
   * Called after {@link release} has handed the lease back, so whoever handed
   * this driver out can forget the token it remembered for the run. A
   * {@link SalvorClient} sets it (see `openClientRun` there); a driver opened
   * through the free function has nobody to tell and leaves it unset.
   */
  onRelease?: () => void;

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
   * The deadline set earlier in THIS drive, live or replayed. The runtime keeps
   * the same one on its context, and for the same reason: what
   * {@link awaitWake} compares against is the instant the log recorded, never a
   * duration and never a fresh reading.
   */
  private sleepingUntil?: Date;

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
   * Report that a client-performed tool call returned nothing because it
   * failed. `seq` must name the pending intent at the end of the log, exactly
   * as for {@link clientToolCompletion}; `error.message` is recorded verbatim,
   * in full, and `error.kind` names the dispatch layer that failed (default
   * `"handler"`, which is what a tool that ran and threw is).
   *
   * The server records the same `__salvor_error` sentinel a native tool's
   * exhausted retries record, byte for byte: the call is closed, the run
   * carries on, and a later replay reads the failure back rather than
   * performing the call again. A subsequent {@link clientToolIntent} at this
   * `seq` comes back `settled: true` with that sentinel as `output`.
   *
   * Refused, recording nothing, with the same `client_completion_refused`
   * {@link clientToolCompletion} is refused with: a `trust_completion = false`
   * declaration holds for a reported failure exactly as it does for a
   * reported result, since "it did not land" is a claim made by the party
   * that benefits from it being believed. There the fix is the same one:
   * settle the call by hand with {@link resolve} once you have verified it
   * externally.
   */
  async clientToolFailure(seq: number, error: ClientToolFailure): Promise<void> {
    const wire: Record<string, unknown> = { message: error.message };
    if (error.kind !== undefined) wire.kind = error.kind;
    await this.send(
      "POST",
      `/v1/client-runs/${this.runId}/client-tool-completion`,
      { seq, error: wire },
    );
  }

  /**
   * Open a model call the CLIENT performs, in its own process, with its own
   * key and its own model configuration. `seq` is the log position the
   * client's cursor reserved for the intent; `requestHash` is the client's own
   * canonical hash of the request it is about to send; `requestBody` is the
   * full request, recorded on the intent only when the run was opened with
   * `recordPrompts: true` and dropped otherwise.
   *
   * This is the counterpart of {@link modelStep} for a call salvor does not
   * make. There the server holds the key, performs the call, and recomputes
   * the hash from the request it was handed, so the hash cannot be lied about.
   * Here it can: the request never reaches the server, so the hash is the
   * client's claim over its own request, the way a client-performed tool
   * result is the client's claim about its own call. What the trust buys is
   * the point of the method: a resume replays the recorded answer instead of
   * paying the provider for it again. The claim is also self-punishing rather
   * than dangerous to anyone else, because the hash is a key into this run's
   * own log: a client that hashes inconsistently diverges against its own
   * history and nobody else's.
   *
   * The returned `settled` is `true` when the intent at `seq` already has its
   * completion recorded, and the recorded `response` and `usage` come back
   * with it. That is what a middleware short-circuits on: call the provider
   * only when `settled` is `false`, and otherwise return the recorded answer
   * without a second request.
   *
   * A re-post at a recorded position with the same hash is a replay that
   * writes nothing. A different hash there, a non-model event, or an intent
   * the SERVER performed throws {@link DivergenceError}, as does a `seq` the
   * log is not ready for.
   */
  async clientModelIntent(
    seq: number,
    requestHash: string,
    requestBody?: unknown,
  ): Promise<ClientModelIntentResult> {
    const body: Record<string, unknown> = { seq, request_hash: requestHash };
    if (requestBody !== undefined) body.request_body = requestBody;
    const obj = await this.send(
      "POST",
      `/v1/client-runs/${this.runId}/client-model-intent`,
      body,
    );
    return parseClientModelIntentResult(obj);
  }

  /**
   * Report what a client-performed model call returned. `seq` must name the
   * pending intent at the end of the log; `response` is recorded verbatim and
   * `usage` is the token count the run's budgets are held to, so it is
   * required rather than optional: a completion that quietly reported none
   * would under-count every budget the run runs under.
   *
   * Refused, recording nothing, as a {@link DivergenceError} when the log does
   * not end at a model intent or ends at one for a different `seq`, and as a
   * `SalvorApiError` with code `client_completion_refused` when the pending
   * intent was performed by the SERVER: salvor holds the real response for
   * that call, so a client may not overwrite it with a claim.
   *
   * Once recorded, the completion is byte-identical to a server-performed
   * one, so the run folds the same either way: pending while the intent is
   * open, closed by this call, tokens counted.
   */
  async clientModelCompletion(
    seq: number,
    response: unknown,
    usage: Usage,
  ): Promise<void> {
    await this.send(
      "POST",
      `/v1/client-runs/${this.runId}/client-model-completion`,
      {
        seq,
        response,
        usage: {
          input_tokens: usage.inputTokens,
          output_tokens: usage.outputTokens,
        },
      },
    );
  }

  /**
   * Observe the clock at `seq`, recording the reading the first time.
   *
   * Returns the recorded reading when `seq` already holds a `NowObserved`, so a
   * later drive replays the identical instant, and otherwise reads
   * {@link clock}, appends it, and returns it. This is the one way a
   * client-driven run gets time into its log: a reading taken outside the log
   * means nothing to a replay, which has no clock of its own to interpret it
   * against.
   */
  async now(seq: number): Promise<Date> {
    const recorded = await this.eventAt(seq);
    if (recorded?.kind === "NowObserved") {
      return new Date(recorded.payload.now as string);
    }
    const reading = this.clock();
    await this.append([
      this.envelope(seq, "NowObserved", { now: reading.toISOString() }),
    ]);
    return reading;
  }

  /**
   * Park the run on a durable timer at `seq`, returning `wakeAt`.
   *
   * `wakeAt` must be derived from recorded data, because a later drive presents
   * it again and it has to be the same instant: derive it from an observed
   * {@link now} (which {@link sleepFor} does for you), never from a clock read
   * outside the log. A position already holding this exact park is a replay:
   * nothing is appended. A position holding a DIFFERENT one is submitted
   * anyway, so the server refuses it as the divergence it is rather than this
   * driver quietly preferring one of the two instants.
   *
   * Follow it with {@link awaitWake}. Never park between a write tool's intent
   * and its completion: the run holds that call's claim for the whole sleep,
   * which for a durable timer is hours or weeks.
   */
  async sleepUntil(seq: number, wakeAt: Date): Promise<Date> {
    const recorded = await this.eventAt(seq);
    if (recorded?.kind === "SleepStarted") {
      const already = new Date(recorded.payload.wake_at as string);
      if (already.getTime() === wakeAt.getTime()) {
        this.sleepingUntil = already;
        return already;
      }
    }
    await this.append([
      this.envelope(seq, "SleepStarted", { wake_at: wakeAt.toISOString() }),
    ]);
    this.sleepingUntil = wakeAt;
    return wakeAt;
  }

  /**
   * Sleep for `durationMs` from a recorded reading of the clock, returning the
   * wake instant it recorded.
   *
   * Exactly `now() + durationMs`, recorded: the reading goes into the log at
   * `seq` as a `NowObserved` before the park is derived from it, and the park
   * lands at `seq + 1`. So every later drive replays the identical reading and
   * derives the identical instant, which is what a duration alone can never do.
   * Carries every rule {@link sleepUntil} does.
   */
  async sleepFor(seq: number, durationMs: number): Promise<Date> {
    const now = await this.now(seq);
    return this.sleepUntil(seq + 1, new Date(now.getTime() + durationMs));
  }

  /**
   * Ask whether the sleep is over, closing the pair at `seq` if it is.
   *
   * The log decides first: a `SleepCompleted` already recorded at `seq` means
   * the sleep ended on an earlier drive, so this replays it and appends
   * nothing. Otherwise {@link clock} decides, against the deadline
   * {@link sleepUntil} or {@link sleepFor} recorded earlier in this same drive.
   * At or past it the completion is appended and the run carries on; before it,
   * nothing is appended and the returned {@link Waking} reports the run still
   * asleep, which is the signal to stop driving and come back later.
   *
   * Nothing here can wake a run early, and that is deliberate: a driver that
   * comes back too soon simply finds it still asleep, exactly as the server
   * wakes nothing before its instant.
   */
  async awaitWake(seq: number): Promise<Waking> {
    const recorded = await this.eventAt(seq);
    const wakeAt = this.sleepingUntil;
    if (recorded?.kind === "SleepCompleted") {
      this.sleepingUntil = undefined;
      return { woken: true, wakeAt };
    }
    // A drive that set no deadline has none that could have arrived, so it
    // stays asleep, mirroring the runtime's stand-in for the same case.
    if (wakeAt === undefined || this.clock().getTime() < wakeAt.getTime()) {
      return { woken: false, wakeAt };
    }
    await this.append([this.envelope(seq, "SleepCompleted")]);
    this.sleepingUntil = undefined;
    return { woken: true, wakeAt };
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

  /**
   * Hand the lease back, so the next open takes the run at once instead of
   * waiting out the TTL.
   *
   * Lapsing is the safety net for a driver that can no longer say anything (it
   * crashed, the tab closed); it is a poor way to end a drive that ended in an
   * orderly fashion, because the run stays unopenable for the rest of the TTL.
   * A short-lived process is exactly where that bites: an invoke returns, the
   * process exits, and the next one is refused `409 lease_held` for up to a
   * minute for nothing. So a driver that is finished calls this.
   *
   * Returns whether there was a lease here to give back. `false` is not an
   * error: it means the run has no lease on this server (already released,
   * lapsed, or never opened here), which is the state the caller was asking
   * for anyway, so the call is idempotent. Throws a `SalvorApiError` with code
   * `invalid_drive_token` when a lease DOES stand and this token is not it: a
   * hold that is not this driver's is not this driver's to end.
   *
   * Only the lease goes. The log is untouched and the run stays client-driven,
   * so a later open adopts it exactly as it would after a server restart.
   */
  async release(): Promise<boolean> {
    const obj = await this.send("POST", `/v1/client-runs/${this.runId}/release`, {});
    const released = (obj.released as boolean) ?? false;
    this.onRelease?.();
    return released;
  }

  /**
   * Say "still here" without driving the run, and learn the lease TTL.
   *
   * Presenting the drive token has always been the heartbeat, and every
   * driving call carries it. What that misses is the driver that makes no
   * drive call for longer than the TTL because it is inside one long body: a
   * tool that takes minutes, a model call it is streaming to its own screen.
   * Its lease would lapse mid-body and another opener could take a run whose
   * driver never went anywhere.
   *
   * Returns `lapses_in_seconds`, the whole lease TTL as of this beat, so a
   * caller picks its interval from the answer rather than from a copy of the
   * server's configuration. Throws a `SalvorApiError` with code
   * `invalid_drive_token` when this token is no longer the run's lease, or
   * `unknown_run` when this server holds no lease for the run at all.
   */
  async heartbeat(): Promise<number> {
    const obj = await this.send("POST", `/v1/client-runs/${this.runId}/heartbeat`, {});
    return (obj.lapses_in_seconds as number | undefined) ?? 0;
  }

  // -- helpers --------------------------------------------------------------

  /**
   * The recorded event at `seq`, or undefined when the log has not reached that
   * position yet.
   *
   * One log read, deliberately: the durable-timer methods are called once per
   * drive apiece, and a driver that has been away for a week cannot trust
   * anything it cached before it left.
   */
  private async eventAt(seq: number): Promise<SalvorEvent | undefined> {
    const tail = await this.log(seq);
    return tail[0]?.seq === seq ? tail[0] : undefined;
  }

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
