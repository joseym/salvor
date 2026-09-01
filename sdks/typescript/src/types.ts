/**
 * Typed views over the JSON the control plane returns.
 *
 * These types stay thin. The event envelope and derived state are defined by
 * the server (see `crates/salvor-replay/src/event.rs` and
 * `crates/salvor-server/API.md`); the client surfaces the common fields and
 * keeps the full decoded JSON on `raw` so nothing the server adds later is
 * lost.
 */

import type { Graph } from "./graph.js";

/** Token counts folded from a run's model calls. */
export interface Usage {
  inputTokens: number;
  outputTokens: number;
}

/**
 * A run's folded status: always a `state` name plus state-specific keys.
 * A `completed` run carries `output`; a `suspended` run carries `reason` and
 * `inputSchema`; a `failed` run carries `error`. Everything is on `raw` too.
 */
export interface RunStatus {
  state: string;
  output?: unknown;
  error?: string;
  reason?: string;
  inputSchema?: unknown;
  /** When a `sleeping` run's durable timer comes due. Absent in every other
   * state. A sleeping run is not waiting on anybody: it continues when this
   * instant passes and something drives it again, which for a client-driven
   * run is its own driver (see `ClientRunDriver.awaitWake`). */
  wakeAt?: string;
  /** True once a `sleeping` run's deadline has passed, alongside
   * `overdueSeconds` (whole seconds since `wakeAt`). Both are absent, not
   * `false`/`0`, on a sleeping run that has not reached its deadline yet, and
   * absent for every other state: the server computes this against its own
   * clock rather than the client deriving it from `wakeAt`, so the two keys
   * arrive together or not at all. */
  overdue?: boolean;
  overdueSeconds?: number;
  /** Present only on an `abandoned` run that was parked at a dangling write:
   * the outstanding intent (`seq`, `tool`) the abandonment recorded rather than
   * claiming settled. Absent for every other abandonment and every other state. */
  unresolvedWrite?: { seq: number; tool: string };
  raw: Record<string, unknown>;
}

/**
 * The step a run is waiting on. `kind` is `"model"` or `"tool"`. A tool pending
 * call carries `tool`, `input`, and `effect`.
 */
export interface PendingCall {
  kind: string;
  seq: number;
  tool?: string;
  effect?: string;
  input?: unknown;
  raw: Record<string, unknown>;
}

/**
 * A run's liveness evidence: `"attached"` when a driver is currently running it
 * (a live server task, or a current client-driven lease), `"none"` when none is.
 * Absent (undefined) for a terminal run (a finished run needs no driver), and
 * absent from an older server that predates the field. It is server-reported
 * evidence, not a verdict: a client derives a `stalled` state from a `running`
 * run whose driver is `"none"` and whose last event has gone stale.
 */
export type Driver = "attached" | "none";

/**
 * Attribution for a hand-recorded tool-call resolution, from `GET
 * /v1/runs/{id}`'s `resolution`. Present only when a run's log holds a
 * `ToolCallCompleted` an operator recorded by hand (see `POST
 * /v1/runs/{id}/resolve` in `API.md`) rather than the run recording its own
 * output; a run never resolved this way carries no `resolution` at all. A
 * run with more than one hand-recorded completion over its life reports the
 * last.
 *
 * `settledBy` is always `"operator"` when this object is present.
 * `settledCaller` names the person: the token's name on a server with a
 * bearer configured, absent on a server running the pass-through posture.
 * `seq` is the completed call's own recorded sequence number.
 *
 * `settledCaller` carries the same trust limit {@link RunState.caller} does:
 * a name recorded like any other field, worth what control of that
 * credential is worth (see `SECURITY.md`).
 */
export interface Resolution {
  settledBy: string;
  settledCaller?: string;
  seq: number;
  raw: Record<string, unknown>;
}

