import type { Graph } from '@salvor-run/client';

/**
 * Typed views over the graph control-plane's JSON (`crates/salvor-server/API.md`, "Graphs and
 * graph runs"). Same posture as `@salvor-run/client`'s own `types.ts`: thin decoders, camelCase
 * fields, and a `raw` escape hatch on every decoded object so a server field this layer has not
 * been taught yet is never lost.
 *
 * Absent-vs-null throughout, matching the server's own documented contract: a key the server
 * genuinely omits decodes to `undefined` (never a fabricated default), and the one field the
 * server sends as an EXPLICIT `null` (`ForkHazardWrite.idempotencyKey`, when a write recorded no
 * idempotency key) decodes to `null`, not `undefined` — the two are different facts on the wire
 * and stay different here.
 */

// -- GET /v1/graphs -----------------------------------------------------------------------------

export interface GraphSummary {
  readonly graph: string;
  readonly nodeCount: number;
  readonly edgeCount: number;
  readonly entryNodes: readonly string[];
  readonly terminalNodes: readonly string[];
  readonly raw: Record<string, unknown>;
}

export function parseGraphSummary(obj: Record<string, unknown>): GraphSummary {
  return {
    graph: obj['graph'] as string,
    nodeCount: Number(obj['node_count'] ?? 0),
    edgeCount: Number(obj['edge_count'] ?? 0),
    entryNodes: (obj['entry_nodes'] as string[]) ?? [],
    terminalNodes: (obj['terminal_nodes'] as string[]) ?? [],
    raw: obj,
  };
}

// -- GET /v1/graphs/{hash} -----------------------------------------------------------------------

export interface GraphDocumentRecord {
  readonly graph: string;
  readonly document: Graph;
  readonly raw: Record<string, unknown>;
}

export function parseGraphDocumentRecord(obj: Record<string, unknown>): GraphDocumentRecord {
  return {
    graph: obj['graph'] as string,
    document: obj['document'] as Graph,
    raw: obj,
  };
}

// -- POST /v1/graphs/validate --------------------------------------------------------------------

export interface GraphSummaryCounts {
  readonly nodeCount: number;
  readonly edgeCount: number;
  readonly entryNodes: readonly string[];
  readonly terminalNodes: readonly string[];
}

function parseGraphSummaryCounts(obj: Record<string, unknown>): GraphSummaryCounts {
  return {
    nodeCount: Number(obj['node_count'] ?? 0),
    edgeCount: Number(obj['edge_count'] ?? 0),
    entryNodes: (obj['entry_nodes'] as string[]) ?? [],
    terminalNodes: (obj['terminal_nodes'] as string[]) ?? [],
  };
}

/** One node/edge-precise validation failure. `node`/`edge`/`missing` name what the error is
 * about; only the fields the server actually attached for that error code are present. */
export interface GraphValidationError {
  readonly code: string;
  readonly message: string;
  readonly node?: string;
  readonly edge?: { readonly from: string; readonly to: string };
  readonly missing?: string;
  /** The server sends this as an explicit `null` when it has no suggestion to offer (see the
   * module doc's absent-vs-null note) — never simply omitted once a `suggestion` key is relevant. */
  readonly suggestion?: string | null;
  readonly raw: Record<string, unknown>;
}

function parseGraphValidationError(obj: Record<string, unknown>): GraphValidationError {
  const out: {
    code: string;
    message: string;
    node?: string;
    edge?: { from: string; to: string };
    missing?: string;
    suggestion?: string | null;
    raw: Record<string, unknown>;
  } = {
    code: (obj['code'] as string) ?? 'unknown',
    message: (obj['message'] as string) ?? '',
    raw: obj,
  };
  if (obj['node'] !== undefined) out.node = obj['node'] as string;
  if (obj['edge'] !== undefined) out.edge = obj['edge'] as { from: string; to: string };
  if (obj['missing'] !== undefined) out.missing = obj['missing'] as string;
  if ('suggestion' in obj) out.suggestion = obj['suggestion'] as string | null;
  return out;
}

export type ValidateResult =
  | { readonly valid: true; readonly graph: string; readonly summary: GraphSummaryCounts; readonly raw: Record<string, unknown> }
  | { readonly valid: false; readonly errors: readonly GraphValidationError[]; readonly raw: Record<string, unknown> };

export function parseValidateResult(obj: Record<string, unknown>): ValidateResult {
  if (obj['valid'] === true) {
    return {
      valid: true,
      graph: obj['graph'] as string,
      summary: parseGraphSummaryCounts((obj['summary'] as Record<string, unknown>) ?? {}),
      raw: obj,
    };
  }
  return {
    valid: false,
    errors: ((obj['errors'] as Record<string, unknown>[]) ?? []).map(parseGraphValidationError),
    raw: obj,
  };
}

// -- POST /v1/graph-runs --------------------------------------------------------------------------

export interface GraphRunStart {
  readonly run: string;
  readonly status: string;
  readonly raw: Record<string, unknown>;
}

