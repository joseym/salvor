import { describe, expect, it } from 'vitest';

import {
  DUP_STACK_OFFSET,
  LAYOUTS,
  NODE_H,
  NODE_W,
  WF_MIN_K,
  type WfBox,
  type WfLayout,
  edgePoints,
  layeredLayout,
  layoutBounds,
  layoutFor,
  routeHitsForeignNode,
  segHitsBox,
  wfBodyPath,
  wfFit,
  wfPath,
  wfRoutes,
  wfTopo,
  wfZoom,
  zoomPercent,
} from './wf-geometry';
import { REFUND_SWEEP_DRAFT } from './wf-draft';
import type { WfGraph } from './wf-model';

/** True iff no two DISTINCT laid-out node boxes intersect. A duplicate id shares one layout entry,
 * so this is the honest non-overlap invariant: distinct nodes never collide. */
function noDistinctBoxesOverlap(layout: WfLayout): boolean {
  const ids = Object.keys(layout);
  for (let i = 0; i < ids.length; i++) {
    for (let j = i + 1; j < ids.length; j++) {
      const a = layout[ids[i]];
      const b = layout[ids[j]];
      if (a.x < b.x + NODE_W && a.x + NODE_W > b.x && a.y < b.y + NODE_H && a.y + NODE_H > b.y) {
        return false;
      }
    }
  }
  return true;
}

/** A simple chain a -> b -> c, plus a branch d off b. */
const chain: WfGraph = {
  key: 'g',
  hash: 'sha256:abc',
  name: 'g',
  state: 'published',
  nodes: [
    { id: 'a', kind: 'agent', name: 'a' },
    { id: 'b', kind: 'branch', name: 'b' },
    { id: 'c', kind: 'tool', name: 'c' },
    { id: 'd', kind: 'tool', name: 'd' },
  ],
  edges: [
    { from: 'a', to: 'b' },
    { from: 'b', to: 'c', label: 'yes' },
    { from: 'b', to: 'd', label: 'no' },
  ],
};

describe('wfTopo', () => {
  it('orders sources before their targets', () => {
    const order = wfTopo(chain);
    expect(order.indexOf('a')).toBeLessThan(order.indexOf('b'));
    expect(order.indexOf('b')).toBeLessThan(order.indexOf('c'));
    expect(order.indexOf('b')).toBeLessThan(order.indexOf('d'));
  });

  it('keeps a cyclic/dangling node rather than dropping it', () => {
    const cyclic: WfGraph = {
      ...chain,
      edges: [...chain.edges, { from: 'c', to: 'a' }],
    };
    expect(wfTopo(cyclic).sort()).toEqual(['a', 'b', 'c', 'd']);
  });
});

describe('layeredLayout: columns follow the edges, rightward', () => {
  it('places every edge target in a strictly later column than its source', () => {
    const layout = layeredLayout(chain);
    expect(layout['b'].x).toBeGreaterThan(layout['a'].x);
    expect(layout['c'].x).toBeGreaterThan(layout['b'].x);
    expect(layout['d'].x).toBeGreaterThan(layout['b'].x);
  });

  it('stacks two nodes in the same column at different rows', () => {
    const layout = layeredLayout(chain);
    expect(layout['c'].x).toBe(layout['d'].x);
    expect(layout['c'].y).not.toBe(layout['d'].y);
  });
});

describe('layoutFor: hand-authored sidecar vs computed, and non-overlap', () => {
  it('returns the ported sidecar for the refund-sweep draft, verbatim', () => {
    expect(layoutFor(REFUND_SWEEP_DRAFT)).toBe(LAYOUTS['draft:refund-sweep']);
    // the clean left-to-right spine the operator should see
    const L = layoutFor(REFUND_SWEEP_DRAFT);
    expect(L['n_start'].x).toBeLessThan(L['n_fetch'].x);
    expect(L['n_fetch'].x).toBeLessThan(L['n_charge'].x);
    expect(L['n_charge'].x).toBeLessThan(L['n_pick'].x);
  });

  it('falls back to the computed layout for a graph with no sidecar', () => {
    // chain has no LAYOUTS entry, so layoutFor must equal the computed layeredLayout.
    expect(layoutFor(chain)).toEqual(layeredLayout(chain));
  });

  it('THE OVERLAP DEFECT: the computed layout scrambles the draft, the sidecar fixes it', () => {
    // The draft's cycle (n_fan -> n_fetch) and dangling edge push n_fetch PAST the nodes it
    // feeds under the computed longest-path layout: the scrambled drawing the operator reported.
    const computed = layeredLayout(REFUND_SWEEP_DRAFT);
    expect(computed['n_fetch'].x).toBeGreaterThan(computed['n_charge'].x);
    // the ported sidecar restores the honest spine: fetch sits BEFORE charge.
    const sidecar = layoutFor(REFUND_SWEEP_DRAFT);
    expect(sidecar['n_fetch'].x).toBeLessThan(sidecar['n_charge'].x);
  });

  it('a laid-out graph never overlaps two DISTINCT nodes: draft and computed alike', () => {
    expect(noDistinctBoxesOverlap(layoutFor(REFUND_SWEEP_DRAFT))).toBe(true);
    expect(noDistinctBoxesOverlap(layeredLayout(chain))).toBe(true);
  });
});