/** The derived state of one run, from `GET /v1/runs/{id}`. */
export interface RunState {
  run: string;
  status: RunStatus;
  eventCount: number;
  usage?: Usage;
  pending?: PendingCall;
  firstRecordedAt?: string;
  lastRecordedAt?: string;
  driver?: Driver;
  /**
   * The name of whoever asked for this run, read off its `RunStarted`. On a
   * server with a bearer configured this is the name the token was declared
   * under; from the CLI it is the operating system user who ran the verb.
   * Absent when the run recorded no caller, which is every run a
   * pass-through server started and every run recorded before the field
   * existed. See {@link RunSummary.caller}.
   *
   * The name is attribution to the token or the operating system account,
   * worth what control of that credential is worth: it is recorded like any
   * other field, so it is only as trustworthy as write access to the store
   * (see `SECURITY.md`).
   */
  caller?: string;
  /** Who resolved a dangling write by hand, when the log records one. See
   * {@link Resolution}. Absent for a run never resolved this way. */
  resolution?: Resolution;
  raw: Record<string, unknown>;
}

/**
 * Free-form correlation tags on a run (a build id, an environment), set once
 * at creation. Plain string-to-string, matching the server's `labels` object.
 */
export type Labels = Record<string, string>;

/**
 * One row of `GET /v1/runs`: a run id with its folded status and counts.
 *
 * `usage`, `stepCount`, `agentDefHash`, and `labels` are additive: present
 * whenever the run's log folds (a real zero when a run genuinely has no model
 * calls yet), and absent (not a fabricated zero) only when the server could
 * not read that run's log at all (see `API.md`). `labels` follows the same
 * rule one step further: also absent when a run recorded no labels at all, or
 * recorded an explicit empty set: the server never sends `labels: {}`. `raw`
 * always carries whatever the server actually sent, so a server-side field
 * this SDK has not been taught yet is never lost.
 */
export interface RunSummary {
  run: string;
  status: RunStatus;
  eventCount: number;
  firstRecordedAt?: string;
  lastRecordedAt?: string;
  usage?: Usage;
  stepCount?: number;
  agentDefHash?: string;
  labels?: Labels;
  /**
   * Liveness evidence: `"attached"` / `"none"` for a non-terminal run, absent
   * for a terminal one (and absent from an older server). See {@link Driver}.
   * `lastRecordedAt` above is the companion "when did anything last happen"
   * evidence: a stalled run is a `running` run with `driver: "none"` whose
   * `lastRecordedAt` has gone stale.
   */
  driver?: Driver;
  /**
   * The name of whoever asked for this run, read off its `RunStarted`: the
   * token's name on a server with a bearer configured, the operating system
   * user from the CLI. Absent when the run recorded no caller, which a
   * pass-through server never records, and never a fabricated default.
   */
  caller?: string;
  raw: Record<string, unknown>;
}

/** The dry-run replay projection from `GET /v1/runs/{id}/replay`. */
export interface ReplayState {
  status: RunStatus;
  nextSeq: number;
  usage?: Usage;
  pending?: PendingCall;
  raw: Record<string, unknown>;
}

/** The result of a resume: `outcome` is `"driving"` or a finished state. */
export interface ResumeResult {
  run: string;
  outcome: string;
  status?: RunStatus;
  raw: Record<string, unknown>;
}

/**
 * The receipt from abandoning a run: the position the terminal `RunAbandoned`
 * landed at and the run's re-derived status (always `abandoned`, carrying the
 * operator reason and any recorded `unresolvedWrite`). Nothing was executed.
 */
export interface AbandonResult {
  run: string;
  appendedSeq?: number;
  status: RunStatus;
  raw: Record<string, unknown>;
}

export function parseAbandonResult(obj: Json): AbandonResult {
  return {
    run: obj.run as string,
    appendedSeq:
      obj.appended_seq === undefined || obj.appended_seq === null
        ? undefined
        : Number(obj.appended_seq),
    status: parseStatus(obj.status),
    raw: obj,
  };
}

/**
 * One recorded event, decoded from the pinned envelope wire JSON. The same
 * bytes arrive as a stream frame's `data` and as a log row, so one decoder
 * serves both. `kind` names what happened; `payload` holds its fields.
 */
export interface SalvorEvent {
  runId: string;
  seq: number;
  schemaVersion: number;
  recordedAt: string;
  kind: string;
  payload: Record<string, unknown>;
}

/**
 * The terminal `event: end` frame that closes a stream. `status` is the run's
 * resting status; `detached` is true when the run was mid-step with no driver
 * in this server process.
 */
