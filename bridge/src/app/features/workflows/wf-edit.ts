import type { BranchCase, BranchCondition } from '@salvor-run/client';

import { WF_KINDS } from './wf-kinds';
import {
  type WfEdge,
  type WfGraph,
  type WfNode,
  type WfNodeKind,
  branchCases,
  documentNode,
  withBranchCases,
} from './wf-model';

/**
 * THE STRUCTURAL EDITS: adding and removing a node, drawing and cutting an edge, and giving a branch
 * a case. Pure functions from a graph to a graph, so the canvas's history (which snapshots the whole
 * document before every mutation) covers them exactly as it covers a field edit, and every one of
 * them is testable without a DOM.
 *
 * Two rules live here rather than in a handler, because a handler is somewhere they could be
 * forgotten:
 *
 * REMOVING A NODE DOES NOT REMOVE ITS EDGES. This is a deliberate divergence from `salvor graph
 * edit`, whose `rm node` drops every edge naming the node: there, nothing is on screen until you
 * type `validate`, so a cascade keeps the document tidy. Here the validator's list is ON the page at
 * all times, so the edges that just lost an endpoint become named `dangling_edge` errors, each one
 * pointing at what it used to reach. That is information about a decision the author just made, and
 * a cascade would spend it: five edges silently vanishing looks identical to deleting a node that
 * nothing referenced.
 *
 * NOTHING HERE JUDGES. A self-edge, a second edge between the same pair, a case with a nonsense
 * expression, a body pointing at nothing: all of them land, and {@link validateGraph} is what says
 * so. An edit that refused what the validator already reports would be a second opinion, and the
 * first thing to drift.
 */

/** A node id no node in the graph is using, of the form `agent_1`, `tool_2`: the kind, so the id says
 * what the node is, and the lowest free ordinal, so ids do not climb as an author adds and undoes. */
export function freeNodeId(g: WfGraph, kind: WfNodeKind): string {
  const taken = new Set(g.nodes.map((n) => n.id));
  let n = 1;
  while (taken.has(`${kind}_${n}`)) n += 1;
  return `${kind}_${n}`;
}

/**
 * Append a node of `kind`, seeded by {@link WF_KINDS} and read into the canvas model through
 * {@link documentNode}: the same path an opened document's nodes take, so a drawn node and a read one
 * are the same kind of value and publish emits them the same way.
 *
 * @returns the new graph and the id of the node added, so the caller can select it.
 */
export function addNode(g: WfGraph, kind: WfNodeKind): { graph: WfGraph; id: string } {
  const id = freeNodeId(g, kind);
  const node = documentNode(WF_KINDS[kind].seed(id));
  return { graph: { ...g, nodes: [...g.nodes, node] }, id };
}

/** Drop one node, and ONLY the node: see this module's note on why its edges stay behind. */
export function deleteNode(g: WfGraph, id: string): WfGraph {
  return { ...g, nodes: g.nodes.filter((n) => n.id !== id) };
}

/** How many edges still name `id` at either end: what a delete is about to turn into errors, said
 * before it happens rather than discovered afterwards. */
export function edgesTouching(g: WfGraph, id: string): number {
  return g.edges.filter((e) => e.from === id || e.to === id).length;
}

/**
 * Draw an edge. An exact duplicate (same ends, same case) is a no-op: it would add nothing to the
 * document and the validator has nothing to say about it, so it would be a click that silently did
 * nothing visible. Everything else lands, including an edge to a node that does not exist and an
 * edge from a node to itself, both of which the validator names.
 */
export function addEdge(g: WfGraph, from: string, to: string, label?: string): WfGraph {
  const already = g.edges.some((e) => e.from === from && e.to === to && e.label === label);
  if (already) return g;
  const edge: WfEdge = label !== undefined ? { from, to, label } : { from, to };
  return { ...g, edges: [...g.edges, edge] };
}

/** Cut one edge by its position in the document's edge list, which is what the validator's errors
 * and the inspector's rows both address an edge by. */
export function removeEdgeAt(g: WfGraph, index: number): WfGraph {
  return { ...g, edges: g.edges.filter((_, i) => i !== index) };
}

/** Set (or clear) the case one edge realizes. Clearing means REMOVING the label, not setting it to
 * an empty string: the format's `label` is absent or a name, and an empty name is neither. */
export function setEdgeCase(g: WfGraph, index: number, label: string | undefined): WfGraph {
  return {
    ...g,
    edges: g.edges.map((e, i) => {
      if (i !== index) return e;
      return label === undefined || label === '' ? { from: e.from, to: e.to } : { from: e.from, to: e.to, label };
    }),
  };
}

/**
 * Give a branch a case, or replace the condition of one it already has by that name. Two cases of
 * one name would make an edge label ambiguous about which condition it realizes, so a repeat name
 * REPLACES rather than appends.
 */
export function setCase(g: WfGraph, nodeId: string, name: string, when: BranchCondition): WfGraph {
  if (!name) return g;
  return patchNode(g, nodeId, (n) => {
    if (n.kind !== 'branch') return n;
    const cases = branchCases(n);
    const at = cases.findIndex((c) => c.name === name);
    const next: BranchCase[] = at >= 0 ? cases.map((c, i) => (i === at ? { name, when } : c)) : [...cases, { name, when }];
    return withBranchCases(n, next);
  });
}

/** Drop one of a branch's cases. Any edge labelled with it stays, and becomes the validator's
 * `edge_type` error naming a case the branch no longer has: the same reason a deleted node leaves
 * its edges behind. */
export function removeCase(g: WfGraph, nodeId: string, name: string): WfGraph {
  return patchNode(g, nodeId, (n) =>
    n.kind === 'branch' ? withBranchCases(n, branchCases(n).filter((c) => c.name !== name)) : n,
  );
}

/** Replace one node, by id, leaving the rest of the document alone. */
export function patchNode(g: WfGraph, id: string, patch: (n: WfNode) => WfNode): WfGraph {
  return { ...g, nodes: g.nodes.map((n) => (n.id === id ? patch(n) : n)) };
}
