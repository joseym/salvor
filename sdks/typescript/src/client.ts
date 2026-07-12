/**
 * A thin async client for the Salvor control plane.
 *
 * The client holds no durability logic and no run state. It submits
 * definitions and inputs, reads events, and decodes the error envelope into
 * exceptions. Every guarantee, exact replay, crash-safe resume, the
 * write-ahead reconciliation rule, lives in the one Rust process it talks to.
 */

import {
  type ClientRunDriver,
  type OpenClientRunOptions,
  openClientRun,
} from "./client_runs.js";
import { SalvorApiError, SalvorStreamError, errorFrom } from "./errors.js";
import { readSseFrames } from "./sse.js";
import {
  type EndFrame,
  type ReplayState,
  type ResumeResult,
  type RunState,
  type RunSummary,
  type SalvorEvent,
  parseEndFrame,
  parseEvent,
  parseReplayState,
  parseResumeResult,
  parseRunState,
  parseRunSummary,
} from "./types.js";

/** Options for constructing a {@link SalvorClient}. */
export interface SalvorClientOptions {
  /** An optional shared-secret bearer token, sent as `Authorization: Bearer`. */
  token?: string;
  /** Per-request timeout in milliseconds for non-streaming calls. Default 30000. */
  timeoutMs?: number;
  /** How many times to reconnect a dropped event stream. Default 5. */
  maxStreamRetries?: number;
}

/**
 * An async iterable of {@link SalvorEvent} from a run's stream. Iterate it with
 * `for await`; iteration stops at the terminal `end` frame, after which
 * {@link end} holds the {@link EndFrame} with the run's resting status.
 */
export interface EventStream extends AsyncIterable<SalvorEvent> {
  /** The terminal frame, set once the stream reaches its resting point. */
  readonly end: EndFrame | undefined;
}

/** A parsed server-sent frame: an event envelope, or the terminal end frame. */
type Frame =
  | { kind: "event"; obj: Record<string, unknown> }
  | { kind: "end"; obj: Record<string, unknown> };

/** A client for one Salvor control plane. */
export class SalvorClient {
  readonly baseUrl: string;
  private readonly headers: Record<string, string>;
  private readonly timeoutMs: number;
  private readonly maxStreamRetries: number;