export interface EndFrame {
  status?: RunStatus;
  detached: boolean;
  error?: string;
  raw: Record<string, unknown>;
}

// -- graphs -----------------------------------------------------------------

/**
 * A graph document's shape: how many nodes and edges it holds, and the ends of
 * its walk. Sent per row by `GET /v1/graphs`, and by
 * `POST /v1/graphs/validate` for a document that validates.
 */
export interface GraphShape {
  nodeCount: number;
  edgeCount: number;
  entryNodes: string[];
  terminalNodes: string[];
}

/**
 * The receipt from `POST /v1/graphs`: the document's content hash, and whether
 * this call is what stored it. `created` is `false` when the identical document
 * was already stored, so re-submitting is idempotent rather than a conflict:
 * the same document always hashes to the same id.
 */
export interface GraphSubmitted {
  graph: string;
  created: boolean;
  raw: Record<string, unknown>;
}

/** One row of `GET /v1/graphs`: a stored document's hash and its shape. */
export interface GraphSummary extends GraphShape {
  graph: string;
  raw: Record<string, unknown>;
}

/** One stored document read back by hash, from `GET /v1/graphs/{hash}`. */
export interface StoredGraph {
  graph: string;
  document: Graph;
  raw: Record<string, unknown>;
}

/**
 * One validation failure, node- or edge-precise. `code` is the stable token
 * (`dangling_edge`, `duplicate_id`, `node_name_too_long`, and a document that
 * does not even parse is the single `malformed_document`); `node` or `edge`
 * names where the failure is, and at most one of the two is ever present.
 */
export interface GraphValidationError {
  code: string;
  message: string;
  node?: string;
  edge?: { from: string; to: string };
  raw: Record<string, unknown>;
}

/**
 * The answer from `POST /v1/graphs/validate`. It answers the question rather
 * than refusing the request: an invalid document comes back `valid: false` with
 * the complete error list instead of throwing, and nothing is stored either
 * way. `graph` and `summary` are present only when the document is valid;
 * `errors` is empty then.
 */
export interface GraphValidation {
  valid: boolean;
  graph?: string;
  summary?: GraphShape;
  errors: GraphValidationError[];
  raw: Record<string, unknown>;
}

/**
 * One node's progress in a graph run's walk. `state` is `entered`, `exited`, or
 * `skipped`; a skipped node carries the `reason`. `branchCase` is the case a
 * `branch` fired, present only once decided. A `map` node's fan-out is on `raw`
 * verbatim. A node the walk has not reached is absent from the projection
 * entirely, which is distinct from a `skipped` one.
 */
export interface GraphNodeProgress {
  node: string;
  state: string;
  reason?: string;
  branchCase?: string;
  raw: Record<string, unknown>;
}

/**
 * A graph run's per-node projection, from `GET /v1/runs/{id}/graph`: which
 * nodes the walk has reached, and the graph it is walking. `currentNode` is
 * present only while a node is entered and not yet exited; `forkedFrom` only on
 * a run that was forked.
 */
export interface GraphProjection {
  graphHash: string;
  currentNode?: string;
  nodes: GraphNodeProgress[];
  forkedFrom?: ForkOrigin;
  raw: Record<string, unknown>;
}

/**
 * The origin a forked run records permanently on its seq-0 `GraphRunStarted`:
 * where it came from, how much of the origin's log it opens with, and which
 * recorded writes the operator accepted may re-fire.
 */
export interface ForkOrigin {
  runId: string;
  throughSeq: number;
  fromNode: string;
  graphHash: string;
  acknowledgedWrites: number[];
  raw: Record<string, unknown>;
}

/** The receipt from a real fork: the new child run, and the origin it records. */
export interface ForkResult {
  run: string;
  status: string;
  forkedFrom?: ForkOrigin;
  raw: Record<string, unknown>;
}

/**
 * One recorded write in the segment a fork would re-walk. A write with an
 * `idempotencyKey` is not a hazard (the provider collapses the duplicate); one
 * without needs acknowledging by `seq` before the fork may proceed.
 */
export interface RecordedWrite {
  seq: number;
  tool: string;
  input?: unknown;
  idempotencyKey?: string;
  recordedAt?: string;
  raw: Record<string, unknown>;
}

