import type { Graph, GraphNode } from '@salvor-run/client';

import type { GraphSummary } from '../../core/api';

/**
 * The canvas's internal graph model — one flat shape both a SERVER graph (`GET /v1/graphs/{hash}`,
 * a {@link Graph} document) and an in-browser DRAFT normalise into, so the renderer, the picker,
 * the topological layout, the validator and the node menu all read one thing. Ported from the
 * prototype's `GRAPHS` fixture shape (id/kind/name per node, from/to/label per edge, a nullable
 * hash that IS the version once published), but sourced from the real control plane rather than a
 * canned object.
 */
export type WfNodeKind = 'agent' | 'tool' | 'gate' | 'branch' | 'map' | 'fold';

/** A tool node's declared effect class — the field fork safety turns on. */
export type WfEffect = 'read' | 'write' | 'idempotent';

export interface WfNode {
  readonly id: string;
  readonly kind: WfNodeKind;
  /** A human label. A server document's payload may carry its own `name` (the graph format's
   * optional node display name); when it does not, the id is the honest fallback — never an
   * invented sentence. A draft always carries an author-typed name (possibly still the id, if
   * never edited). */
  readonly name: string;
  // Kind-specific payload, optional because each field belongs to exactly one kind. The node
  // inspector panel and the validator read these; a field a document does not carry stays
  // undefined rather than defaulted.
  readonly agentHash?: string;
  readonly tool?: string;
  readonly effect?: string;
  readonly idempotencyKey?: string | null;
  readonly input?: unknown;
  readonly prompt?: string;
  readonly inputSchema?: unknown;
  readonly cases?: readonly string[];
  /** The subset of a branch's `cases` whose condition is `model_decision` rather than an
   * expression — a case the engine resolves by driving the branch's own `agentHash` at run time,
   * never by evaluating a predicate. Undefined (or empty) means every case is expression-decided,
   * so the validator's `model_decision_without_agent` check has nothing to require an agent for. */
  readonly modelCases?: readonly string[];
  readonly over?: string;
  readonly concurrency?: number;
  readonly body?: { readonly tool?: string; readonly effect?: string; readonly node?: string };
  // Fold-specific payload: the iteration bound, the stop predicate, and a short
  // label for the join rule ("best by score", "last", "all").
  readonly maxIterations?: number;
  readonly stopWhen?: string;
  readonly join?: string;
}

export interface WfEdge {
  readonly from: string;
  readonly to: string;
  readonly label?: string;
}

export interface WfGraph {
  /** The picker's option value and the app's local id. A draft's key is `draft:<name>`; a
   * published graph's key IS its hash. */
  readonly key: string;
  /** `null` until published — a draft has no identity, because the hash IS the version. */
  readonly hash: string | null;
  readonly name: string;
  readonly state: 'draft' | 'published';
  readonly nodes: readonly WfNode[];
  readonly edges: readonly WfEdge[];
}

/** The id of a server graph node — every kind's payload carries `id` (see `@salvor-run/client` graph). */
function nodeId(n: GraphNode): string {
  return n.payload.id;
}

/** One line of plain prose — what a node DOES, not what kind it is. Guarded for the fields a
 * server document may omit. */
export function nodeDoes(n: WfNode): string {
  switch (n.kind) {
    case 'agent':
      return `runs the agent at ${String(n.agentHash ?? '').slice(0, 13)}…`;
    case 'tool':
      return `calls ${n.tool ?? 'a tool'}`;
    case 'gate':
      return n.prompt ?? 'waits for a human approval';
    case 'branch':
      return `picks one of ${n.cases?.length ?? 0} cases`;
    case 'map':
      return `fans out over a list, ${n.concurrency ?? 0} at a time`;
    case 'fold':
      return `iterates up to ${n.maxIterations ?? 0} times, then ${n.join ?? 'joins'}`;
  }
}

/** Map one server node into the canvas model, keeping every kind-specific payload field the panel
 * or validator reads. A server document's node payload may carry its own optional `name`; when it
 * does, the node reads by that display name, and when it does not, the id is the honest fallback
 * (never an invented sentence). */
function fromServerNode(n: GraphNode): WfNode {
  const base = { id: nodeId(n), kind: n.kind, name: n.payload.name ?? nodeId(n) } as const;
  switch (n.kind) {
    case 'agent':
      return { ...base, agentHash: n.payload.agent_hash };
    case 'tool':
      return {
        ...base,
        tool: n.payload.tool,
        ...(n.payload.input !== undefined ? { input: n.payload.input } : {}),
      };
    case 'gate':
      return {
        ...base,
        ...(n.payload.prompt !== undefined ? { prompt: n.payload.prompt } : {}),
        inputSchema: n.payload.approval_schema,
      };
    case 'branch':
      return { ...base, cases: n.payload.cases.map((c) => c.name) };
    case 'map':
      return {
        ...base,
        over: n.payload.over,
        concurrency: n.payload.concurrency,
        body: n.payload.body.kind === 'node' ? { node: n.payload.body.value } : {},
      };
    case 'fold':
      return {
        ...base,
        maxIterations: n.payload.max_iterations,
        stopWhen: n.payload.stop_when,
        join: joinLabel(n.payload.join),
        body: n.payload.body.kind === 'node' ? { node: n.payload.body.value } : {},
      };
  }
}

/** A short prose label for a fold's join rule, for the node's `does` line and
 * the inspector. */
function joinLabel(join: { kind: string; value?: string }): string {
  switch (join.kind) {
    case 'best_by':
      return `keeps the best by ${join.value ?? ''}`.trimEnd();
    case 'last':
      return 'takes the last pass';
    case 'all':
      return 'collects every pass';
    default:
      return 'joins the passes';
  }
}

/**
 * Normalise a stored server document into the canvas model. The `name` shown in the picker is the
 * short hash — a published graph's identity is its hash.
 */
export function fromServerGraph(hash: string, doc: Graph, summary?: GraphSummary): WfGraph {
  const nodes: WfNode[] = doc.nodes.map(fromServerNode);
  const edges: WfEdge[] = doc.edges.map((e) => (e.label !== undefined ? { from: e.from, to: e.to, label: e.label } : { from: e.from, to: e.to }));
  void summary;
  return { key: hash, hash, name: shortHash(hash), state: 'published', nodes, edges };
}

/** `sha256:4f1c8ab3…` — the picker/name face of a published graph. */
export function shortHash(hash: string): string {
  return 'sha256:' + hash.replace(/^sha256:/, '').slice(0, 8);
}

/** One picker option: the value the `<select>` carries and the label it shows. Drafts first (the
 * author's own work), then published server graphs, each by its short hash. */
export interface WfPickOption {
  readonly value: string;
  readonly label: string;
  readonly state: 'draft' | 'published';
}

export function pickOptions(drafts: readonly WfGraph[], server: readonly WfGraph[]): WfPickOption[] {
  const d = drafts.map((g) => ({ value: g.key, label: g.name, state: g.state }));
  const s = server.map((g) => ({ value: g.key, label: g.name, state: g.state }));
  return [...d, ...s];
}
