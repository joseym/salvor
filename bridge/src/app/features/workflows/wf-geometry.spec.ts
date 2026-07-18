import { describe, expect, it } from 'vitest';

import {
  NODE_H,
  NODE_W,
  WF_MIN_K,
  layeredLayout,
  layoutBounds,
  wfFit,
  wfTopo,
  wfZoom,
  zoomPercent,
} from './wf-geometry';
import type { WfGraph } from './wf-model';

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