describe('body-by-id: the layout slots the body node, the path draws it distinctly', () => {
  // A map whose body is another node by id, with the map wired into a real spine and the body node
  // referenced ONLY by that id (no edge touches it): the orphan the layered layout drops in column 0.
  const withBody: WfGraph = {
    key: 'g',
    hash: 'sha256:map',
    name: 'g',
    state: 'published',
    nodes: [
      { id: 'start', kind: 'tool', name: 'start' },
      { id: 'pay_each', kind: 'map', name: 'pay_each', over: 'roster', concurrency: 1, body: { node: 'pay_one' } },
      { id: 'pay_one', kind: 'tool', name: 'pay_one' },
    ],
    edges: [{ from: 'start', to: 'pay_each' }],
  };

  it('pulls a body-only node directly below its referencer, not into column 0', () => {
    const layout = layeredLayout(withBody);
    expect(layout['pay_one'].x).toBe(layout['pay_each'].x);
    expect(layout['pay_one'].y).toBeGreaterThan(layout['pay_each'].y);
    expect(noDistinctBoxesOverlap(layout)).toBe(true);
  });

  it('leaves a body node that also carries a real edge in its edge-derived column', () => {
    const alsoEdged: WfGraph = { ...withBody, edges: [...withBody.edges, { from: 'pay_each', to: 'pay_one' }] };
    const layout = layeredLayout(alsoEdged);
    expect(layout['pay_one'].x).toBeGreaterThan(layout['pay_each'].x);
  });

  it('draws a body directly below as a straight vertical drop with a downward arrowhead', () => {
    const path = wfBodyPath({ x: 300, y: 0 }, { x: 300, y: 160 });
    // one vertical segment down the source's centre line, no elbow
    expect(path.d).toBe(`M ${300 + NODE_W / 2} ${NODE_H} L ${300 + NODE_W / 2} 160`);
    expect(path.d).not.toContain('Q');
  });

  it('falls back to the ordinary elbow for a body that is not directly below', () => {
    expect(wfBodyPath({ x: 0, y: 0 }, { x: 600, y: 0 })).toEqual(wfPath({ x: 0, y: 0 }, { x: 600, y: 0 }));
  });

  it('a body reference never enters the topological order', () => {
    // pay_one has no edge, so it trails the ordered spine rather than being wired after pay_each.
    const order = wfTopo(withBody);
    expect(order).toContain('pay_one');
    expect(order.indexOf('start')).toBeLessThan(order.indexOf('pay_each'));
  });
});

describe('wfPath: orthogonal elbows with one fine arrowhead', () => {
  it('draws a same-rank edge as a straight rule (no cubic, no elbow)', () => {
    const path = wfPath({ x: 0, y: 0 }, { x: 300, y: 0 });
    expect(path.d).toMatch(/^M .* L [^Q]*$/);
    expect(path.d).not.toContain('C'); // never a cubic
    expect(path.d).not.toContain('Q'); // straight rank needs no corner
  });

  it('elbows a rank change with rounded corners (quadratic joints), never a cubic', () => {
    const path = wfPath({ x: 0, y: 0 }, { x: 300, y: 160 });
    expect(path.d).toContain('Q'); // corner radii
    expect(path.d).not.toContain('C');
  });

  it('lands the arrowhead at the target left port, as a closed triangle', () => {
    const path = wfPath({ x: 0, y: 0 }, { x: 300, y: 0 });
    // target left port is at x = 300, y = NODE_H/2
    expect(path.arrow).toContain(`L 300 ${NODE_H / 2}`);
    expect(path.arrow.trim().endsWith('Z')).toBe(true);
  });
});

/** Every coordinate pair in a path `d`, in order, as consecutive segments: the drawn polyline
 * (corner chords included). What the router draws is exactly what this walks for clearance. */
function pathSegments(d: string): [readonly [number, number], readonly [number, number]][] {
  const nums = (d.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number);
  const pts: [number, number][] = [];
  for (let i = 0; i + 1 < nums.length; i += 2) pts.push([nums[i], nums[i + 1]]);
  const segs: [readonly [number, number], readonly [number, number]][] = [];
  for (let i = 0; i + 1 < pts.length; i++) segs.push([pts[i], pts[i + 1]]);
  return segs;
}

