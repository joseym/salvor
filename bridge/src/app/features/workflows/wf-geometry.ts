import type { WfEdge, WfGraph } from './wf-model';

/**
 * THE CANVAS GEOMETRY: the pure math the pan/zoom surface runs on, lifted from the prototype so it
 * can be unit-tested without a DOM. A node is {@link NODE_W}x{@link NODE_H}; columns sit a
 * {@link PITCH} apart. Positions come from a layered topological layout (server graphs carry no
 * layout sidecar, so the drawing is derived from the edges, never guessed).
 */
export const NODE_W = 208;
export const NODE_H = 104;
export const PITCH_X = 300;
export const PITCH_Y = 160;

/**
 * THE DUPLICATE-STACK OFFSET. Two nodes claiming the same id cannot be placed apart by an id-keyed
 * layout, so the renderer nudges each successive one down-and-right by this much: the consequence
 * of the duplicate, made visible rather than hidden. Larger than the prototype's 16px so the
 * under-card's kind eyebrow and title stay readable under the top card, and paired on the canvas
 * with the {@link WfDupStack} caption chip so the overlap READS as a shared id, not a glitch.
 */
export const DUP_STACK_OFFSET = 24;

/** The corner radius an orthogonal trace turns with: the prototype's `r`, one value everywhere. */
const CORNER_R = 10;
/** The gap below the whole node band where a rerouted trace runs its horizontal: clear of every
 * card by this margin, so a channel run never grazes a node. */
const CHANNEL_MARGIN = 40;
/** Parallel channel traces are stacked this far apart, so two reroutes never merge into one rule. */
const LANE_STEP = 28;
/** The horizontal a rerouted trace steps into the column GAP by, on the way out of the source and
 * into the target: half the layered gap (PITCH_X − NODE_W = 92), so the vertical runs sit clear of
 * both the node they leave and the node they enter. */
const CHANNEL_STUB = 46;

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
 * THE SIDECAR, ported from the prototype. Coordinates live here, keyed by the graph's own key:
 * NEVER inside the hashed document, so moving a node can never change a graph's identity. The
 * prototype hand-authored a layout for each fixture graph; the one graph this build ships with a
 * known key is the seeded `refund-sweep` draft, so its positions are ported verbatim. A published
 * SERVER graph arrives from the control plane with no sidecar, so {@link layeredLayout} draws it.
 *
 * The clean left-to-right spine is exactly what a computed longest-path layout could NOT produce
 * for this graph: its dangling edge and its cycle scramble the topological columns (the fetch node
 * lands five columns out, past every node it feeds), which is the overlap the operator reported.
 * The two `n_charge` nodes share an id on purpose, the duplicate-id defect, so they share this
 * one entry and the renderer nudges the second into view.
 */
export const LAYOUTS: Record<string, WfLayout> = {
  'draft:refund-sweep': {
    n_start: { x: 0, y: 180 },
    n_fetch: { x: 300, y: 180 },
    n_charge: { x: 600, y: 180 },
    n_pick: { x: 900, y: 180 },
    n_fan: { x: 1200, y: 30 },
    n_notify: { x: 1200, y: 330 },
  },
};

/**
 * The layout a graph is DRAWN with: its hand-authored sidecar when one exists (the ported drafts),
 * else the computed {@link layeredLayout}. Ported from the prototype's `wfLayout()` fallthrough.
 */
export function layoutFor(g: WfGraph): WfLayout {
  const sidecar = LAYOUTS[g.key];
  // A hand-authored sidecar describes a specific set of nodes. The moment an author adds one it no
  // longer describes this graph, and a node the sidecar has never heard of would be drawn at the
  // origin, on top of whatever the sidecar put there. So the sidecar is used only while it still
  // covers the whole graph; past that the layout is derived, which is collision-free by construction.
  if (sidecar && g.nodes.every((n) => sidecar[n.id] !== undefined)) return sidecar;
  return layeredLayout(g);
}

/**
 * A layered left-to-right layout derived from the topological order: a node's COLUMN is the longest
 * path from any entry to it (so an edge always points rightward), and rows within a column stack
 * downward. Distinct ids never share a (column, row), so a computed graph is collision-free by
 * construction: the honest stand-in for a hand-authored sidecar, which only the fixture graphs had.
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

export interface WfEdgePath {
  /** The `d` of the trace itself. */
  readonly d: string;
  /** The `d` of the single fine arrowhead at the target port. */
  readonly arrow: string;
  /** Where a case label sits so the paper-knockout clears the rule, never strikes through it. */
  readonly lx: number;
  readonly ly: number;
}

export type Pt = readonly [number, number];

/** A node box for routing: its top-left corner; every box is {@link NODE_W}×{@link NODE_H}. */
export interface WfBox {
  readonly x: number;
  readonly y: number;
}

/**
 * The routing context {@link wfRoutes} hands each edge. When {@link channelY} is set the trace is
 * rerouted through a clear horizontal channel below the node band (a back-edge, or a forward edge
 * whose direct elbow would cut through a card); otherwise the direct elbow is drawn. Keeping this a
 * plain data input is what lets {@link wfPath} stay a pure function of its arguments, unit-testable
 * with and without a reroute.
 */
