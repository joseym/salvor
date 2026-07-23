import { describe, expect, it } from 'vitest';

import { REFUND_SWEEP_DRAFT } from './wf-draft';
import type { WfGraph } from './wf-model';
import { applyFix, HASH_RE, nearestId, validateGraph, verdictOf } from './wf-validate';

// The server's real form: a full sha256, 64 lowercase hex characters — the same literal the
// e2e suite types into the panel's hash field by hand,
// kept identical here so the two are coherent about what "a well-formed hash" looks like.
const FULL_HASH = `sha256:${'0123456789abcdef'.repeat(4)}`;

const CLEAN: WfGraph = {
  key: 'draft:clean',
  hash: null,
  name: 'clean',
  state: 'draft',
  nodes: [
    { id: 'a', kind: 'agent', name: 'a', agentHash: FULL_HASH },
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

  // sync-ledger item 4: the server's rule is the full 64-hex form, not this app's own old 16-hex
  // short form — a hash that used to pass must now fail, node/edge-precise as ever.
  it('a well-formed-but-short hash (the old 16-hex tolerance) is now malformed', () => {
    expect(HASH_RE.test(FULL_HASH)).toBe(true);
    const g: WfGraph = {
      ...CLEAN,
      nodes: [{ ...CLEAN.nodes[0], agentHash: 'sha256:0123456789abcdef' }, CLEAN.nodes[1]],
    };
    const err = validateGraph(g).find((e) => e.code === 'bad_agent_hash');
    expect(err).toBeDefined();
    // valid hex, just short — completing it is offered
    expect(err?.fix).toMatchObject({ kind: 'complete_hash', id: 'a' });
  });

  // sync-ledger item 4: a `model_decision` case names no agent to decide it — node/case-precise,
  // exactly like the server's own error, with a fix that only ever reuses a hash already in the
  // document (never a fabricated one).
  it('a model_decision case with no branch agent_hash is reported, one error per case', () => {
    const g: WfGraph = {
      ...CLEAN,
      nodes: [
        ...CLEAN.nodes,
        { id: 'br', kind: 'branch', name: 'br', cases: ['x', 'y'], modelCases: ['x', 'y'] },
      ],
      edges: [{ from: 'a', to: 'b' }, { from: 'br', to: 'a', label: 'x' }, { from: 'br', to: 'b', label: 'y' }],
    };
    const errs = validateGraph(g).filter((e) => e.code === 'model_decision_without_agent');
    expect(errs).toHaveLength(2);
    expect(errs.map((e) => e.case).sort()).toEqual(['x', 'y']);
    // node 'a' is the only well-formed agent in the document — the donor the fix reuses
    expect(errs[0].fix).toMatchObject({ kind: 'attach_agent', id: 'br', hash: FULL_HASH });

    const fixed = applyFix(g, errs[0].fix!);
    expect(validateGraph(fixed).some((e) => e.code === 'model_decision_without_agent')).toBe(false);
  });

  it('a model_decision case is unreported once the branch already carries a well-formed agent_hash', () => {
    const g: WfGraph = {
      ...CLEAN,
      nodes: [
        ...CLEAN.nodes,
        { id: 'br', kind: 'branch', name: 'br', cases: ['x'], modelCases: ['x'], agentHash: FULL_HASH },
      ],
      edges: [{ from: 'a', to: 'b' }, { from: 'br', to: 'a', label: 'x' }],
    };
    expect(validateGraph(g).some((e) => e.code === 'model_decision_without_agent')).toBe(false);
  });

  // sync-ledger item 4: the server's node-name bounds (64 characters, non-blank when set).
  it('a display name over the character cap, or blank, is node-precise with a matching fix', () => {
    const long: WfGraph = {
      ...CLEAN,
      nodes: [{ ...CLEAN.nodes[0], name: 'x'.repeat(65) }, CLEAN.nodes[1]],
    };
    const tooLong = validateGraph(long).find((e) => e.code === 'name_too_long');
    expect(tooLong?.fix).toMatchObject({ kind: 'truncate_name', id: 'a' });
    const fixedLong = applyFix(long, tooLong!.fix!);
    expect(validateGraph(fixedLong).some((e) => e.code === 'name_too_long')).toBe(false);

    const blank: WfGraph = {
      ...CLEAN,
      nodes: [{ ...CLEAN.nodes[0], name: '   ' }, CLEAN.nodes[1]],
    };
    const empty = validateGraph(blank).find((e) => e.code === 'name_empty');
    expect(empty?.fix).toMatchObject({ kind: 'clear_name', id: 'a' });
    const fixedBlank = applyFix(blank, empty!.fix!);
    expect(fixedBlank.nodes.find((n) => n.id === 'a')?.name).toBe('a'); // reverts to the id
    expect(validateGraph(fixedBlank).some((e) => e.code === 'name_empty')).toBe(false);
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
      nodes: g.nodes.map((n) => (n.id === 'n_start' ? { ...n, agentHash: FULL_HASH } : n)),
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
