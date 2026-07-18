import { describe, expect, it } from 'vitest';

import {
  LAYOUTS,
  NODE_H,
  NODE_W,
  WF_MIN_K,
  type WfLayout,
  layeredLayout,
  layoutBounds,
  layoutFor,
  wfFit,
  wfPath,
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

describe('layeredLayout — columns follow the edges, rightward', () => {
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

describe('layoutFor — hand-authored sidecar vs computed, and non-overlap', () => {
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
    // feeds under the computed longest-path layout — the scrambled drawing the operator reported.
    const computed = layeredLayout(REFUND_SWEEP_DRAFT);
    expect(computed['n_fetch'].x).toBeGreaterThan(computed['n_charge'].x);
    // the ported sidecar restores the honest spine: fetch sits BEFORE charge.
    const sidecar = layoutFor(REFUND_SWEEP_DRAFT);
    expect(sidecar['n_fetch'].x).toBeLessThan(sidecar['n_charge'].x);
  });

  it('a laid-out graph never overlaps two DISTINCT nodes — draft and computed alike', () => {
    expect(noDistinctBoxesOverlap(layoutFor(REFUND_SWEEP_DRAFT))).toBe(true);
    expect(noDistinctBoxesOverlap(layeredLayout(chain))).toBe(true);
  });
});

describe('wfPath — orthogonal elbows with one fine arrowhead', () => {
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

describe('wfFit — the WF_MIN_K legibility floor', () => {
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

describe('wfZoom — clamps and keeps the cursor point fixed', () => {
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
