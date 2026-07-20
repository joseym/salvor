import { describe, expect, it } from 'vitest';

import {
  STALL_GRACE_MS,
  agentIdentity,
  derivedStatus,
  groupOf,
  isHash,
  isWaiting,
  labelOf,
  toRunRow,
  type RunRow,
} from './run-model';
import type { RunSummary } from '@salvor/client';

function row(agentDefHash: string | undefined): RunRow {
  return { id: 'r1', status: 'completed', eventCount: 1, agentDefHash };
}
/** A run whose log the server folded (usage/step_count present) — the "graph run" signal when it
 * also carries no agent_def_hash. */
function foldedRow(agentDefHash: string | undefined): RunRow {
  return { id: 'r1', status: 'completed', eventCount: 1, stepCount: 3, agentDefHash };
}

describe('isHash', () => {
  it('a sha256: value is hash-shaped', () => {
    expect(isHash('sha256:abc123')).toBe(true);
  });
  it('a caller-supplied readable label is not hash-shaped', () => {
    expect(isHash('aarg_jd_parser_v1')).toBe(false);
  });
});

describe('agentIdentity — the honest renderings', () => {
  it('no agent_def_hash AND an unfolded log: kind "none", an em dash (nothing recorded to show)', () => {
    const id = agentIdentity(row(undefined));
    expect(id).toEqual({ text: '—', kind: 'none' });
  });

  it('no agent_def_hash but the log DID fold: kind "graph", "graph run" (a GraphRunStarted head)', () => {
    const id = agentIdentity(foldedRow(undefined));
    expect(id).toEqual({ text: 'graph run', kind: 'graph' });
  });

  it('the graph rendering carries no hash — GET /v1/runs carries nothing graph-shaped on the row', () => {
    const id = agentIdentity(foldedRow(undefined));
    expect(id.hash).toBeUndefined();
  });

  it('a folded run WITH an agent_def_hash is still an agent run, never "graph"', () => {
    const id = agentIdentity(foldedRow('sha256:e8a1d362'));
    expect(id.kind).toBe('hash');
  });

  it('a caller-supplied readable label (not hash-shaped): shown as-is, kind "label"', () => {
    const id = agentIdentity(row('aarg_jd_parser_v1'));
    expect(id).toEqual({ text: 'aarg_jd_parser_v1', kind: 'label', hash: 'aarg_jd_parser_v1' });
  });

  it('a hash-shaped agent_def_hash the registry resolved a name for: kind "name", hash rides along', () => {
    const names = new Map([['sha256:e8a1d362', 'support-triage']]);
    const id = agentIdentity(row('sha256:e8a1d362'), names);
    expect(id).toEqual({ text: 'support-triage', kind: 'name', hash: 'sha256:e8a1d362' });
  });

  it('a hash-shaped agent_def_hash with no resolved name (unregistered, or not yet looked up): kind "hash", the hash itself', () => {
    const id = agentIdentity(row('sha256:e8a1d362'));
    expect(id).toEqual({ text: 'sha256:e8a1d362', kind: 'hash', hash: 'sha256:e8a1d362' });
  });

  it('an unrelated resolved name in the map does not leak onto a different hash', () => {
    const names = new Map([['sha256:other', 'billing-reconciler']]);
    const id = agentIdentity(row('sha256:e8a1d362'), names);
    expect(id.kind).toBe('hash');
    expect(id.text).toBe('sha256:e8a1d362');
  });
});

describe('derivedStatus — running + driverless + stale ⟹ stalled (the one derivation rule)', () => {
  const NOW = 1_000_000_000_000;
  const stale = new Date(NOW - STALL_GRACE_MS - 1).toISOString();
  const fresh = new Date(NOW - 1_000).toISOString();

  it('fires only when ALL THREE hold: running, driver none, and last event stale', () => {
    expect(derivedStatus('running', 'none', stale, NOW)).toBe('stalled');
  });

  it('an ATTACHED driver is never stalled, however old the last event', () => {
    expect(derivedStatus('running', 'attached', stale, NOW)).toBe('running');
  });

  it('within the grace period a driverless running run is NOT yet stalled (the just-opened case)', () => {
    expect(derivedStatus('running', 'none', fresh, NOW)).toBe('running');
  });

  it('an ABSENT driver field is not evidence of a stall — never derived as one (honesty over a guess)', () => {
    expect(derivedStatus('running', undefined, stale, NOW)).toBe('running');
  });

  it('only a RUNNING fold can stall — a resting wait or terminal state passes through unchanged', () => {
    for (const s of ['suspended', 'budget_exceeded', 'needs_reconciliation', 'awaiting_tool', 'completed', 'failed']) {
      expect(derivedStatus(s, 'none', stale, NOW)).toBe(s);
    }
  });

  it('a running run with no last event at all reads as infinitely stale, so driverless ⟹ stalled', () => {
    expect(derivedStatus('running', 'none', undefined, NOW)).toBe('stalled');
  });

  it('stalled is in the WAITING group and labelled "stalled" — it sorts with waiting-on-you', () => {
    expect(groupOf('stalled')).toBe('waiting');
    expect(isWaiting('stalled')).toBe(true);
    expect(labelOf('stalled')).toBe('stalled');
  });
});

describe('toRunRow — bakes the derived status and carries driver evidence', () => {
  const NOW = 1_000_000_000_000;
  const stale = new Date(NOW - STALL_GRACE_MS - 1).toISOString();

  it('a running run the server reports driverless-and-stale becomes stalled on the row', () => {
    const r = toRunRow(
      { run: 'r1', status: { state: 'running', raw: {} }, eventCount: 4, lastRecordedAt: stale, driver: 'none', raw: {} },
      NOW,
    );
    expect(r.status).toBe('stalled');
    expect(r.driver).toBe('none');
  });

  it('the same run WITH an attached driver stays running', () => {
    const r = toRunRow(
      { run: 'r1', status: { state: 'running', raw: {} }, eventCount: 4, lastRecordedAt: stale, driver: 'attached', raw: {} },
      NOW,
    );
    expect(r.status).toBe('running');
    expect(r.driver).toBe('attached');
  });
});

describe('toRunRow — labels pass through honestly (absent, never a fabricated {})', () => {
  function summary(over: Partial<RunSummary>): RunSummary {
    return {
      run: 'r1',
      status: { state: 'completed', raw: {} },
      eventCount: 3,
      raw: {},
      ...over,
    };
  }

  it('a run with no labels recorded carries none through', () => {
    const r = toRunRow(summary({}));
    expect(r.labels).toBeUndefined();
  });

  it('a run with labels carries them through flat', () => {
    const r = toRunRow(summary({ labels: { build_id: 'bld_7f3a2c', jd_id: 'jd_2291' } }));
    expect(r.labels).toEqual({ build_id: 'bld_7f3a2c', jd_id: 'jd_2291' });
  });

  it('agentDefHash carries through unchanged, hash or label alike', () => {
    expect(toRunRow(summary({ agentDefHash: 'sha256:abc' })).agentDefHash).toBe('sha256:abc');
    expect(toRunRow(summary({ agentDefHash: 'aarg_cover_writer_v1' })).agentDefHash).toBe(
      'aarg_cover_writer_v1',
    );
  });
});
