import type { WfEdge, WfGraph } from './wf-model';

/**
 * THE CANVAS GEOMETRY — the pure math the pan/zoom surface runs on, lifted from the prototype so it
 * can be unit-tested without a DOM. A node is {@link NODE_W}x{@link NODE_H}; columns sit a
 * {@link PITCH} apart. Positions come from a layered topological layout (server graphs carry no
 * layout sidecar, so the drawing is derived from the edges, never guessed).
 */
export const NODE_W = 208;
export const NODE_H = 104;
export const PITCH_X = 300;
export const PITCH_Y = 160;

/**
 * THE LEGIBILITY FLOOR. `.wf-name` is a 15px serif; below 11px it stops reading, so zoom never
 * drops below 11/15. Fit obeys it: a graph too wide to fit above the floor clamps to the floor and
 * anchors the entry node at the left margin rather than shrinking the labels into mush (the minimap
 * carries the overview). Ported exactly: `WF_MIN_K = WF_LEGIBLE_PX / WF_NAME_PX`.
 */
export const WF_NAME_PX = 15;
export const WF_LEGIBLE_PX = 11;
export const WF_MIN_K = WF_LEGIBLE_PX / WF_NAME_PX; // ≈ 0.733
export const WF_MAX_K = 2;
export const WF_ABS_MIN_K = 0.25;
const FIT_PAD = 32;

export interface WfView {
  readonly k: number;
  readonly x: number;
  readonly y: number;
}

/** Kahn's topological order over the graph's edges, ties broken by a node's own order in the
 * document (stable, and derived from the graph, not from any layout). Cyclic or dangling-edge nodes
 * are still in the graph, so they are appended rather than dropped. Ported from `wfTopo`. */
export function wfTopo(g: WfGraph): string[] {
  const indeg: Record<string, number> = {};
  const out: Record<string, string[]> = {};
  g.nodes.forEach((n) => {
    indeg[n.id] = 0;
    out[n.id] = [];
  });
  g.edges.forEach((e: WfEdge) => {
    if (!(e.to in indeg) || !(e.from in out)) return; // a dangling edge has no order
    indeg[e.to]++;
    out[e.from].push(e.to);
  });
  const rank: Record<string, number> = {};
  g.nodes.forEach((n, i) => (rank[n.id] = i));
  const ready = g.nodes.filter((n) => indeg[n.id] === 0).map((n) => n.id);
  const order: string[] = [];
  while (ready.length) {
    ready.sort((a, b) => rank[a] - rank[b]);
    const id = ready.shift() as string;
    order.push(id);
    out[id].forEach((to) => {
      if (--indeg[to] === 0) ready.push(to);
    });
  }
  g.nodes.forEach((n) => {
    if (!order.includes(n.id)) order.push(n.id);
  });
  return order;
}

export type WfLayout = Record<string, { readonly x: number; readonly y: number }>;

/**
 * A layered left-to-right layout derived from the topological order: a node's COLUMN is the longest
 * path from any entry to it (so an edge always points rightward), and rows within a column stack
 * downward. This is the honest stand-in for the prototype's hand-authored `LAYOUTS` sidecar, which
 * only its two fixture graphs had — a real server graph has no sidecar, so its drawing is computed.
 */
export function layeredLayout(g: WfGraph): WfLayout {
  const order = wfTopo(g);
  const depth: Record<string, number> = {};
  order.forEach((id) => (depth[id] = 0));
  // Longest-path column: relax along edges in topological order.
  order.forEach((id) => {
    g.edges
      .filter((e) => e.from === id)
      .forEach((e) => {
        if (e.to in depth) depth[e.to] = Math.max(depth[e.to], depth[id] + 1);
      });
  });
  const rowInCol: Record<number, number> = {};
  const layout: WfLayout = {};
  // Place in topological order so a column's rows fill in reading order.
  order.forEach((id) => {
    const col = depth[id] ?? 0;
    const row = rowInCol[col] ?? 0;
    rowInCol[col] = row + 1;
    layout[id] = { x: col * PITCH_X, y: row * PITCH_Y };
  });
  return layout;
}

/** The bounding box of a laid-out graph, in graph coordinates. */
export function layoutBounds(g: WfGraph, layout: WfLayout): {
  minX: number;
  minY: number;
  width: number;
  height: number;
} {
  const xs = g.nodes.map((n) => layout[n.id]?.x ?? 0);
  const ys = g.nodes.map((n) => layout[n.id]?.y ?? 0);
  const minX = Math.min(...xs);
  const minY = Math.min(...ys);
  return { minX, minY, width: Math.max(...xs) + NODE_W - minX, height: Math.max(...ys) + NODE_H - minY };
}

/** The node a reading starts from: the run's first-entered node when known, else the leftmost node
 * in the layout (a graph is drawn left to right). Ported from `wfEntryNode`. */
export function entryNode(g: WfGraph, layout: WfLayout, firstEntered?: string): string {
  if (firstEntered && g.nodes.some((n) => n.id === firstEntered)) return firstEntered;
  return g.nodes.slice().sort((a, b) => (layout[a.id]?.x ?? 0) - (layout[b.id]?.x ?? 0))[0]?.id ?? '';
}

/**
 * Fit the graph to the viewport, obeying the legibility floor. Above the floor the whole graph is
 * centred; at or below it, k clamps to {@link WF_MIN_K} and the entry node anchors at the left
 * margin, vertically centred. Ported from `wfFit`. Pure: takes the viewport box and returns the
 * view rather than mutating a global.
 */
export function wfFit(
  g: WfGraph,
  layout: WfLayout,
  box: { readonly width: number; readonly height: number },
  firstEntered?: string,
): WfView {
  const { minX, minY, width, height } = layoutBounds(g, layout);
  const k = Math.min((box.width - FIT_PAD * 2) / width, (box.height - FIT_PAD * 2) / height, 1);
  if (k >= WF_MIN_K) {
    return {
      k,
      x: FIT_PAD - minX * k + (box.width - FIT_PAD * 2 - width * k) / 2,
      y: FIT_PAD - minY * k + (box.height - FIT_PAD * 2 - height * k) / 2,
    };
  }
  const at = layout[entryNode(g, layout, firstEntered)] ?? { x: 0, y: 0 };
  return {
    k: WF_MIN_K,
    x: FIT_PAD - at.x * WF_MIN_K,
    y: box.height / 2 - (at.y + NODE_H / 2) * WF_MIN_K,
  };
}

/**
 * Zoom by `mult`, keeping the point under the cursor fixed so the view does not lurch toward the
 * middle of nowhere. Clamped to [{@link WF_ABS_MIN_K}, {@link WF_MAX_K}]. Ported from `wfZoom`.
 */
export function wfZoom(view: WfView, mult: number, cx: number, cy: number): WfView {
  const k = Math.min(WF_MAX_K, Math.max(WF_ABS_MIN_K, view.k * mult));
  return {
    k,
    x: cx - (cx - view.x) * (k / view.k),
    y: cy - (cy - view.y) * (k / view.k),
  };
}

/** The reset-to-100% view, centred on the entry node — the `#wf-reset` control. */
export function wfReset(g: WfGraph, layout: WfLayout, box: { readonly width: number; readonly height: number }, firstEntered?: string): WfView {
  const at = layout[entryNode(g, layout, firstEntered)] ?? { x: 0, y: 0 };
  return { k: 1, x: box.width / 2 - (at.x + NODE_W / 2), y: box.height / 2 - (at.y + NODE_H / 2) };
}

/** The `%` zoom readout, e.g. `73%`. */
export function zoomPercent(k: number): string {
  return `${Math.round(k * 100)}%`;
}
