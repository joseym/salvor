import { SalvorClient } from '@salvor/client';

import {
  budgetFloor,
  budgetKindLabel,
  budgetPropose,
  extendKey,
  fetchAppendedEvent,
  formatBudgetValue,
  parseBudgetInfo,
  reconcileIntentFrom,
  shortId,
  usd,
} from './inbox-model';

describe('parseBudgetInfo', () => {
  it('reads kind/limit/observed off the real server wire shape (status.raw.budget + status.raw.observed)', () => {
    const info = parseBudgetInfo({ budget: { kind: 'steps', limit: 1 }, observed: 1 });
    expect(info).toEqual({ kind: 'steps', limit: 1, observed: 1 });
  });

  it('returns undefined, honestly, for a missing or malformed budget object rather than fabricating one', () => {
    expect(parseBudgetInfo({})).toBeUndefined();
    expect(parseBudgetInfo({ budget: { kind: 'not_a_real_kind', limit: 1 }, observed: 1 })).toBeUndefined();
    expect(parseBudgetInfo({ budget: { kind: 'steps' }, observed: 'nope' })).toBeUndefined();
  });
});

describe('extendKey', () => {
  it('matches crates/salvor-cli/src/render.rs::extend_key exactly, including wall_time_seconds', () => {
    expect(extendKey('steps')).toBe('steps');
    expect(extendKey('tokens')).toBe('tokens');
    expect(extendKey('cost_usd')).toBe('cost_usd');
    expect(extendKey('wall_time')).toBe('wall_time_seconds');
  });
});

describe('budgetKindLabel', () => {
  it('is the dimension name, with wall_time spelled out', () => {
    expect(budgetKindLabel('steps')).toBe('steps');
    expect(budgetKindLabel('wall_time')).toBe('wall time');
  });
});

describe('budget floor and proposal — the honest math', () => {
  it('cost_usd: floor is the max of observed/limit rounded up to the cent', () => {
    const floor = budgetFloor({ kind: 'cost_usd', limit: 0.5, observed: 0.4999 });
    expect(floor).toBeCloseTo(0.5, 5);
  });

  it('a value already exactly on the cent boundary is not pushed up past itself (the -1e-9 epsilon)', () => {
    expect(budgetFloor({ kind: 'cost_usd', limit: 1, observed: 2 })).toBeCloseTo(2, 5);
  });

  it('steps/tokens: floor rounds up to a whole unit — the real seeded fixture is a steps budget', () => {
    expect(budgetFloor({ kind: 'steps', limit: 1, observed: 1 })).toBe(1);
    expect(budgetFloor({ kind: 'tokens', limit: 100, observed: 100.4 })).toBe(101);
  });

  it("proposal is twice the DECLARED LIMIT (this dashboard's own suggestion), never below the floor", () => {
    expect(budgetPropose({ kind: 'steps', limit: 1, observed: 1 })).toBe(2);
    // observed far exceeds 2x limit: the floor wins, not a stale, too-low 2x figure
    expect(budgetPropose({ kind: 'cost_usd', limit: 0.1, observed: 5 })).toBeCloseTo(5, 5);
  });
});

describe('formatBudgetValue', () => {
  it('is dollar-formatted for cost_usd, seconds-suffixed for wall_time, and a plain integer count otherwise', () => {
    expect(formatBudgetValue('cost_usd', 0.5)).toBe(usd(0.5));
    expect(formatBudgetValue('wall_time', 45)).toBe('45s');
    expect(formatBudgetValue('steps', 12000)).toBe('12,000');
  });
});

describe('usd', () => {
  it('uses 4 decimal places under a dollar, 2 at or above', () => {
    expect(usd(0.0273)).toBe('$0.0273');
    expect(usd(1.5)).toBe('$1.50');
  });
});

describe('reconcileIntentFrom', () => {
  it('builds the evidence from a plain pending call + the recorded_at read off its own envelope — never a 409', () => {
    const intent = reconcileIntentFrom(
      {
        kind: 'tool',
        seq: 4,
        tool: 'charge',
        input: { amount: 10 },
        effect: 'write',
        raw: { idempotency_key: null },
      },
      '2026-07-16T00:00:00Z',
    );
    expect(intent).toEqual({
      kind: 'tool',
      seq: 4,
      tool: 'charge',
      input: { amount: 10 },
      effect: 'write',
      idempotencyKey: null,
      recordedAt: '2026-07-16T00:00:00Z',
    });
  });

  it('reads a real idempotency_key off raw, and defaults recordedAt to empty string honestly when the envelope read failed', () => {
    const intent = reconcileIntentFrom(
      { kind: 'tool', seq: 1, tool: 'charge', raw: { idempotency_key: 'key-123' } },
      undefined,
    );
    expect(intent?.idempotencyKey).toBe('key-123');
    expect(intent?.recordedAt).toBe('');
  });

  it('returns undefined for a non-tool pending call — the only shape needs_reconciliation derives from', () => {
    expect(reconcileIntentFrom({ kind: 'model', seq: 1, raw: {} }, '2026-01-01T00:00:00Z')).toBeUndefined();
  });
});

describe('shortId', () => {
  it('truncates to 8 characters', () => {
    expect(shortId('abcdefgh-1234-5678')).toBe('abcdefgh');
  });
});

describe('fetchAppendedEvent', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('reads the first event at fromSeq off the stream and releases the connection', async () => {
    const sse =
      `id: 5\nevent: message\ndata: ${JSON.stringify({
        run_id: 'r1',
        seq: 5,
        recorded_at: '2026-07-16T00:00:01Z',
        event: { kind: 'Resumed', payload: { input: {} } },
      })}\n\n`;
    fetchMock.mockResolvedValueOnce(
      new Response(sse, { status: 200, headers: { 'Content-Type': 'text/event-stream' } }),
    );
    const client = new SalvorClient('http://test.local');

    const event = await fetchAppendedEvent(client, 'r1', 5);

    expect(event?.seq).toBe(5);
    expect(event?.kind).toBe('Resumed');
  });
});