export function parseGraphRunStart(obj: Record<string, unknown>): GraphRunStart {
  return {
    run: obj['run'] as string,
    status: obj['status'] as string,
    raw: obj,
  };
}

// -- GET /v1/runs/{id}/graph ------------------------------------------------------------------------

export type NodeState = 'entered' | 'exited' | 'skipped';

/** One node's progress in the walk. `reason`/`branchCase`/`map` are present only when the server
 * recorded them (a node the walk has not reached is simply absent from `nodes`, distinct from a
 * `skipped` entry). */
export interface NodeProgress {
  readonly node: string;
  readonly state: NodeState;
  readonly reason?: string;
  readonly branchCase?: string;
  readonly map?: unknown;
  readonly fold?: unknown;
  readonly raw: Record<string, unknown>;
}

function parseNodeProgress(obj: Record<string, unknown>): NodeProgress {
  const out: {
    node: string;
    state: NodeState;
    reason?: string;
    branchCase?: string;
    map?: unknown;
    fold?: unknown;
    raw: Record<string, unknown>;
  } = {
    node: obj['node'] as string,
    state: (obj['state'] as NodeState) ?? 'skipped',
    raw: obj,
  };
  if (obj['reason'] !== undefined) out.reason = obj['reason'] as string;
  if (obj['branch_case'] !== undefined) out.branchCase = obj['branch_case'] as string;
  if (obj['map'] !== undefined) out.map = obj['map'];
  if (obj['fold'] !== undefined) out.fold = obj['fold'];
  return out;
}

export interface ForkOrigin {
  readonly runId: string;
  readonly throughSeq: number;
  readonly fromNode: string;
  readonly graphHash: string;
  readonly acknowledgedWrites: readonly number[];
  readonly raw: Record<string, unknown>;
}

export function parseForkOrigin(obj: Record<string, unknown>): ForkOrigin {
  return {
    runId: obj['run_id'] as string,
    throughSeq: Number(obj['through_seq'] ?? 0),
    fromNode: obj['from_node'] as string,
    graphHash: obj['graph_hash'] as string,
    acknowledgedWrites: ((obj['acknowledged_writes'] as number[]) ?? []).map(Number),
    raw: obj,
  };
}

/** A graph run's per-node projection, for the canvas. `currentNode` is present only while a node
 * is entered and not yet exited; `forkedFrom` is present only on a forked run's projection. */
export interface GraphProjection {
  readonly graphHash: string;
  readonly currentNode?: string;
  readonly nodes: readonly NodeProgress[];
  readonly forkedFrom?: ForkOrigin;
  readonly raw: Record<string, unknown>;
}

export function parseGraphProjection(obj: Record<string, unknown>): GraphProjection {
  const out: {
    graphHash: string;
    currentNode?: string;
    nodes: NodeProgress[];
    forkedFrom?: ForkOrigin;
    raw: Record<string, unknown>;
  } = {
    graphHash: obj['graph_hash'] as string,
    nodes: ((obj['nodes'] as Record<string, unknown>[]) ?? []).map(parseNodeProgress),
    raw: obj,
  };
  if (obj['current_node'] !== undefined) out.currentNode = obj['current_node'] as string;
  if (obj['forked_from'] !== undefined) {
    out.forkedFrom = parseForkOrigin(obj['forked_from'] as Record<string, unknown>);
  }
  return out;
}

// -- POST /v1/runs/{id}/fork ------------------------------------------------------------------------

/** One recorded write the re-walked segment would re-execute. `idempotencyKey` is an EXPLICIT
 * `null` on the wire when the write recorded none — see the module doc's absent-vs-null note. */
export interface ForkHazardWrite {
  readonly seq: number;
  readonly tool: string;
  readonly input: unknown;
  readonly idempotencyKey: string | null;
  readonly recordedAt: string;
  readonly raw: Record<string, unknown>;
}

function parseForkHazardWrite(obj: Record<string, unknown>): ForkHazardWrite {
  return {
    seq: Number(obj['seq'] ?? 0),
    tool: obj['tool'] as string,
    input: obj['input'],
    idempotencyKey: (obj['idempotency_key'] as string | null) ?? null,
    recordedAt: obj['recorded_at'] as string,
    raw: obj,
  };
}

/**
 * The typed result of a fork attempt. `kind: 'hazard'` is the `409 write_replay_hazard` refusal,
 * surfaced as data (never a thrown string) so a hazard-review dialog can render `writes`
 * verbatim. `kind: 'preview'` is a `dry_run: true` response (created nothing); `kind: 'forked'` is
 * the real thing.
 */
