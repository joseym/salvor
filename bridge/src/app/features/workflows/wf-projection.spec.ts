import { describe, expect, it } from 'vitest';

import type { GraphProjection } from '../../core/api';
import { edgeWalked, projectNodeStates, projectionUsable } from './wf-projection';
import type { WfGraph } from './wf-model';

const graph: WfGraph = {
  key: 'sha256:g',
  hash: 'sha256:g',
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

function raw(): Record<string, unknown> {
  return {};
}

const projection: GraphProjection = {
  graphHash: 'sha256:g',
  currentNode: 'c',
  nodes: [
    { node: 'a', state: 'exited', raw: raw() },
    { node: 'b', state: 'exited', branchCase: 'yes', raw: raw() },
    { node: 'c', state: 'entered', raw: raw() },
    { node: 'd', state: 'skipped', raw: raw() },
  ],
  raw: raw(),
};

describe('projectNodeStates', () => {
  it('maps exited / current / skipped / not-reached honestly', () => {
    const states = projectNodeStates(graph, projection);
    expect(states['a'].state).toBe('exited');
    expect(states['b'].state).toBe('exited');
    expect(states['c'].state).toBe('current');
    expect(states['d'].state).toBe('skipped');
  });

  it('a node absent from the projection is not-reached (never conflated with skipped)', () => {
    const partial: GraphProjection = { ...projection, currentNode: 'b', nodes: [{ node: 'a', state: 'exited', raw: raw() }, { node: 'b', state: 'entered', raw: raw() }] };
    const states = projectNodeStates(graph, partial);
    expect(states['c'].state).toBe('not-reached');
    expect(states['d'].state).toBe('not-reached');
  });

  it('carries the recorded branch case through', () => {
    expect(projectNodeStates(graph, projection)['b'].branchCase).toBe('yes');
  });

  it('an entered non-current node reads as reached', () => {
    const p: GraphProjection = { ...projection, currentNode: 'd', nodes: [{ node: 'c', state: 'entered', raw: raw() }, { node: 'd', state: 'entered', raw: raw() }] };
    expect(projectNodeStates(graph, p)['c'].state).toBe('reached');
  });
});

describe('edgeWalked — a branch inks only the arm the run took', () => {
  it('inks the taken branch arm and not the other', () => {
    const states = projectNodeStates(graph, projection);
    expect(edgeWalked('b', 'c', 'yes', true, states)).toBe(true);
    expect(edgeWalked('b', 'd', 'no', true, states)).toBe(false);
  });

  it('does not ink an ordinary edge whose source was never reached', () => {
    const states = projectNodeStates(graph, { ...projection, nodes: [{ node: 'a', state: 'exited', raw: raw() }], currentNode: undefined });
    expect(edgeWalked('b', 'c', 'yes', true, states)).toBe(false);
  });
});

describe('projectionUsable', () => {
  it('true only when the projection names the shown graph', () => {
    expect(projectionUsable(graph, projection)).toBe(true);
    expect(projectionUsable(graph, { ...projection, graphHash: 'sha256:other' })).toBe(false);
    expect(projectionUsable(graph, undefined)).toBe(false);
  });
});