/** For each drawn route, the ids of any node OTHER than the edge's own endpoints its polyline
 * enters (padded). The routing invariant: this is empty for every edge, on every graph. */
function routeFouls(g: WfGraph, layout: WfLayout, pad: number): string[] {
  const boxes = g.nodes
    .filter((n) => layout[n.id] !== undefined)
    .map((n) => ({ id: n.id, x: layout[n.id].x, y: layout[n.id].y }));
  const routes = wfRoutes(g, layout);
  const fouls: string[] = [];
  g.edges.forEach((e, i) => {
    const route = routes[i];
    if (!route) return;
    const segs = pathSegments(route.d);
    for (const box of boxes) {
      if (box.id === e.from || box.id === e.to) continue;
      if (segs.some((s) => segHitsBox(s, box, pad))) fouls.push(`${e.from}->${e.to} through ${box.id}`);
    }
  });
  return fouls;
}

describe('wfRoutes: no trace runs through a card', () => {
  it('DEFECT 2: the direct elbow cut through cards; the router reroutes those and clears them', () => {
    const layout = layoutFor(REFUND_SWEEP_DRAFT);
    const boxes = REFUND_SWEEP_DRAFT.nodes.map((n) => ({ id: n.id, ...layout[n.id] }));
    // the two offenders the operator saw: the cycle back-edge n_fan->n_fetch (the bracket that
    // exited the far-right port and doubled back through n_charge) and n_charge->n_notify (its
    // mid-column elbow struck n_pick). Both foul as a DIRECT elbow...
    const fan = { a: layout['n_fan'], b: layout['n_fetch'] };
    const chg = { a: layout['n_charge'], b: layout['n_notify'] };
    expect(routeHitsForeignNode(fan.a, fan.b, boxes, 'n_fan', 'n_fetch')).toBe(true);
    expect(routeHitsForeignNode(chg.a, chg.b, boxes, 'n_charge', 'n_notify')).toBe(true);
    // ...and are clear once wfRoutes reroutes them through the below-band channel.
    expect(routeFouls(REFUND_SWEEP_DRAFT, layout, 12)).toEqual([]);
  });

  it('a computed layout with a column-skip AND a back-edge routes both clear of every card', () => {
    // a seeded server graph carries no sidecar, so it is drawn by layeredLayout. This shape has a
    // forward edge that skips a column (a->d, past c) and a back-edge (d->b): the two cases the
    // router must reroute, so the assertion bites on a computed layout, not only the draft.
    const seeded: WfGraph = {
      key: 'sha256:seed',
      hash: 'sha256:seed',
      name: 'seed',
      state: 'published',
      nodes: [
        { id: 'a', kind: 'agent', name: 'a' },
        { id: 'b', kind: 'tool', name: 'b' },
        { id: 'c', kind: 'tool', name: 'c' },
        { id: 'd', kind: 'agent', name: 'd' },
      ],
      edges: [
        { from: 'a', to: 'b' },
        { from: 'b', to: 'c' },
        { from: 'c', to: 'd' },
        { from: 'a', to: 'd' }, // skips column c
        { from: 'd', to: 'b' }, // back-edge
      ],
    };
    expect(routeFouls(seeded, layeredLayout(seeded), 12)).toEqual([]);
  });

  it('leaves a clean forward edge on its direct elbow (only fouling traces are rerouted)', () => {
    const layout = layoutFor(REFUND_SWEEP_DRAFT);
    const routes = wfRoutes(REFUND_SWEEP_DRAFT, layout);
    // n_start->n_fetch is a clean same-rank rule; edge index 0. It keeps the straight two-point form.
    expect(routes[0]?.d).toMatch(/^M \S+ \S+ L \S+ \S+$/);
    // the dangling edge (index 4, n_pick->n_notifyy) has an unlaid endpoint, so it is not drawn.
    expect(routes[4]).toBeUndefined();
  });
});

