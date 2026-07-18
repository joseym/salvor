import { describe, expect, it } from 'vitest';

import { REFUND_SWEEP_DRAFT } from './wf-draft';
import type { WfGraph } from './wf-model';
import { applyFix, nearestId, validateGraph, verdictOf } from './wf-validate';

const CLEAN: WfGraph = {
  key: 'draft:clean',
  hash: null,
  name: 'clean',
  state: 'draft',
  nodes: [
    { id: 'a', kind: 'agent', name: 'a', agentHash: 'sha256:0123456789abcdef' },
    { id: 'b', kind: 'tool', name: 'b', tool: 't', effect: 'read', input: {} },
  ],
  edges: [{ from: 'a', to: 'b' }],
};

describe('validateGraph', () => {
  it('passes a clean graph with the publishable verdict', () => {
    const errs = validateGraph(CLEAN);
    expect(errs).toHaveLength(0);
    expect(verdictOf(errs)).toBe('No errors · this graph can be published');
  });

  it('finds exactly the six seeded defects in the refund-sweep draft, one per class', () => {
    const errs = validateGraph(REFUND_SWEEP_DRAFT);
    expect(errs).toHaveLength(6);
    expect(errs.map((e) => e.code).sort()).toEqual(
      ['bad_agent_hash', 'bad_concurrency', 'cycle', 'dangling_edge', 'duplicate_id', 'edge_type'].sort(),
    );
    expect(verdictOf(errs)).toBe('6 errors — publish is blocked');
  });

  it('the dangling edge suggests the one unambiguous near-miss (n_notifyy → n_notify)', () => {
    const dangling = validateGraph(REFUND_SWEEP_DRAFT).find((e) => e.code === 'dangling_edge');
    expect(dangling?.fix).toMatchObject({ kind: 'repoint', to: 'n_notify' });
  });

  it('nearestId refuses to guess between equally close candidates', () => {
    expect(nearestId('n_x', ['n_a', 'n_b'])).toBeNull();
    expect(nearestId('n_notifyy', ['n_notify', 'n_fetch'])).toBe('n_notify');
    expect(nearestId('completely_off', ['n_notify'])).toBeNull();
  });

  it('a branch edge with an undeclared case, and a caseless branch edge, are both edge_type errors', () => {
    const g: WfGraph = {
      ...CLEAN,
      nodes: [...CLEAN.nodes, { id: 'br', kind: 'branch', name: 'br', cases: ['x'] }],
      edges: [
        { from: 'a', to: 'b' },
        { from: 'br', to: 'a', label: 'zzz' },
        { from: 'br', to: 'b' },
      ],
    };
    const codes = validateGraph(g).map((e) => e.code);
    expect(codes.filter((c) => c === 'edge_type')).toHaveLength(2);
    // the declared case "x" is realized by no edge
    expect(codes).toContain('unrealized_case');
  });
});

describe('applyFix — every one-click offer, applied through the same pure path', () => {
  it('the five offered fixes plus one typed hash repair take refund-sweep to zero errors', () => {
    let g: WfGraph = REFUND_SWEEP_DRAFT;
    // apply first-offered fixes until none remain, as the panel's buttons would
    for (let i = 0; i < 12; i++) {
      const withFix = validateGraph(g).find((e) => e.fix);
      if (!withFix?.fix) break;
      g = applyFix(g, withFix.fix);
    }
    // the malformed agent hash has no one-click fix by design — repair it as the field edit does
    const remaining = validateGraph(g);
    expect(remaining.map((e) => e.code)).toEqual(['bad_agent_hash']);
    g = {
      ...g,
      nodes: g.nodes.map((n) => (n.id === 'n_start' ? { ...n, agentHash: 'sha256:0123456789abcdef' } : n)),
    };
    expect(validateGraph(g)).toHaveLength(0);
  });

  it('rename_dupe renames only the SECOND claimant of the id', () => {
    const fixed = applyFix(REFUND_SWEEP_DRAFT, { label: '', kind: 'rename_dupe', id: 'n_charge' });
    const claimants = fixed.nodes.filter((n) => n.id.startsWith('n_charge')).map((n) => n.id);
    expect(claimants).toEqual(['n_charge', 'n_charge_2']);
  });

  it('drop_edge removes the cycle-closing edge and nothing else', () => {
    const cyc = validateGraph(REFUND_SWEEP_DRAFT).find((e) => e.code === 'cycle');
    expect(cyc?.fix?.kind).toBe('drop_edge');
    const fixed = applyFix(REFUND_SWEEP_DRAFT, cyc!.fix!);
    expect(fixed.edges).toHaveLength(REFUND_SWEEP_DRAFT.edges.length - 1);
    expect(validateGraph(fixed).some((e) => e.code === 'cycle')).toBe(false);
  });
});