export interface WfPathCtx {
  /** Absolute y of the below-band channel this trace runs its horizontal along, when rerouted. */
  readonly channelY?: number;
  /** How far the exit/approach stubs step into the column gap; defaults to {@link CHANNEL_STUB}. */
  readonly stub?: number;
}

/**
 * THE POLYLINE a trace follows, in graph coordinates: the ordered corner points, before any corner
 * radius is applied. This is the single source the drawn `d`, the arrowhead, and the node-clearance
 * check all derive from, so what is tested for intersections is exactly what is drawn.
 *
 * - A same-rank forward edge is two points: a straight rule.
 * - A rerouted edge ({@link WfPathCtx.channelY} set) leaves the source toward the target, steps into
 *   the adjacent column gap, drops to the channel below the whole band, runs across it, rises in the
 *   gap before the target, and enters the target's LEFT port horizontally, so the arrowhead stays
 *   the same fine horizontal triangle on every route. Back-edges leave the LEFT port (the trace then
 *   flows leftward and never doubles back across its own card); forward edges leave the RIGHT port.
 * - Otherwise it is the prototype's elbow: out the right port, to the halfway column, up or down,
 *   into the left port.
 */
export function edgePoints(a: WfBox, b: WfBox, ctx?: WfPathCtx): Pt[] {
  const x1 = a.x + NODE_W;
  const y1 = a.y + NODE_H / 2;
  const x2 = b.x;
  const y2 = b.y + NODE_H / 2;
  if (Math.abs(y2 - y1) < 1 && b.x > a.x && ctx?.channelY === undefined) {
    return [
      [x1, y1],
      [x2, y2],
    ]; // same rank, forward, and not rerouted: a straight rule
  }
  if (ctx?.channelY !== undefined) {
    const stub = ctx.stub ?? CHANNEL_STUB;
    const forward = b.x >= a.x;
    const ex = forward ? a.x + NODE_W : a.x; // leave the right port going forward, the left going back
    const sx = ex + (forward ? stub : -stub); // step into the column gap beside the source
    const ax = x2 - stub; // rise in the gap just before the target's left port
    return [
      [ex, y1],
      [sx, y1],
      [sx, ctx.channelY],
      [ax, ctx.channelY],
      [ax, y2],
      [x2, y2],
    ];
  }
  const mx = (x1 + x2) / 2;
  return [
    [x1, y1],
    [mx, y1],
    [mx, y2],
    [x2, y2],
  ];
}

/** The fine horizontal arrowhead at a target's left port: one triangle, the prototype's exactly. */
function arrowAt(x: number, y: number): string {
  return `M ${x - 7} ${y - 4} L ${x} ${y} L ${x - 7} ${y + 4} Z`;
}

/** An orthogonal polyline drawn with a rounded corner (a quadratic joint) at every interior vertex,
 * the radius clamped per corner so it never overshoots a short segment. Straight two-point runs pass
 * through as a plain `L`. This is the ONE corner treatment, so the elbow and the channel reroute
 * turn identically. */
function orthoRound(pts: readonly Pt[], r: number): string {
  if (pts.length < 2) return '';
  if (pts.length === 2) return `M ${pts[0][0]} ${pts[0][1]} L ${pts[1][0]} ${pts[1][1]}`;
  let d = `M ${pts[0][0]} ${pts[0][1]}`;
  for (let i = 1; i < pts.length - 1; i++) {
    const [px, py] = pts[i - 1];
    const [cx, cy] = pts[i];
    const [nx, ny] = pts[i + 1];
    const inLen = Math.hypot(cx - px, cy - py) || 1;
    const outLen = Math.hypot(nx - cx, ny - cy) || 1;
    const rr = Math.min(r, inLen / 2, outLen / 2);
    const sx = cx - ((cx - px) / inLen) * rr;
    const sy = cy - ((cy - py) / inLen) * rr;
    const qx = cx + ((nx - cx) / outLen) * rr;
    const qy = cy + ((ny - cy) / outLen) * rr;
    d += ` L ${round(sx)} ${round(sy)} Q ${cx} ${cy} ${round(qx)} ${round(qy)}`;
  }
  const last = pts[pts.length - 1];
  d += ` L ${last[0]} ${last[1]}`;
  return d;
}

function round(n: number): number {
  return Math.round(n * 100) / 100;
}

/**
 * An ORTHOGONAL trace, the way a wiring diagram runs one, built from its {@link edgePoints}. Without
 * a reroute context this is the prototype's elbow (out the right port, to the halfway column, into
 * the left port) or a straight same-rank rule. With {@link WfPathCtx.channelY} set it runs the
 * clear below-band channel {@link wfRoutes} assigned it. The stroke, the single fine arrowhead, and
 * the label knockout are the prototype's; only the geometry is aware of the cards now.
 */
