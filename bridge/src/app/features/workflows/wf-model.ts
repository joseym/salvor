import type { Graph, GraphNode } from '@salvor/client';

import type { GraphSummary } from '../../core/api';

/**
 * The canvas's internal graph model — one flat shape both a SERVER graph (`GET /v1/graphs/{hash}`,
 * a {@link Graph} document) and an in-browser DRAFT normalise into, so the renderer, the picker,
 * the topological layout and the node menu all read one thing. Ported from the prototype's `GRAPHS`
 * fixture shape (id/kind/name per node, from/to/label per edge, a nullable hash that IS the version
 * once published), but sourced from the real control plane rather than a canned object.
 */
export type WfNodeKind = 'agent' | 'tool' | 'gate' | 'branch' | 'map';

export interface WfNode {
  readonly id: string;
  readonly kind: WfNodeKind;
  /** A human label. Server documents carry none (a node is its id), so the id is the honest
   * fallback — never an invented sentence. A draft may carry an author-typed name. */
  readonly name: string;
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

/** The id of a server graph node — every kind's payload carries `id` (see `@salvor/client` graph). */
function nodeId(n: GraphNode): string {
  return n.payload.id;
}

/**
 * Normalise a stored server document into the canvas model. A published server graph carries no
 * display names, so each node reads by its own id (honest: the document records no name to show).
 * The `name` shown in the picker is the short hash — a published graph's identity is its hash.
 */
export function fromServerGraph(hash: string, doc: Graph, summary?: GraphSummary): WfGraph {
  const nodes: WfNode[] = doc.nodes.map((n) => ({ id: nodeId(n), kind: n.kind, name: nodeId(n) }));
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