/**
 * A fork's dry-run preview, from `POST /v1/runs/{id}/fork` with
 * `dry_run: true`. Nothing is created. `unacknowledgedWrites` is exactly the
 * set of `seq`s a real fork would refuse over, and `wouldProceed` says whether
 * the fork as previewed could go ahead.
 */
export interface ForkPreview {
  origin: string;
  fromNode: string;
  throughSeq: number;
  graphHash: string;
  prefixEventCount: number;
  writes: RecordedWrite[];
  unacknowledgedWrites: number[];
  wouldProceed: boolean;
  raw: Record<string, unknown>;
}

/** One row of the forks index: a child run and the boundary it forked at. */
export interface ForkEntry {
  run: string;
  fromNode: string;
  throughSeq: number;
  acknowledgedWrites: number[];
  raw: Record<string, unknown>;
}

/**
 * The forks of a run, from `GET /v1/runs/{id}/forks`. `derived` is the server
 * saying so out loud: an origin is immutable and never points forward at its
 * children, so this index is a scan of every run's recorded origin rather than
 * a fact the origin holds.
 */
export interface ForksIndex {
  run: string;
  derived: boolean;
  forks: ForkEntry[];
  raw: Record<string, unknown>;
}

/**
 * One operator-declared tool the CLIENT performs, from `GET /v1/client-tools`.
 * `inputSchema` is the same JSON Schema a client-tool intent's `input` is
 * checked against server-side, which is also, verbatim, the parameter schema
 * to hand the model as this tool's function definition: one schema, not a
 * copy a client keeps in step by hand. `outputSchema` is present only when the
 * declaration carries one; a tool declared without one cannot be
 * self-completed by a client (see `ClientRunDriver.clientToolCompletion`).
 * `trustCompletion` is `false` unless the operator opted in: silence gets the
 * safe direction. `requireEqual` lists the fields whose reported value the
 * server pins to the intent's recorded value, and is empty unless the
 * declaration names one. `idempotencyKey` lists the input fields a
 * client-tool intent's derived key is taken from, and is present only when
 * the declaration names them; absent, the key is positional (`(run, seq,
 * tool)`, one identity per call site) rather than by value.
 */
export interface ClientToolDecl {
  name: string;
  effect: string;
  inputSchema: unknown;
  outputSchema?: unknown;
  trustCompletion: boolean;
  requireEqual: string[];
  idempotencyKey?: string[];
  raw: Record<string, unknown>;
}

// -- decoders ---------------------------------------------------------------

type Json = Record<string, unknown>;

export function parseUsage(obj: unknown): Usage | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const o = obj as Json;
  return {
    inputTokens: Number(o.input_tokens ?? 0),
    outputTokens: Number(o.output_tokens ?? 0),
  };
}

export function parseStatus(obj: unknown): RunStatus {
  const o = (obj ?? {}) as Json;
  const uw = o.unresolved_write as
    | { seq?: unknown; tool?: unknown }
    | undefined;
  return {
    state: (o.state as string) ?? "unknown",
    output: o.output,
    error: o.error as string | undefined,
    reason: o.reason as string | undefined,
    inputSchema: o.input_schema,
    wakeAt: o.wake_at as string | undefined,
    overdue: o.overdue as boolean | undefined,
    overdueSeconds: o.overdue_seconds as number | undefined,
    unresolvedWrite:
      uw && typeof uw === "object"
        ? { seq: Number(uw.seq ?? 0), tool: String(uw.tool ?? "") }
        : undefined,
    raw: o,
  };
}

function parsePending(obj: unknown): PendingCall | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const o = obj as Json;
  return {
    kind: (o.kind as string) ?? "unknown",
    seq: Number(o.seq ?? 0),
    tool: o.tool as string | undefined,
    effect: o.effect as string | undefined,
    input: o.input,
    raw: o,
  };
}

function parseResolution(obj: unknown): Resolution | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const o = obj as Json;
  return {
    settledBy: (o.settled_by as string) ?? "operator",
    settledCaller: o.settled_caller as string | undefined,
    seq: Number(o.seq ?? 0),
    raw: o,
  };
}

