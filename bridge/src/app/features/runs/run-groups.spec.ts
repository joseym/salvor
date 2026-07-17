import { describe, expect, it } from 'vitest';

import { attentionRank, buildRunGroups, canCollapse, clusterOf } from './run-groups';
import type { RunRow } from './run-model';

function row(over: Partial<RunRow> & { id: string; status: string }): RunRow {
  return { eventCount: 1, last: '2026-07-12T09:00:00Z', ...over };
}

describe('clusterOf — key priority: build_id label, then agent identity, then Unlabelled', () => {
  it('a run tagged with labels.build_id clusters by build, named after the build id', () => {
    const r = row({ id: 'r1', status: 'completed', agentDefHash: 'sha256:abc', labels: { build_id: 'bld_7f3a2c' } });
    expect(clusterOf(r)).toEqual({ key: 'build:bld_7f3a2c', kind: 'build', name: 'bld_7f3a2c' });
  });

  it('build_id wins even when the run also has an agent hash', () => {
    const r = row({ id: 'r2', status: 'running', agentDefHash: 'sha256:def', labels: { build_id: 'bld_x', jd_id: 'jd_1' } });
    expect(clusterOf(r).kind).toBe('build');
  });

  it('no build label, but an agent hash: clusters by agent identity (registered name, when resolved)', () => {
    const r = row({ id: 'r3', status: 'completed', agentDefHash: 'sha256:triage' });
    const names = new Map([['sha256:triage', 'support-triage']]);
    expect(clusterOf(r, names)).toEqual({ key: 'agent:sha256:triage', kind: 'agent', name: 'support-triage' });
  });

  it('no build label, unregistered agent hash: clusters by the hash itself, honestly', () => {
    const r = row({ id: 'r4', status: 'completed', agentDefHash: 'sha256:unknown' });
    expect(clusterOf(r)).toEqual({ key: 'agent:sha256:unknown', kind: 'agent', name: 'sha256:unknown' });
  });

  it('a caller-supplied readable agent label (not hash-shaped) clusters by that label as-is', () => {
    const r = row({ id: 'r5', status: 'completed', agentDefHash: 'aarg_jd_parser_v1' });
    expect(clusterOf(r)).toEqual({ key: 'agent:aarg_jd_parser_v1', kind: 'agent', name: 'aarg_jd_parser_v1' });
  });

  it('neither a build label nor an agent hash: the literal Unlabelled bucket — never guessed into a build', () => {
    const r = row({ id: 'r6', status: 'completed' });
    expect(clusterOf(r)).toEqual({ key: 'unlabelled', kind: 'unlabelled', name: 'Unlabelled' });
  });
});

describe('attentionRank — the same worst-first scale the flat sort uses', () => {
  it('waiting > failed > everything else', () => {
    expect(attentionRank('suspended')).toBe(2);
    expect(attentionRank('budget_exceeded')).toBe(2);
    expect(attentionRank('needs_reconciliation')).toBe(2);
    expect(attentionRank('failed')).toBe(1);
    expect(attentionRank('completed')).toBe(0);
    expect(attentionRank('running')).toBe(0);
  });
});

describe('buildRunGroups — plural clustering, worst-first, attention survives grouping', () => {
  const rows: RunRow[] = [
    row({ id: 'a1', status: 'completed', agentDefHash: 'aarg_jd_parser_v1', labels: { build_id: 'bld_1' }, last: '2026-07-12T08:10:00Z' }),
    row({ id: 'a2', status: 'running', agentDefHash: 'aarg_durable_retailor_v1', labels: { build_id: 'bld_1' }, last: '2026-07-12T08:11:00Z' }),
    row({ id: 'b1', status: 'completed', agentDefHash: 'aarg_jd_parser_v1', labels: { build_id: 'bld_2' }, last: '2026-07-12T08:30:00Z' }),
    row({ id: 'w1', status: 'suspended', agentDefHash: 'sha256:triage', last: '2026-07-12T09:00:00Z' }),
    row({ id: 'u1', status: 'completed', last: '2026-07-12T07:00:00Z' }),
  ];

  it('produces one group per distinct key — plural, not degenerate', () => {
    const groups = buildRunGroups(rows);
    expect(groups.map((g) => g.key).sort()).toEqual(
      ['agent:sha256:triage', 'build:bld_1', 'build:bld_2', 'unlabelled'].sort(),
    );
  });

  it('a group holding a waiting run floats to the top, exactly as a waiting run would flat', () => {
    const groups = buildRunGroups(rows);
    expect(groups[0].key).toBe('agent:sha256:triage');
    expect(groups[0].waiting).toBe(1);
    expect(groups[0].rank).toBe(2);
  });

  it('non-waiting groups are ordered by most-recent activity after rank', () => {
    const groups = buildRunGroups(rows);
    const rest = groups.slice(1).map((g) => g.key);
    expect(rest).toEqual(['build:bld_2', 'build:bld_1', 'unlabelled']);
  });

  it('each group carries its run count and the distinct states present', () => {
    const groups = buildRunGroups(rows);
    const bld1 = groups.find((g) => g.key === 'build:bld_1')!;
    expect(bld1.runs).toHaveLength(2);
    expect([...bld1.states].sort()).toEqual(['completed', 'running']);
  });

  it('canCollapse is false for any group with a waiting run, true otherwise', () => {
    const groups = buildRunGroups(rows);
    for (const g of groups) {
      expect(canCollapse(g)).toBe(g.waiting === 0);
    }
    expect(canCollapse(groups.find((g) => g.key === 'agent:sha256:triage')!)).toBe(false);
    expect(canCollapse(groups.find((g) => g.key === 'build:bld_1')!)).toBe(true);
  });

  it('an empty visible list produces no groups', () => {
    expect(buildRunGroups([])).toEqual([]);
  });
});