describe('a back-edge routes the designed below-band shape (DEFECT 2)', () => {
  const a: WfBox = { x: 1200, y: 30 }; // source, far right / top
  const b: WfBox = { x: 300, y: 180 }; // target, to the LEFT: a back-edge
  const channelY = 474;

  it('leaves the source LEFT port and never doubles back across its own card', () => {
    const pts = edgePoints(a, b, { channelY });
    // exits at the source's left edge (x = a.x), NOT the right port (a.x + NODE_W) the old elbow used
    expect(pts[0][0]).toBe(a.x);
    // every x on the route is <= the source's left edge: the trace flows leftward toward the
    // target and never crosses back over the source (the old bracket's defect).
    for (const [x] of pts) expect(x).toBeLessThanOrEqual(a.x);
  });

  it('runs its long horizontal in a channel BELOW the whole node band, then rises into the target', () => {
    const pts = edgePoints(a, b, { channelY });
    // the two channel corners sit at channelY, below both cards (max card bottom here is 180+104)
    const ys = pts.map((p) => p[1]);
    expect(Math.max(...ys)).toBe(channelY);
    expect(channelY).toBeGreaterThan(180 + NODE_H);
    // it still enters the target's LEFT port horizontally, so the arrowhead is unchanged
    const last = pts[pts.length - 1];
    expect(last).toEqual([b.x, b.y + NODE_H / 2]);
    expect(wfPath(a, b, { channelY }).arrow).toContain(`L ${b.x} ${b.y + NODE_H / 2}`);
  });

  it('separates parallel reroutes into distinct channel lanes', () => {
    // wfRoutes stacks each rerouted trace a lane deeper, so two never merge into one rule. The
    // computed seed above has two reroutes (a->d skip, d->b back); their channel ys must differ.
    const seeded: WfGraph = {
      key: 'k',
      hash: 'sha256:k',
      name: 'k',
      state: 'published',
      nodes: [
        { id: 'a', kind: 'agent', name: 'a' },
        { id: 'b', kind: 'tool', name: 'b' },
        { id: 'c', kind: 'tool', name: 'c' },
        { id: 'd', kind: 'agent', name: 'd' },
      ],
      edges: [
        { from: 'a', to: 'b' },
        { from: 'b', to: 'c' },
        { from: 'c', to: 'd' },
        { from: 'a', to: 'd' },
        { from: 'd', to: 'b' },
      ],
    };
    const routes = wfRoutes(seeded, layeredLayout(seeded));
    const channelYOf = (d: string): number =>
      Math.max(...(d.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number).filter((_, i) => i % 2 === 1));
    const skipY = channelYOf(routes[3]!.d);
    const backY = channelYOf(routes[4]!.d);
    expect(skipY).not.toBe(backY);
  });
});

describe('DUP_STACK_OFFSET: the intentional stack nudge', () => {
  it('is larger than the prototype 16px, so the under-card title stays readable', () => {
    expect(DUP_STACK_OFFSET).toBeGreaterThan(16);
  });
});

describe('wfFit: the WF_MIN_K legibility floor', () => {
  it('centres the whole graph when it fits above the floor', () => {
    const layout = layeredLayout(chain);
    const view = wfFit(chain, layout, { width: 4000, height: 4000 });
    expect(view.k).toBeGreaterThanOrEqual(WF_MIN_K);
    expect(view.k).toBeLessThanOrEqual(1);
  });

  it('clamps to the floor and anchors the entry node at the left margin when it cannot fit legibly', () => {
    const layout = layeredLayout(chain);
    // A viewport far too small forces k below the floor; fit must clamp UP to WF_MIN_K.
    const view = wfFit(chain, layout, { width: 200, height: 160 });
    expect(view.k).toBeCloseTo(WF_MIN_K, 10);
    // entry node 'a' is at x=0; anchored at the left pad (32) => x = 32 - 0*k = 32
    expect(view.x).toBeCloseTo(32, 6);
  });

  it('WF_MIN_K is 11/15', () => {
    expect(WF_MIN_K).toBeCloseTo(11 / 15, 12);
  });
});

describe('wfZoom: clamps and keeps the cursor point fixed', () => {
  it('keeps the point under the cursor stationary', () => {
    const start = { k: 1, x: 0, y: 0 };
    const cx = 400;
    const cy = 300;
    const graphPointBefore = { x: (cx - start.x) / start.k, y: (cy - start.y) / start.k };
    const zoomed = wfZoom(start, 1.25, cx, cy);
    const graphPointAfter = { x: (cx - zoomed.x) / zoomed.k, y: (cy - zoomed.y) / zoomed.k };
    expect(graphPointAfter.x).toBeCloseTo(graphPointBefore.x, 6);
    expect(graphPointAfter.y).toBeCloseTo(graphPointBefore.y, 6);
  });

  it('clamps to [0.25, 2]', () => {
    expect(wfZoom({ k: 1.9, x: 0, y: 0 }, 4, 0, 0).k).toBe(2);
    expect(wfZoom({ k: 0.3, x: 0, y: 0 }, 0.1, 0, 0).k).toBe(0.25);
  });
});

describe('layoutBounds + zoomPercent', () => {
  it('a bounds width includes the node width', () => {
    const single: WfGraph = { ...chain, nodes: [{ id: 'a', kind: 'agent', name: 'a' }], edges: [] };
    const b = layoutBounds(single, layeredLayout(single));
    expect(b.width).toBe(NODE_W);
    expect(b.height).toBe(NODE_H);
  });

  it('rounds k to a whole percent', () => {
    expect(zoomPercent(WF_MIN_K)).toBe('73%');
    expect(zoomPercent(1)).toBe('100%');
  });
});