export function parseRunState(obj: Json): RunState {
  return {
    run: obj.run as string,
    status: parseStatus(obj.status),
    eventCount: Number(obj.event_count ?? 0),
    usage: parseUsage(obj.usage),
    pending: parsePending(obj.pending),
    firstRecordedAt: obj.first_recorded_at as string | undefined,
    lastRecordedAt: obj.last_recorded_at as string | undefined,
    driver: parseDriver(obj.driver),
    caller: obj.caller as string | undefined,
    resolution: parseResolution(obj.resolution),
    raw: obj,
  };
}

/** `"attached"`/`"none"`, or undefined for anything else (a terminal run omits
 * it, and an older server never sends it), never a fabricated default. */
function parseDriver(value: unknown): Driver | undefined {
  return value === "attached" || value === "none" ? value : undefined;
}

export function parseRunSummary(obj: Json): RunSummary {
  return {
    run: obj.run as string,
    status: parseStatus(obj.status),
    eventCount: Number(obj.event_count ?? 0),
    firstRecordedAt: obj.first_recorded_at as string | undefined,
    lastRecordedAt: obj.last_recorded_at as string | undefined,
    usage: parseUsage(obj.usage),
    stepCount:
      obj.step_count === undefined ? undefined : Number(obj.step_count),
    agentDefHash: obj.agent_def_hash as string | undefined,
    labels: obj.labels as Labels | undefined,
    driver: parseDriver(obj.driver),
    caller: obj.caller as string | undefined,
    raw: obj,
  };
}

export function parseReplayState(obj: Json): ReplayState {
  return {
    status: parseStatus(obj.status),
    nextSeq: Number(obj.next_seq ?? 0),
    usage: parseUsage(obj.usage),
    pending: parsePending(obj.pending),
    raw: obj,
  };
}

export function parseResumeResult(obj: Json): ResumeResult {
  const status = obj.status;
  return {
    run: obj.run as string,
    outcome: (obj.outcome as string) ?? "unknown",
    status:
      status && typeof status === "object" ? parseStatus(status) : undefined,
    raw: obj,
  };
}

export function parseEvent(obj: Json): SalvorEvent {
  const event = (obj.event ?? {}) as Json;
  return {
    runId: obj.run_id as string,
    seq: Number(obj.seq),
    schemaVersion: Number(obj.schema_version ?? 1),
    recordedAt: (obj.recorded_at as string) ?? "",
    kind: (event.kind as string) ?? "Unknown",
    payload: (event.payload as Record<string, unknown>) ?? {},
  };
}

export function parseEndFrame(obj: Json): EndFrame {
  const status = obj.status;
  return {
    status:
      status && typeof status === "object" ? parseStatus(status) : undefined,
    detached: Boolean(obj.detached ?? false),
    error: obj.error as string | undefined,
    raw: obj,
  };
}

/** Token usage on a `ModelCallCompleted` event, else undefined. */
export function eventUsage(event: SalvorEvent): Usage | undefined {
  return parseUsage(event.payload.usage);
}

// -- graph decoders ---------------------------------------------------------

function parseShape(obj: unknown): GraphShape {
  const o = (obj ?? {}) as Json;
  return {
    nodeCount: Number(o.node_count ?? 0),
    edgeCount: Number(o.edge_count ?? 0),
    entryNodes: (o.entry_nodes as string[]) ?? [],
    terminalNodes: (o.terminal_nodes as string[]) ?? [],
  };
}

export function parseGraphSubmitted(obj: Json): GraphSubmitted {
  return {
    graph: obj.graph as string,
    created: Boolean(obj.created ?? false),
    raw: obj,
  };
}

export function parseGraphSummary(obj: Json): GraphSummary {
  return { graph: obj.graph as string, ...parseShape(obj), raw: obj };
}

export function parseStoredGraph(obj: Json): StoredGraph {
  return {
    graph: obj.graph as string,
    document: obj.document as Graph,
    raw: obj,
  };
}

function parseGraphValidationError(obj: Json): GraphValidationError {
  const edge = obj.edge as { from?: unknown; to?: unknown } | undefined;
  return {
    code: (obj.code as string) ?? "unknown",
    message: (obj.message as string) ?? "",
    node: obj.node as string | undefined,
    edge:
      edge && typeof edge === "object"
        ? { from: String(edge.from ?? ""), to: String(edge.to ?? "") }
        : undefined,
    raw: obj,
  };
}

