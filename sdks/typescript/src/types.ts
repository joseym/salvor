/**
 * Typed views over the JSON the control plane returns.
 *
 * These types stay thin. The event envelope and derived state are defined by
 * the server (see `crates/salvor-replay/src/event.rs` and
 * `crates/salvor-server/API.md`); the client surfaces the common fields and
 * keeps the full decoded JSON on `raw` so nothing the server adds later is
 * lost.
 */

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

/** The derived state of one run, from `GET /v1/runs/{id}`. */
export interface RunState {
  run: string;
  status: RunStatus;
  eventCount: number;
  usage?: Usage;
  pending?: PendingCall;
  firstRecordedAt?: string;
  lastRecordedAt?: string;
  raw: Record<string, unknown>;
}

/** One row of `GET /v1/runs`: a run id with its folded status and counts. */
export interface RunSummary {
  run: string;
  status: RunStatus;
  eventCount: number;
  firstRecordedAt?: string;
  lastRecordedAt?: string;
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

// -- decoders ---------------------------------------------------------------

type Json = Record<string, unknown>;

function parseUsage(obj: unknown): Usage | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const o = obj as Json;
  return {
    inputTokens: Number(o.input_tokens ?? 0),
    outputTokens: Number(o.output_tokens ?? 0),
  };
}

export function parseStatus(obj: unknown): RunStatus {
  const o = (obj ?? {}) as Json;
  return {
    state: (o.state as string) ?? "unknown",
    output: o.output,
    error: o.error as string | undefined,
    reason: o.reason as string | undefined,
    inputSchema: o.input_schema,
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

export function parseRunState(obj: Json): RunState {
  return {
    run: obj.run as string,
    status: parseStatus(obj.status),
    eventCount: Number(obj.event_count ?? 0),
    usage: parseUsage(obj.usage),
    pending: parsePending(obj.pending),
    firstRecordedAt: obj.first_recorded_at as string | undefined,
    lastRecordedAt: obj.last_recorded_at as string | undefined,
    raw: obj,
  };
}

export function parseRunSummary(obj: Json): RunSummary {
  return {
    run: obj.run as string,
    status: parseStatus(obj.status),
    eventCount: Number(obj.event_count ?? 0),
    firstRecordedAt: obj.first_recorded_at as string | undefined,
    lastRecordedAt: obj.last_recorded_at as string | undefined,
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
