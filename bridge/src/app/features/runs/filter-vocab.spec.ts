import { describe, expect, it } from 'vitest';

import {
  COLON_KEYS,
  FILTER_VOCAB,
  FIELDS,
  MENU_KEYS,
  QUERYABLE,
  matchesAll,
  parseQuery,
  vocabDrift,
} from './filter-vocab';
import type { RunRow } from './run-model';

/**
 * The filter-vocabulary bijection, checked as a real unit test — VOCAB_DRIFT's job, moved out of
 * the prototype's boot-time console probe (which evaporates when the run ends) into the suite.
 * This pins "FILTER_VOCAB as the single source with the bijection as a
 * vitest unit test".
 */
describe('filter-vocabulary bijection', () => {
  it('has no drift: every offered key parses, and no unlisted key is accepted', () => {
    expect(vocabDrift()).toEqual([]);
  });

  it('direction 1 — every @-menu key parses to a structured (non-text) term', () => {
    for (const v of MENU_KEYS) {
      const probe = v.key + v.op + (v.op === ':' ? 'x' : '1');
      const terms = parseQuery(probe);
      expect(terms, `offered ${probe} must parse to one term`).toHaveLength(1);
      expect(terms[0].f, `offered ${probe} must not fall through to a bare word`).not.toBe('text');
      expect(terms[0].f).toBe(v.key);
    }
  });

  it('direction 2 — the historically-refused fields stay refused by the parser', () => {
    for (const key of ['agent', 'cost', 'steps', 'tokens']) {
      expect(FILTER_VOCAB.some((v) => v.key === key)).toBe(false);
      expect(() => parseQuery(`${key}:1`)).toThrow();
    }
  });

  it('COLON_KEYS is exactly the deduped set of `:`-operator keys in the vocab', () => {
    expect([...COLON_KEYS].sort()).toEqual(['group', 'hour', 'status', 'text']);
  });

  it('hour: is an offered, enumerable key (it joined the vocabulary)', () => {
    const hour = FILTER_VOCAB.find((v) => v.key === 'hour');
    expect(hour).toBeDefined();
    expect(hour?.offered).toBe(true);
    expect(hour?.enumerable).toBe(true);
    expect(MENU_KEYS.map((v) => v.key)).toContain('hour');
  });

  it('a bare word parses to a text term (matches the run id)', () => {
    const terms = parseQuery('9f3a');
    expect(terms).toEqual([{ f: 'text', op: ':', v: '9f3a' }]);
  });

  it('refusal messages name what is queryable', () => {
    expect(() => parseQuery('cost:5')).toThrow(/Queryable:/);
    expect(QUERYABLE).toContain('status:');
    expect(QUERYABLE).toContain('hour:');
  });
});

describe('matchesAll over the field readers', () => {
  const rows: RunRow[] = [
    { id: 'aaaa1111', status: 'completed', eventCount: 40, last: '2026-07-12T09:41:00Z' },
    { id: 'bbbb2222', status: 'budget_exceeded', eventCount: 3, last: '2026-07-12T08:12:00Z' },
    { id: 'cccc3333', status: 'running', eventCount: 12, last: '2026-07-12T09:05:00Z' },
  ];

  it('status: matches by folded state', () => {
    expect(rows.filter((r) => matchesAll(r, parseQuery('status:completed'))).map((r) => r.id)).toEqual([
      'aaaa1111',
    ]);
  });

  it('group: classifies status through the same map every dot reads', () => {
    expect(rows.filter((r) => matchesAll(r, parseQuery('group:waiting'))).map((r) => r.id)).toEqual([
      'bbbb2222',
    ]);
  });

  it('events> and events< compare the recorded count', () => {
    expect(rows.filter((r) => matchesAll(r, parseQuery('events>10'))).map((r) => r.id)).toEqual([
      'aaaa1111',
      'cccc3333',
    ]);
    expect(rows.filter((r) => matchesAll(r, parseQuery('events<10'))).map((r) => r.id)).toEqual([
      'bbbb2222',
    ]);
  });

  it('hour: truncates last-active to the UTC hour', () => {
    expect(rows.filter((r) => matchesAll(r, parseQuery('hour:2026-07-12T09:00Z'))).map((r) => r.id)).toEqual([
      'aaaa1111',
      'cccc3333',
    ]);
  });

  it('a bare word matches the run id by substring', () => {
    expect(rows.filter((r) => matchesAll(r, parseQuery('bbbb'))).map((r) => r.id)).toEqual(['bbbb2222']);
  });

  it('FIELDS.group reads through groupOf', () => {
    expect(FIELDS['group'](rows[1])).toBe('waiting');
  });
});