export function parseGraphValidation(obj: Json): GraphValidation {
  const summary = obj.summary;
  return {
    valid: Boolean(obj.valid ?? false),
    graph: obj.graph as string | undefined,
    summary:
      summary && typeof summary === "object" ? parseShape(summary) : undefined,
    errors: ((obj.errors as Json[]) ?? []).map(parseGraphValidationError),
    raw: obj,
  };
}

function parseGraphNodeProgress(obj: Json): GraphNodeProgress {
  return {
    node: obj.node as string,
    state: (obj.state as string) ?? "unknown",
    reason: obj.reason as string | undefined,
    branchCase: obj.branch_case as string | undefined,
    raw: obj,
  };
}

export function parseGraphProjection(obj: Json): GraphProjection {
  return {
    graphHash: obj.graph_hash as string,
    currentNode: obj.current_node as string | undefined,
    nodes: ((obj.nodes as Json[]) ?? []).map(parseGraphNodeProgress),
    forkedFrom: parseForkOrigin(obj.forked_from),
    raw: obj,
  };
}

function parseForkOrigin(obj: unknown): ForkOrigin | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const o = obj as Json;
  return {
    runId: o.run_id as string,
    throughSeq: Number(o.through_seq ?? 0),
    fromNode: o.from_node as string,
    graphHash: o.graph_hash as string,
    acknowledgedWrites: ((o.acknowledged_writes as number[]) ?? []).map(Number),
    raw: o,
  };
}

export function parseForkResult(obj: Json): ForkResult {
  return {
    run: obj.run as string,
    status: (obj.status as string) ?? "unknown",
    forkedFrom: parseForkOrigin(obj.forked_from),
    raw: obj,
  };
}

/** One recorded write. `idempotency_key` arrives as an explicit `null` for a
 * write that has none, which is absence, not the string `"null"`. */
export function parseRecordedWrite(obj: Json): RecordedWrite {
  const key = obj.idempotency_key;
  return {
    seq: Number(obj.seq ?? 0),
    tool: (obj.tool as string) ?? "",
    input: obj.input,
    idempotencyKey: key === null || key === undefined ? undefined : String(key),
    recordedAt: obj.recorded_at as string | undefined,
    raw: obj,
  };
}

export function parseForkPreview(obj: Json): ForkPreview {
  return {
    origin: obj.origin as string,
    fromNode: obj.from_node as string,
    throughSeq: Number(obj.through_seq ?? 0),
    graphHash: obj.graph_hash as string,
    prefixEventCount: Number(obj.prefix_event_count ?? 0),
    writes: ((obj.writes as Json[]) ?? []).map(parseRecordedWrite),
    unacknowledgedWrites: ((obj.unacknowledged_writes as number[]) ?? []).map(
      Number,
    ),
    wouldProceed: Boolean(obj.would_proceed ?? false),
    raw: obj,
  };
}

function parseForkEntry(obj: Json): ForkEntry {
  return {
    run: obj.run as string,
    fromNode: obj.from_node as string,
    throughSeq: Number(obj.through_seq ?? 0),
    acknowledgedWrites: ((obj.acknowledged_writes as number[]) ?? []).map(
      Number,
    ),
    raw: obj,
  };
}

export function parseForksIndex(obj: Json): ForksIndex {
  return {
    run: obj.run as string,
    derived: Boolean(obj.derived ?? false),
    forks: ((obj.forks as Json[]) ?? []).map(parseForkEntry),
    raw: obj,
  };
}

export function parseClientToolDecl(obj: Json): ClientToolDecl {
  return {
    name: obj.name as string,
    effect: obj.effect as string,
    inputSchema: obj.input_schema,
    outputSchema: obj.output_schema,
    trustCompletion: Boolean(obj.trust_completion ?? false),
    requireEqual: Array.isArray(obj.require_equal) ? (obj.require_equal as string[]) : [],
    idempotencyKey: Array.isArray(obj.idempotency_key)
      ? (obj.idempotency_key as string[])
      : undefined,
    raw: obj,
  };
}
