import { describe, expect, it } from 'vitest';

import { agentIdentity, isHash, toRunRow, type RunRow } from './run-model';
import type { RunSummary } from '@salvor/client';

function row(agentDefHash: string | undefined): RunRow {
  return { id: 'r1', status: 'completed', eventCount: 1, agentDefHash };
}

describe('isHash', () => {
  it('a sha256: value is hash-shaped', () => {
    expect(isHash('sha256:abc123')).toBe(true);
  });
  it('a caller-supplied readable label is not hash-shaped', () => {
    expect(isHash('aarg_jd_parser_v1')).toBe(false);
  });
});

describe('agentIdentity — the three honest renderings', () => {
  it('no agent_def_hash at all: kind "none", an em dash, no hash to show in a title', () => {
    const id = agentIdentity(row(undefined));
    expect(id).toEqual({ text: '—', kind: 'none' });
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
