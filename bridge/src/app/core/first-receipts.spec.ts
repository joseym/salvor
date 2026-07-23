import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { provideSalvorApi } from './api';
import {
  FirstReceiptsService,
  STEPS,
  TOTAL_STEPS,
  filteredByState,
  inboxReceiptDetail,
  inspectorVisited,
  readState,
  scrubbedEarlier,
} from './first-receipts';
import { routes } from '../app.routes';

describe('first-receipts predicates', () => {
  it('inspectorVisited is true only for the Inspector over a real run', () => {
    expect(inspectorVisited('inspector', 'r-1')).toBe(true);
    expect(inspectorVisited('inspector', undefined)).toBe(false);
    expect(inspectorVisited('runs', 'r-1')).toBe(false);
  });

  it('scrubbedEarlier is true only when the playhead sits before the end of a non-empty log', () => {
    expect(scrubbedEarlier(3, 8)).toBe(true);
    expect(scrubbedEarlier(8, 8)).toBe(false); // at the head — following live, not scrubbed back
    expect(scrubbedEarlier(0, 0)).toBe(false); // empty log
  });

  it('filteredByState matches a state or group filter term, and nothing else', () => {
    expect(filteredByState('status:suspended')).toBe(true);
    expect(filteredByState('group:waiting')).toBe(true);
    expect(filteredByState('  events>3 group:terminal')).toBe(true);
    expect(filteredByState('agent:invoicer')).toBe(false);
    expect(filteredByState('')).toBe(false);
    expect(filteredByState('mystatus:x')).toBe(false); // not a bare token boundary
  });

  it('inboxReceiptDetail reads the verb + run from a card announce, in receipt voice', () => {
    expect(inboxReceiptDetail('Resumed run 88f2abcd. Appended at sequence 12. Status now running.')).toBe(
      'you resumed run 88f2abcd',
    );
    expect(inboxReceiptDetail('Recorded the outcome for run 33ce. Appended at sequence 4.')).toBe(
      'you recorded the outcome for run 33ce',
    );
    expect(inboxReceiptDetail('')).toBe('you cleared a run that was waiting on you');
  });
});

function stubStorage(): void {
  const store = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, String(v)),
    removeItem: (k: string) => void store.delete(k),
    clear: () => store.clear(),
    key: (i: number) => [...store.keys()][i] ?? null,
    get length() {
      return store.size;
    },
  } as Storage);
}

describe('first-receipts persistence', () => {
  // A stubbed in-memory storage: deterministic in every environment, incl.
  // Node runners with no localStorage global at all.
  beforeEach(() => stubStorage());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('a fresh profile (no stored key) defaults to the collapsed dock with no ticks', () => {
    const state = readState();
    expect(state.open).toBe(false);
    expect(state.steps).toEqual({});
  });

  it('a stored choice is honoured, and only known step ids survive', () => {
    localStorage.setItem(
      'salvor.firstReceipts',
      JSON.stringify({ open: true, steps: { inspect: { at: '14:32', detail: 'you opened run abcd’s log' }, bogus: { at: '00:00', detail: 'x' } } }),
    );
    const state = readState();
    expect(state.open).toBe(true);
    expect(state.steps['inspect']).toEqual({ at: '14:32', detail: 'you opened run abcd’s log' });
    expect(state.steps['bogus']).toBeUndefined();
  });

  it('READ FAILURE defaults to SHOWN/teaching — an expanded dock, never suppression', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('storage unavailable');
      },
    } as unknown as Storage);
    const state = readState();
    expect(state.open).toBe(true);
    expect(state.steps).toEqual({});
  });

  it('a malformed-but-readable value degrades to the teaching fallback rather than crashing', () => {
    localStorage.setItem('salvor.firstReceipts', '{not json');
    expect(() => readState()).not.toThrow();
    expect(readState().open).toBe(true);
  });
});

describe('FirstReceiptsService — ticks', () => {
  beforeEach(() => {
    stubStorage();
    TestBed.configureTestingModule({
      providers: [provideRouter(routes), provideSalvorApi({ baseUrl: 'http://test.local' })],
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it('tick is first-write-wins — a second trigger never rewrites the recorded time (silent pre-ticking)', () => {
    const svc = TestBed.inject(FirstReceiptsService);
    svc.tick('inbox', 'you resumed run abcd');
    const first = svc.receiptOf('inbox');
    expect(first?.detail).toBe('you resumed run abcd');
    svc.tick('inbox', 'you resumed run abcd AGAIN');
    expect(svc.receiptOf('inbox')).toEqual(first); // unchanged — the first occurrence is the truth
    expect(svc.count()).toBe(1);
  });

  it('a pre-existing tick loaded from storage is honoured and counts toward completion', () => {
    localStorage.setItem(
      'salvor.firstReceipts',
      JSON.stringify({ open: false, steps: { scrub: { at: '09:01', detail: 'you scrubbed back to an earlier point' } } }),
    );
    const svc = TestBed.inject(FirstReceiptsService);
    expect(svc.isTicked('scrub')).toBe(true);
    expect(svc.activeStepId()).toBe('inspect'); // front-most UNticked step
    expect(svc.complete()).toBe(false);
  });

  it('every step id in STEPS has label, why, coda and a real target selector', () => {
    expect(STEPS).toHaveLength(TOTAL_STEPS);
    for (const s of STEPS) {
      expect(s.label.length, `${s.id} label`).toBeGreaterThan(0);
      expect(s.why.length, `${s.id} why`).toBeGreaterThan(0);
      expect(s.coda.length, `${s.id} coda`).toBeGreaterThan(0);
      expect(s.target, `${s.id} target`).toMatch(/^[#.\[]/);
    }
  });

  it('reopen and dismiss drive the open signal and persist', () => {
    const svc = TestBed.inject(FirstReceiptsService);
    svc.reopen();
    expect(svc.open()).toBe(true);
    expect(JSON.parse(localStorage.getItem('salvor.firstReceipts')!).open).toBe(true);
    svc.dismiss();
    expect(svc.open()).toBe(false);
    expect(JSON.parse(localStorage.getItem('salvor.firstReceipts')!).open).toBe(false);
  });
});
