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
import type { Graph } from "./graph.js";
import { readSseFrames } from "./sse.js";
import {
  type AbandonResult,
  type ClientToolDecl,
  type EndFrame,
  type ForkPreview,
  type ForkResult,
  type ForksIndex,
  type GraphProjection,
  type GraphSubmitted,
  type GraphSummary,
  type GraphValidation,
  type Labels,
  type ReplayState,
  type ResumeResult,
  type RunState,
  type RunSummary,
  type SalvorEvent,
  type StoredGraph,
  parseAbandonResult,
  parseClientToolDecl,
  parseEndFrame,
  parseEvent,
  parseForkPreview,
  parseForkResult,
  parseForksIndex,
  parseGraphProjection,
  parseGraphSubmitted,
  parseGraphSummary,
  parseGraphValidation,
  parseReplayState,
  parseResumeResult,
  parseRunState,
  parseRunSummary,
  parseStoredGraph,
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
  /**
   * The last drive token this client obtained for a run it opened, by run id.
   * See {@link openClientRun} on why this instance, and no one else,
   * remembers it.
   */
  private readonly leaseTokens = new Map<string, string>();

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
   *
   * `options.labels` are optional correlation tags (a build id, an
   * environment) recorded once on the run's `RunStarted` event and readable
   * back from {@link listRuns}; see `API.md` for the bounds (at most 16
   * labels, keys under 64 bytes, values under 256 bytes) and the honest-
   * absence rule. Omitted entirely, a run records none, byte-identical to a
   * client that predates this option.
   */
  async startRun(
    agent: string,
    input: unknown = null,
    options: { runId?: string; labels?: Labels } = {},
  ): Promise<string> {
    const body: Record<string, unknown> = { agent, input };
    if (options.runId !== undefined) body.run_id = options.runId;
    if (options.labels !== undefined) body.labels = options.labels;
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

  /**
   * Abandon a run: retire it by hand, appending a terminal `RunAbandoned`. The
   * operator's "we do not care about this run anymore" path, for a run that is
   * dead forever or no longer worth carrying. Executes nothing and drives
   * nothing; it needs no drive token, and works for any non-terminal run
   * whatever drove it.
   *
   * When the run is parked at a dangling write, the outstanding intent is
   * recorded on the terminal event and surfaced as
   * {@link RunStatus.unresolvedWrite}: the abandonment never claims the write
   * settled. A run that is already terminal is refused with a `409`.
   */
  async abandon(runId: string, reason?: string): Promise<AbandonResult> {
    const body =
      reason === undefined ? "{}" : JSON.stringify({ reason });
    return parseAbandonResult(
      await this.request("POST", `/v1/runs/${runId}/abandon`, {
        body,
        contentType: "application/json",
      }),
    );
  }

  // -- graphs ---------------------------------------------------------------

  /**
   * Submit and strictly validate a graph document; return its content hash and
   * whether this call is what stored it. A graph document IS its hash, so
   * re-submitting an identical document is idempotent: the same hash comes back
   * with `created: false`. A validation failure throws a `SalvorApiError` with
   * code `invalid_graph`, whose `details.errors` is the complete node- and
   * edge-precise list (nothing short-circuits); {@link validateGraph} asks the
   * same question without storing and without throwing.
   *
   * The server keeps submitted documents IN MEMORY only, so a restart drops
   * them and a hash from a previous process no longer resolves. That is safe
   * rather than lossy: submit the identical document again and it mints the
   * identical hash, so a caller can simply re-submit before starting a run.
   */
  async submitGraph(document: Graph): Promise<GraphSubmitted> {
    return parseGraphSubmitted(
      await this.request("POST", "/v1/graphs", {
        body: JSON.stringify(document),
        contentType: "application/json",
      }),
    );
  }

  /** List the stored graphs, each with its hash and shape summary. */
  async listGraphs(): Promise<GraphSummary[]> {
    const obj = await this.request("GET", "/v1/graphs");
    return ((obj.graphs as Record<string, unknown>[]) ?? []).map(
      parseGraphSummary,
    );
  }

  /**
   * Read one stored document back by hash. Throws a `SalvorApiError` with code
   * `unknown_graph` when nothing is stored under it, which is also what a hash
   * from before a server restart gets: see {@link submitGraph} on the in-memory
   * store.
   */
  async getGraph(graphHash: string): Promise<StoredGraph> {
    return parseStoredGraph(
      await this.request("GET", `/v1/graphs/${graphHash}`),
    );
  }

  /**
   * Validate a document without storing it: {@link submitGraph}'s dry run, the
   * graph counterpart of {@link replay}. It answers the question rather than
   * refusing the request, so an invalid document resolves with `valid: false`
   * and the full error list instead of throwing. Nothing is ever stored.
   */
  async validateGraph(document: Graph): Promise<GraphValidation> {
    return parseGraphValidation(
      await this.request("POST", "/v1/graphs/validate", {
        body: JSON.stringify(document),
        contentType: "application/json",
      }),
    );
  }

  /**
   * Start a fresh run of a STORED graph; return its run id. Fire-and-return,
   * exactly as {@link startRun} is for an agent run: the call resolves as soon
   * as the run is accepted, and the walk then drives in the background. A graph
   * run is an ordinary run with a richer log, so {@link getRun},
   * {@link streamEvents}, {@link replay} and {@link resume} all work on it
   * unchanged; {@link getRunGraph} adds the per-node view only a graph run has.
   *
   * Everything the document references is resolved BEFORE the run is spawned,
   * so a reference that cannot resolve is a refusal rather than a run that
   * fails halfway:
   *
   * - `unknown_graph` when the hash names no stored graph (including a hash
   *   from before a server restart, since the graph store is in memory).
   * - `unknown_agent`, naming the node, when an `agent` node references a hash
   *   no agent is registered under.
   * - `unknown_tool`, naming the node, when a `tool` node names a tool the
   *   server's registry does not hold. A stock `salvor serve` wires that
   *   registry EMPTY, so on a default server EVERY `tool` node refuses this
   *   way until a host registers the tool it names (`salvor serve
   *   --demo-tools` is the built-in way to get a non-empty registry).
   *
   * `options.labels` are optional correlation tags under the same bounds an
   * agent run's carry; omitted entirely, the run records none.
   */
  async startGraphRun(
    graphHash: string,
    input: unknown = null,
    options: { labels?: Labels } = {},
  ): Promise<string> {
    const body: Record<string, unknown> = { graph_hash: graphHash, input };
    if (options.labels !== undefined) body.labels = options.labels;
    const obj = await this.request("POST", "/v1/graph-runs", {
      body: JSON.stringify(body),
      contentType: "application/json",
    });
    return obj.run as string;
  }

  /**
   * A graph run's per-node projection: which nodes the walk has reached, which
   * case each `branch` fired, and the node it is inside right now. A node the
   * walk has not reached is absent rather than reported as some pending state.
   * Refuses with `not_a_graph_run` for an ordinary agent run, which has no walk
   * to project.
   */
  async getRunGraph(runId: string): Promise<GraphProjection> {
    return parseGraphProjection(
      await this.request("GET", `/v1/runs/${runId}/graph`),
    );
  }

  /**
   * Fork a graph run from a node boundary into a NEW run. The child opens with
   * the origin's log prefix rewritten under its own id and then continues from
   * `fromNode`; the origin is never touched, and the child reuses the origin's
   * graph unchanged (to change the graph, submit a new document and start a
   * fresh run).
   *
   * A fork that would re-execute a recorded write the operator has not
   * acknowledged is REFUSED, not silently replayed: a `SalvorApiError` with
   * code `write_replay_hazard`, whose `details.writes` lists exactly the writes
   * still needing acknowledgement. Pass their `seq`s as
   * `options.acknowledgeWrites` to accept that they may re-fire, and they are
   * recorded permanently on the child. {@link previewFork} asks the same
   * question first, without creating anything.
   */
  async forkRun(
    runId: string,
    fromNode: string,
    options: { acknowledgeWrites?: readonly number[] } = {},
  ): Promise<ForkResult> {
    const body: Record<string, unknown> = { from_node: fromNode };
    if (options.acknowledgeWrites !== undefined) {
      body.acknowledge_writes = options.acknowledgeWrites;
    }
    return parseForkResult(
      await this.request("POST", `/v1/runs/${runId}/fork`, {
        body: JSON.stringify(body),
        contentType: "application/json",
      }),
    );
  }

  /**
   * Preview a fork without creating one: which writes the re-walked segment
   * holds, which of them are still unacknowledged, and whether the fork would
   * proceed. The structural refusals ({@link forkRun}'s `invalid_fork_node`,
   * `origin_needs_reconciliation`, `not_a_graph_run`) still throw here, so a
   * fork that could never proceed is reported rather than faked.
   */
  async previewFork(
    runId: string,
    fromNode: string,
    options: { acknowledgeWrites?: readonly number[] } = {},
  ): Promise<ForkPreview> {
    const body: Record<string, unknown> = {
      from_node: fromNode,
      dry_run: true,
    };
    if (options.acknowledgeWrites !== undefined) {
      body.acknowledge_writes = options.acknowledgeWrites;
    }
    return parseForkPreview(
      await this.request("POST", `/v1/runs/${runId}/fork`, {
        body: JSON.stringify(body),
        contentType: "application/json",
      }),
    );
  }

  /**
   * The forks of a run, as the server's own DERIVED index: an origin is
   * immutable and never points forward at its children, so this is a scan of
   * every run's recorded origin, labelled `derived` to say so.
   */
  async listForks(runId: string): Promise<ForksIndex> {
    return parseForksIndex(
      await this.request("GET", `/v1/runs/${runId}/forks`),
    );
  }

  // -- client-performed tools -------------------------------------------------

  /**
   * List the client-performed tool declarations this server holds: tools an
   * operator declared with `salvor serve --client-tool <FILE>`, which the
   * client runs itself (see {@link ClientRunDriver.clientToolIntent}).
   *
   * This is how a client-driven loop gets the function definitions to hand the
   * model: a declaration's `inputSchema` IS the model tool's parameter schema,
   * the same schema the server checks a call's input against, published here
   * so a client never keeps a second copy that can quietly drift from it.
   */
  async listClientTools(): Promise<ClientToolDecl[]> {
    const obj = await this.request("GET", "/v1/client-tools");
    return ((obj.client_tools as Record<string, unknown>[]) ?? []).map(
      parseClientToolDecl,
    );
  }

  // -- client-driven runs ---------------------------------------------------

  /**
   * Open or re-open a client-driven run over this client's base URL and auth.
   * Returns a {@link ClientRunDriver}: the client owns the agent loop and streams
   * the events it produces, while the server owns the durable log and guards
   * every append. This is the second of Salvor's two modes; {@link startRun} is
   * the server-driven first.
   *
   * This instance remembers the drive token it was last handed for a given
   * `runId` and presents it here automatically when `options.driveToken` is
   * not given explicitly. That is what lets this SAME client re-open a run it
   * already drove, a moment or a day later, without being refused as a
   * stranger to its own lease (see {@link LeaseHeldError}): the run's own
   * driver, in the sense the server means it, is whichever caller last
   * presented that lease's token, and this object is the one place in a
   * process that would know it. A genuinely different driver, whether a
   * second `SalvorClient` in this same process or a wholly separate one, has
   * no such memory and is refused exactly as it should be while the lease it
   * is trying to take is still current. Pass `options.driveToken` explicitly
   * to override this (or to present a token this instance never saw, read
   * back from wherever an application persists it).
   */
  openClientRun(options: OpenClientRunOptions = {}): Promise<ClientRunDriver> {
    const token = this.headers["Authorization"]?.replace(/^Bearer /, "");
    const cached = options.runId ? this.leaseTokens.get(options.runId) : undefined;
    return openClientRun(this.baseUrl, {
      token,
      timeoutMs: this.timeoutMs,
      driveToken: cached,
      ...options,
    }).then((driver) => {
      this.leaseTokens.set(driver.runId, driver.driveToken);
      return driver;
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