export type ForkOutcome =
  | { readonly kind: 'forked'; readonly run: string; readonly status: string; readonly forkedFrom: ForkOrigin; readonly raw: Record<string, unknown> }
  | {
      readonly kind: 'hazard';
      readonly code: 'write_replay_hazard';
      readonly message: string;
      readonly writes: readonly ForkHazardWrite[];
      readonly raw: Record<string, unknown>;
    }
  | {
      readonly kind: 'preview';
      readonly origin: string;
      readonly fromNode: string;
      readonly throughSeq: number;
      readonly graphHash: string;
      readonly prefixEventCount: number;
      readonly writes: readonly ForkHazardWrite[];
      readonly unacknowledgedWrites: readonly number[];
      readonly wouldProceed: boolean;
      readonly raw: Record<string, unknown>;
    };

export function parseForkCreated(obj: Record<string, unknown>): ForkOutcome {
  return {
    kind: 'forked',
    run: obj['run'] as string,
    status: obj['status'] as string,
    forkedFrom: parseForkOrigin((obj['forked_from'] as Record<string, unknown>) ?? {}),
    raw: obj,
  };
}

export function parseForkPreview(obj: Record<string, unknown>): ForkOutcome {
  return {
    kind: 'preview',
    origin: obj['origin'] as string,
    fromNode: obj['from_node'] as string,
    throughSeq: Number(obj['through_seq'] ?? 0),
    graphHash: obj['graph_hash'] as string,
    prefixEventCount: Number(obj['prefix_event_count'] ?? 0),
    writes: ((obj['writes'] as Record<string, unknown>[]) ?? []).map(parseForkHazardWrite),
    unacknowledgedWrites: ((obj['unacknowledged_writes'] as number[]) ?? []).map(Number),
    wouldProceed: Boolean(obj['would_proceed'] ?? false),
    raw: obj,
  };
}

/** Decode a `409 write_replay_hazard` error envelope's `details` into the same {@link ForkOutcome}
 * union, so a caller (and the dialog reading it) never has to branch on "did this come from the
 * error path or the success path". */
export function parseForkHazard(message: string, details: Record<string, unknown>): ForkOutcome {
  return {
    kind: 'hazard',
    code: 'write_replay_hazard',
    message,
    writes: ((details['writes'] as Record<string, unknown>[]) ?? []).map(parseForkHazardWrite),
    raw: details,
  };
}

// -- GET /v1/runs/{id}/forks --------------------------------------------------------------------------

export interface ForkListEntry {
  readonly run: string;
  readonly fromNode: string;
  readonly throughSeq: number;
  readonly acknowledgedWrites: readonly number[];
  readonly raw: Record<string, unknown>;
}

function parseForkListEntry(obj: Record<string, unknown>): ForkListEntry {
  return {
    run: obj['run'] as string,
    fromNode: obj['from_node'] as string,
    throughSeq: Number(obj['through_seq'] ?? 0),
    acknowledgedWrites: ((obj['acknowledged_writes'] as number[]) ?? []).map(Number),
    raw: obj,
  };
}

/** The forks of a run, as the server's own DERIVED index — `derived: true` always, per `API.md`:
 * "It is not a fact the origin recorded." */
export interface ForksIndex {
  readonly run: string;
  readonly derived: boolean;
  readonly forks: readonly ForkListEntry[];
  readonly raw: Record<string, unknown>;
}

export function parseForksIndex(obj: Record<string, unknown>): ForksIndex {
  return {
    run: obj['run'] as string,
    derived: Boolean(obj['derived'] ?? false),
    forks: ((obj['forks'] as Record<string, unknown>[]) ?? []).map(parseForkListEntry),
    raw: obj,
  };
}

// -- GET /v1/capabilities --------------------------------------------------------------------------

/** The exact build serving the response (`API.md`, "GET /v1/capabilities"): `version` is the
 * server's own `CARGO_PKG_VERSION`, always present; `commit` is the short git hash the build was
 * compiled from, present only when that build had a `.git` history to read — absent, never a
 * fabricated placeholder, for a source-tarball or no-git build. */
export interface ServerBuildInfo {
  readonly version: string;
  readonly commit?: string;
}

export interface CapabilityProbe {
  readonly fork: boolean;
  /** Absent when the server predates this field (a pre-`server`-block build): a decoder that only
   * ever sees `capabilities.fork` still parses cleanly, matching the `raw` escape hatch's own
   * absent-vs-null posture above. */
  readonly server?: ServerBuildInfo;
  readonly raw: Record<string, unknown>;
}

function parseServerBuildInfo(obj: Record<string, unknown>): ServerBuildInfo | undefined {
  const server = obj['server'] as Record<string, unknown> | undefined;
  if (typeof server?.['version'] !== 'string') return undefined;
  const commit = server['commit'];
  return typeof commit === 'string' ? { version: server['version'], commit } : { version: server['version'] };
}

export function parseCapabilityProbe(obj: Record<string, unknown>): CapabilityProbe {
  const caps = (obj['capabilities'] as Record<string, unknown>) ?? {};
  const server = parseServerBuildInfo(obj);
  return server === undefined
    ? { fork: caps['fork'] === true, raw: obj }
    : { fork: caps['fork'] === true, server, raw: obj };
}
