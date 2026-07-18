import { beforeEach, describe, expect, it, vi } from 'vitest';

import { REFUND_SWEEP_DRAFT, loadDrafts, removeDraft, saveDraft } from './wf-draft';
import type { WfGraph } from './wf-model';

/** An in-memory localStorage stand-in — the vitest node env has none by default. */
function installStorage(): Record<string, string> {
  const store: Record<string, string> = {};
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k in store ? store[k] : null),
    setItem: (k: string, v: string) => {
      store[k] = v;
    },
    removeItem: (k: string) => {
      delete store[k];
    },
  });
  return store;
}

describe('the draft store', () => {
  beforeEach(() => installStorage());

  it('seeds the ported refund-sweep draft into an empty store, once', () => {
    const drafts = loadDrafts();
    expect(drafts.map((d) => d.key)).toContain('draft:refund-sweep');
    const seeded = drafts.find((d) => d.key === 'draft:refund-sweep')!;
    expect(seeded.name).toBe('refund-sweep');
    expect(seeded.hash).toBeNull();
    expect(seeded.state).toBe('draft');
    expect(seeded.nodes[0].id).toBe('n_start');
  });

  it('does not re-seed once the author has saved something (no overwrite)', () => {
    loadDrafts();
    const edited: WfGraph = { ...REFUND_SWEEP_DRAFT, name: 'renamed' };
    saveDraft(edited);
    const after = loadDrafts();
    expect(after.find((d) => d.key === 'draft:refund-sweep')!.name).toBe('renamed');
  });

  it('saveDraft persists a new draft and returns the full list', () => {
    loadDrafts();
    const fresh: WfGraph = {
      key: 'draft:new-thing',
      hash: null,
      name: 'new-thing',
      state: 'draft',
      nodes: [{ id: 'n1', kind: 'tool', name: 'n1' }],
      edges: [],
    };
    const list = saveDraft(fresh);
    expect(list.map((d) => d.key).sort()).toEqual(['draft:new-thing', 'draft:refund-sweep']);
  });

  it('removeDraft drops a draft (as publish would, promoting it to a server graph)', () => {
    loadDrafts();
    const list = removeDraft('draft:refund-sweep');
    expect(list.map((d) => d.key)).not.toContain('draft:refund-sweep');
  });

  it('a broken localStorage never crashes the load', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    });
    expect(() => loadDrafts()).not.toThrow();
  });
});