  constructor(baseUrl: string, options: SalvorClientOptions = {}) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.maxStreamRetries = options.maxStreamRetries ?? 5;
    this.headers = {};
    if (options.token) {
      this.headers["Authorization"] = `Bearer ${options.token}`;
    }
  }

  // -- agents ---------------------------------------------------------------

  /**
   * Register and validate an agent definition; return its content hash.
   * Accepts a TOML string (sent as `application/toml`) or an object of the same
   * fields (sent as `application/json`). An agent is data with a content hash,
   * the same id every `RunStarted` event records; submit it once and reference
   * it by that hash.
   */
  async registerAgent(definition: string | Record<string, unknown>): Promise<string> {
    let body: string;
    let contentType: string;
    if (typeof definition === "string") {
      body = definition;
      contentType = "application/toml";
    } else {
      body = JSON.stringify(definition);
      contentType = "application/json";
    }
    const obj = await this.request("POST", "/v1/agents", {
      body,
      contentType,
    });
    return obj.agent as string;
  }

  /** List the registered agent hashes. */
  async listAgents(): Promise<string[]> {
    const obj = await this.request("GET", "/v1/agents");
    return ((obj.agents as Record<string, unknown>[]) ?? []).map(
      (a) => a.agent as string,
    );
  }

  /** Read one registered definition back: its hash, format, and body. */
  async getAgent(agentHash: string): Promise<Record<string, unknown>> {
    return this.request("GET", `/v1/agents/${agentHash}`);
  }

  // -- runs -----------------------------------------------------------------

  /**
   * Start a fresh run of a registered agent; return its run id. The call
   * resolves as soon as the run is accepted; the run then drives in the
   * background on the server. Open {@link streamEvents} to watch it.
   */
  async startRun(
    agent: string,
    input: unknown = null,
    options: { runId?: string } = {},
  ): Promise<string> {
    const body: Record<string, unknown> = { agent, input };
    if (options.runId !== undefined) body.run_id = options.runId;
    const obj = await this.request("POST", "/v1/runs", {
      body: JSON.stringify(body),
      contentType: "application/json",
    });
    return obj.run as string;
  }

  /** List every run with its folded status and counts. */
  async listRuns(): Promise<RunSummary[]> {
    const obj = await this.request("GET", "/v1/runs");
    return ((obj.runs as Record<string, unknown>[]) ?? []).map(parseRunSummary);
  }

  /** Get one run's derived state (status, usage, pending call, counts). */
  async getRun(runId: string): Promise<RunState> {
    return parseRunState(await this.request("GET", `/v1/runs/${runId}`));
  }

  /**
   * Dry-run replay: the derived state as a pure fold of the log, executing
   * nothing. This is what `salvor replay --dry-run` prints.
   */
  async replay(runId: string): Promise<ReplayState> {
    return parseReplayState(
      await this.request("GET", `/v1/runs/${runId}/replay`),
    );
  }

  /**
   * Continue a run: resume a parked one, or recover a crashed one. A parked run
   * needs an `input` validated against its recorded schema; a crashed run
   * recovers with no input. A run that needs reconciliation is refused,
   * throwing {@link NeedsReconciliationError}; use {@link resolve} to move past
   * it.
   */
  async resume(runId: string, input?: unknown): Promise<ResumeResult> {
    const body = input === undefined ? "{}" : JSON.stringify({ input });
    return parseResumeResult(
      await this.request("POST", `/v1/runs/${runId}/resume`, {
        body,
        contentType: "application/json",
      }),
    );
  }

  /**
   * Record a dangling write's completion by hand, the operator side of
   * reconciliation. After verifying externally what a recorded-but-never
   * -completed write did, this records the output it produced, so replay
   * treats the call as done and never re-runs it.
   */
  async resolve(runId: string, output: unknown): Promise<RunState> {
    const obj = await this.request("POST", `/v1/runs/${runId}/resolve`, {
      body: JSON.stringify({ output }),
      contentType: "application/json",
    });
    return parseRunState({
      run: obj.run ?? runId,
      status: obj.status ?? {},
      event_count: obj.event_count ?? 0,
    });
  }

  // -- client-driven runs ---------------------------------------------------

  /**
   * Open or re-open a client-driven run over this client's base URL and auth.
   * Returns a {@link ClientRunDriver}: the client owns the agent loop and streams
   * the events it produces, while the server owns the durable log and guards
   * every append. This is the second of Salvor's two modes; {@link startRun} is
   * the server-driven first.
   */
  openClientRun(options: OpenClientRunOptions = {}): Promise<ClientRunDriver> {
    const token = this.headers["Authorization"]?.replace(/^Bearer /, "");
    return openClientRun(this.baseUrl, {
      token,
      timeoutMs: this.timeoutMs,
      ...options,
    });
  }

  // -- event stream ---------------------------------------------------------

  /**
   * Stream a run's events in order until it rests, as an async iterable.
   *
   * On connect the server replays every recorded event at or after the cursor,
   * then tails new events as they land, then sends one terminal `end` frame and
   * closes. The log is gap-free and duplicate-free by construction, so tracking
   * one "next sequence" number is enough. A dropped connection is resumed from
   * that cursor (`?from_seq`), and any event seen before the drop is skipped, so
   * the merged stream stays gap-free and duplicate-free across reconnects.
   */
  streamEvents(runId: string, options: { fromSeq?: number } = {}): EventStream {
    const self = this;
    const state = { end: undefined as EndFrame | undefined };

    async function* generate(): AsyncGenerator<SalvorEvent> {
      let nextSeq = options.fromSeq ?? 0;
      let lastSeen: number | undefined;
      let attempts = 0;
      for (;;) {
        let ended = false;
        try {
          for await (const frame of self.readFrames(runId, nextSeq)) {
            if (frame.kind === "event") {
              const event = parseEvent(frame.obj);
              if (lastSeen !== undefined && event.seq <= lastSeen) continue;
              lastSeen = event.seq;
              nextSeq = event.seq + 1;
              attempts = 0; // forward progress refreshes the retry budget
              yield event;
            } else {
              state.end = parseEndFrame(frame.obj);
              ended = true;
              break;
            }
          }
        } catch (err) {
          if (err instanceof SalvorApiError) throw err;
          // A network or protocol error is a transient drop; reconnect below.
        }
        if (ended) return;
        attempts += 1;
        if (attempts > self.maxStreamRetries) {
          throw new SalvorStreamError(
            `event stream for run ${runId} dropped and did not resume after ` +
              `${self.maxStreamRetries} attempts`,
          );
        }
        await sleep(Math.min(250 * attempts, 2000));
      }
    }

    return {
      get end() {
        return state.end;
      },
      [Symbol.asyncIterator]: generate,
    };
  }

  /** Open one connection and yield each SSE frame, parsing the line protocol. */
  private async *readFrames(runId: string, fromSeq: number): AsyncGenerator<Frame> {
    const url = `${this.baseUrl}/v1/runs/${runId}/events?from_seq=${fromSeq}`;
    // No timeout on the streaming read: a live tail blocks between events. A
    // drop surfaces as the stream ending or an error, and reconnect handles it.
    const resp = await fetch(url, { headers: this.headers });
    if (!resp.ok || !resp.body) {
      throw errorFrom(resp.status, await resp.text());
    }
    for await (const frame of readSseFrames(resp.body)) {
      const obj = JSON.parse(frame.data) as Record<string, unknown>;
      yield frame.event === "end" ? { kind: "end", obj } : { kind: "event", obj };
    }
  }

  // -- helpers --------------------------------------------------------------

  private async request(
    method: string,
    path: string,
    opts: { body?: string; contentType?: string } = {},
  ): Promise<Record<string, unknown>> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const headers: Record<string, string> = { ...this.headers };
      if (opts.contentType) headers["Content-Type"] = opts.contentType;
      const resp = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers,
        body: opts.body,
        signal: controller.signal,
      });
      const text = await resp.text();
      if (!resp.ok) throw errorFrom(resp.status, text);
      return text ? (JSON.parse(text) as Record<string, unknown>) : {};
    } finally {
      clearTimeout(timer);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
