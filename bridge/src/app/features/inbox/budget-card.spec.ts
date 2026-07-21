import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor/client';
import type { RunSummary } from '@salvor/client';

import { SALVOR_CLIENT } from '../../core/api';
import { BudgetCard } from './budget-card';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

/* Mirrors the real seeded fixture (bridge/e2e-serve.sh's tiny-budget agent): a STEPS budget, not
 * cost_usd — the honest-per-dimension divergence from the prototype's USD-only BudgetCard. */
const BUDGET_ROW: RunSummary = {
  run: 'run-budget-1',
  status: {
    state: 'budget_exceeded',
    raw: { state: 'budget_exceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 },
  },
  eventCount: 3,
  usage: { inputTokens: 120, outputTokens: 30 },
  raw: {},
};

describe('BudgetCard', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    TestBed.configureTestingModule({
      providers: [{ provide: SALVOR_CLIENT, useValue: new SalvorClient('http://test.local') }],
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('renders the honest floor/propose figures for a STEPS budget (plain counts, never a dollar sign)', () => {
    const fixture = TestBed.createComponent(BudgetCard);
    fixture.componentRef.setInput('row', BUDGET_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    expect(el.querySelector('form[data-raise]')).toBeTruthy();
    const input = el.querySelector('input[type="number"]') as HTMLInputElement;
    expect(input.min).toBe('1');
    expect(input.value).toBe('2'); // propose = max(floor, limit*2) = max(1, 2) = 2
    expect(el.textContent).not.toContain('$');
    expect(el.textContent).toContain("this dashboard's suggestion");
  });

  it('refuses a ceiling that does not exceed the observed spend, with an honest error', () => {
    const fixture = TestBed.createComponent(BudgetCard);
    fixture.componentRef.setInput('row', BUDGET_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    const input = el.querySelector('input[type="number"]') as HTMLInputElement;
    input.value = '1';
    input.dispatchEvent(new Event('input'));
    fixture.detectChanges();
    const form = el.querySelector('form[data-raise]') as HTMLFormElement;
    form.dispatchEvent(new Event('submit', { cancelable: true }));
    fixture.detectChanges();

    expect(el.querySelector('.f-err')?.textContent).toContain('must exceed');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('resumes with {extend:{steps: v}} — the real extend-key mapping, not a USD-only one', async () => {
    const fixture = TestBed.createComponent(BudgetCard);
    fixture.componentRef.setInput('row', BUDGET_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    fetchMock
      .mockResolvedValueOnce(jsonResponse({ run: BUDGET_ROW.run, outcome: 'driving', status: 'running' }))
      .mockResolvedValueOnce(jsonResponse({ run: BUDGET_ROW.run, status: { state: 'running' }, event_count: 4 }))
      .mockResolvedValueOnce(
        new Response(
          `data: ${JSON.stringify({
            run_id: BUDGET_ROW.run,
            seq: 3,
            recorded_at: '2026-07-16T00:00:04Z',
            event: { kind: 'Resumed', payload: { input: { extend: { steps: 2 } } } },
          })}\n\n`,
          { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
        ),
      )
      .mockResolvedValueOnce(jsonResponse({ run: BUDGET_ROW.run, status: { state: 'running' }, event_count: 4 }));

    const form = el.querySelector('form[data-raise]') as HTMLFormElement;
    form.dispatchEvent(new Event('submit', { cancelable: true }));
    await fixture.whenStable();
    for (let i = 0; i < 5; i++) await Promise.resolve();
    fixture.detectChanges();

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({ input: { extend: { steps: 2 } } });
    expect(el.querySelector('form.committed')).toBeTruthy();
  });

  it(
    "THE HUSK'S EXIT: the secondary abandon receipt is PINNED against budget() reclassifying " +
      'after a simulated post-commit refresh, and only folds once dismissed',
    async () => {
      const fixture = TestBed.createComponent(BudgetCard);
      fixture.componentRef.setInput('row', BUDGET_ROW);
      fixture.detectChanges();
      const el = fixture.nativeElement as HTMLElement;

      let retired = 0;
      fixture.componentInstance.retire.subscribe(() => retired++);

      // open the confirm, check the box
      (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
      fixture.detectChanges();
      (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
      fixture.detectChanges();

      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          run: BUDGET_ROW.run,
          abandoned: true,
          appended_seq: 9,
          status: { state: 'abandoned' },
        }),
      );

      (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
      for (let i = 0; i < 10; i++) await Promise.resolve();
      await new Promise((resolve) => setTimeout(resolve, 0));
      for (let i = 0; i < 10; i++) await Promise.resolve();
      fixture.detectChanges();

      expect(el.querySelector('[data-abandon-receipt]'), 'the abandon receipt lands').toBeTruthy();

      // Simulate the parent's `onCommitted` -> `RunsService.refresh()`: the same run id, now
      // reclassified away from `budget_exceeded` with no readable budget object — exactly the
      // refresh that used to make `budget()` go undefined and tear the abandon-action (and its
      // still-unread receipt) out from under the operator, before Done was ever reachable.
      fixture.componentRef.setInput('row', {
        ...BUDGET_ROW,
        status: { state: 'abandoned', raw: { state: 'abandoned' } },
      });
      fixture.detectChanges();

      expect(el.querySelector('form[data-raise]'), 'the card survives the reclassifying refresh').toBeTruthy();
      expect(
        el.querySelector('[data-abandon-receipt]'),
        'PINNED: the receipt survives the reclassifying refresh',
      ).toBeTruthy();
      expect(el.querySelector('[data-abandon-receipt]')?.textContent).toContain('Appended RunAbandoned at seq 9');

      // "Done": the receipt is untouched, but this card's job is done — it reports retire upward
      // and leaves the fold-away animation and actual removal to the parent (inbox.ts).
      const done = el.querySelector('[data-abandon-done]') as HTMLButtonElement;
      done.click();
      fixture.detectChanges();
      expect(retired).toBe(1);
      expect(el.querySelector('[data-abandon-receipt]'), 'still rendered — Done does not itself remove it').toBeTruthy();
    },
  );
});