export function wfPath(a: WfBox, b: WfBox, ctx?: WfPathCtx): WfEdgePath {
  const pts = edgePoints(a, b, ctx);
  const [tx, ty] = pts[pts.length - 1];
  const arrow = arrowAt(tx, ty);
  const d = orthoRound(pts, CORNER_R);
  let lx: number;
  let ly: number;
  if (pts.length === 2) {
    lx = (pts[0][0] + pts[1][0]) / 2;
    ly = pts[0][1]; // same rank: the label sits ON the rule
  } else if (ctx?.channelY !== undefined) {
    lx = pts[1][0]; // the label rides the source drop, staying near the card it leaves
    ly = Math.min(ctx.channelY - CORNER_R, pts[0][1] + 22);
  } else {
    lx = pts[1][0]; // the elbow's vertical run
    ly = (pts[0][1] + pts[2][1]) / 2;
  }
  return { d, arrow, lx, ly };
}

/** Every axis-aligned segment of a route, as [start, end] pairs: what a node-clearance check walks. */
export function edgeSegments(a: WfBox, b: WfBox, ctx?: WfPathCtx): [Pt, Pt][] {
  const pts = edgePoints(a, b, ctx);
  const segs: [Pt, Pt][] = [];
  for (let i = 0; i < pts.length - 1; i++) segs.push([pts[i], pts[i + 1]]);
  return segs;
}

/** True iff an axis-aligned segment enters a node's box, padded by `pad` on every side. Both a
 * segment and a padded box are rectangles, so this is a rectangle overlap; strict inequalities mean
 * a trace lying exactly on the padded edge does not count as a strike. */
export function segHitsBox(seg: readonly [Pt, Pt], box: WfBox, pad = 0): boolean {
  const bx0 = box.x - pad;
  const by0 = box.y - pad;
  const bx1 = box.x + NODE_W + pad;
  const by1 = box.y + NODE_H + pad;
  const sx0 = Math.min(seg[0][0], seg[1][0]);
  const sx1 = Math.max(seg[0][0], seg[1][0]);
  const sy0 = Math.min(seg[0][1], seg[1][1]);
  const sy1 = Math.max(seg[0][1], seg[1][1]);
  return sx0 < bx1 && sx1 > bx0 && sy0 < by1 && sy1 > by0;
}

/** True iff any segment of a route strikes a node OTHER than the edge's own endpoints. The one
 * predicate {@link wfRoutes} reroutes on, and the tests assert never holds on the drawn routes. */
export function routeHitsForeignNode(
  a: WfBox,
  b: WfBox,
  boxes: readonly (WfBox & { id: string })[],
  from: string,
  to: string,
  ctx?: WfPathCtx,
  pad = 0,
): boolean {
  const segs = edgeSegments(a, b, ctx);
  return boxes.some(
    (box) => box.id !== from && box.id !== to && segs.some((s) => segHitsBox(s, box, pad)),
  );
}

/**
 * ROUTE EVERY EDGE of a laid-out graph so no trace runs through a card. Aligned to `g.edges` (an
 * edge whose endpoints are not both laid out is `undefined`). A trace keeps its direct elbow when
 * that elbow is clear; a back-edge, or a forward edge whose elbow would cut through an intervening
 * card, is rerouted through a horizontal channel below the whole node band. Reroutes get their own
 * stacked lanes so two never merge, and each still enters the target's left port horizontally so the
 * arrowhead and the visual grammar are unchanged; only the geometry now clears the cards.
 *
 * Pure: derived entirely from the graph and its layout, so it is unit-testable without a DOM.
 */
export function wfRoutes(g: WfGraph, layout: WfLayout): (WfEdgePath | undefined)[] {
  const boxes = g.nodes
    .filter((n) => layout[n.id] !== undefined)
    .map((n) => ({ id: n.id, x: layout[n.id].x, y: layout[n.id].y }));
  const bandBottom = boxes.length ? Math.max(...boxes.map((box) => box.y + NODE_H)) : 0;
  let lane = 0;
  return g.edges.map((e) => {
    const a = layout[e.from];
    const b = layout[e.to];
    if (a === undefined || b === undefined) return undefined;
    // A back-edge is rerouted on sight (its direct trace would double back across its own card);
    // a forward edge only when its direct elbow actually strikes an intervening card.
    const reroute =
      b.x <= a.x || routeHitsForeignNode(a, b, boxes, e.from, e.to);
    if (!reroute) return wfPath(a, b);
    const channelY = bandBottom + CHANNEL_MARGIN + lane * LANE_STEP;
    lane += 1;
    return wfPath(a, b, { channelY });
  });
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

/** The reset-to-100% view, centred on the entry node: the `#wf-reset` control. */
export function wfReset(g: WfGraph, layout: WfLayout, box: { readonly width: number; readonly height: number }, firstEntered?: string): WfView {
  const at = layout[entryNode(g, layout, firstEntered)] ?? { x: 0, y: 0 };
  return { k: 1, x: box.width / 2 - (at.x + NODE_W / 2), y: box.height / 2 - (at.y + NODE_H / 2) };
}

/** The `%` zoom readout, e.g. `73%`. */
export function zoomPercent(k: number): string {
  return `${Math.round(k * 100)}%`;
}
