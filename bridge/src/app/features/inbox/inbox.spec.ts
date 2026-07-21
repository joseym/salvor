import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor/client';

import { SALVOR_CLIENT } from '../../core/api';
import { Inbox } from './inbox';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

describe('Inbox', () => {
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

  /**
   * Poll (bounded, real-timer via `vi.waitFor`) until `condition` is true, running change
   * detection on every attempt. The mocked `fetch()`'s promise chain — fetch resolution, the
   * real `Response` body read, `JSON.parse`, the signal write, the zoneless render — takes a
   * number of microtask hops that is NOT a fixed constant: it depends on the Node/undici/Angular
   * versions in play and, empirically, grows under CPU contention (many worker threads/files
   * competing for the event loop lengthen exactly the internal stream-buffering hops that decide
   * how many `.then()`s the chain needs). A fixed `for (let i = 0; i < N; i++) await
   * Promise.resolve()` loop bakes in a guess at that hop count; under load the guess undercounts
   * and the assertion below sees stale (pre-fetch) DOM. Waiting on the actual condition — never a
   * tick count — is the only version of this that is true regardless of how many hops the chain
   * happens to need on a given run.
   */
  async function settle(
    fixture: { whenStable(): Promise<void>; detectChanges(): void },
    condition: () => boolean,
  ): Promise<void> {
    await fixture.whenStable();
    await vi.waitFor(() => {
      fixture.detectChanges();
      if (!condition()) throw new Error('condition not yet true');
    });
  }

  async function mount() {
    const fixture = TestBed.createComponent(Inbox);
    fixture.detectChanges();
    await settle(fixture, () => fixture.componentInstance.listLoaded());
    return fixture;
  }

  it('renders the all-clear state when GET /v1/runs carries no waiting run', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ runs: [{ run: 'r1', status: { state: 'completed' }, event_count: 3 }] }));
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;

    expect(el.querySelector('.state.all-clear')).toBeTruthy();
    expect(el.querySelector('#inbox-cards form')).toBeNull();
    expect(el.textContent).toContain('All clear');
  });

  it('routes each waiting run to its matching card kind, inside #inbox-cards', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        runs: [
          { run: 'r-susp', status: { state: 'suspended', reason: 'x', input_schema: { properties: {} } }, event_count: 1 },
          { run: 'r-budget', status: { state: 'budget_exceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 }, event_count: 1 },
          { run: 'r-done', status: { state: 'completed' }, event_count: 5 },
        ],
      }),
    );
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;
    const cards = el.querySelector('#inbox-cards') as HTMLElement;

    expect(cards.querySelectorAll('form').length).toBe(2); // the completed run is not a card
    expect(cards.querySelector('form[data-resume]')).toBeTruthy();
    expect(cards.querySelector('form[data-raise]')).toBeTruthy();
    expect(cards.querySelector('.state.all-clear')).toBeNull();
  });

  it('the lede states the real snake_case statuses, not the fixture-era hyphenated slugs', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ runs: [] }));
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('.lede')?.textContent).toContain('needs_reconciliation');
    expect(el.querySelector('.lede')?.textContent).not.toContain('needs-reconciliation');
  });

  it('a committed receipt SURVIVES the post-commit refresh (card permanence: the receipt is replaced in place, never re-rendered away)', async () => {
    const budgetRun = (state: unknown, eventCount: number) => ({
      run: 'r-budget',
      status: state,
      event_count: eventCount,
    });
    // initial load: one budget_exceeded run
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        runs: [budgetRun({ state: 'budget_exceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 }, 3)],
      }),
    );
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('form[data-raise]')).toBeTruthy();

    fetchMock
      // client.resume()
      .mockResolvedValueOnce(jsonResponse({ run: 'r-budget', outcome: 'driving', status: 'running' }))
      // RunDetailService.resume's internal re-fetch of the run
      .mockResolvedValueOnce(jsonResponse(budgetRun({ state: 'running' }, 4)))
      // buildReceipt: the appended event via SSE
      .mockResolvedValueOnce(
        new Response(
          `data: ${JSON.stringify({
            run_id: 'r-budget',
            seq: 3,
            recorded_at: '2026-07-16T00:00:05Z',
            event: { kind: 'Resumed', payload: { input: { extend: { steps: 2 } } } },
          })}\n\n`,
          { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
        ),
      )
      // buildReceipt: the fresh getRun
      .mockResolvedValueOnce(jsonResponse(budgetRun({ state: 'running' }, 4)))
      // onCommitted -> RunsService.refresh: the run is NO LONGER waiting and carries NO budget
      .mockResolvedValueOnce(jsonResponse({ runs: [budgetRun({ state: 'running' }, 4)] }));

    const form = el.querySelector('form[data-raise]') as HTMLFormElement;
    form.dispatchEvent(new Event('submit', { cancelable: true }));
    await settle(fixture, () => el.querySelector('form.committed') !== null);

    // the receipt is still on screen: neither the loading-note swap (listLoaded must be sticky)
    // nor the live re-filter (shownCards pins id+kind) may destroy the committed card
    expect(el.querySelector('form.committed')).toBeTruthy();
    expect(el.textContent).toContain('Appended to the log');
    expect(el.querySelector('.state.all-clear')).toBeNull();
  });

  it('the Evidence panel is COLLAPSED on cold load (tab visible), opens on a card Evidence click with a fresh GET, and never disturbs the cards', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        runs: [
          {
            run: 'r-budget',
            status: { state: 'budget_exceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 },
            event_count: 3,
          },
        ],
      }),
    );
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;

    // cold load: panel hidden, tab visible (the prototype's PANELS.inbox = false)
    const panel = el.querySelector('.cpanel[data-panel="inbox"]') as HTMLElement;
    const tab = el.querySelector('.cpanel-tab[data-panel="inbox"]') as HTMLElement;
    expect(panel.hidden).toBe(true);
    expect(tab.hidden).toBe(false);

    // a real Evidence click opens the panel and fetches the run's derived state
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        run: 'r-budget',
        status: { state: 'budget_exceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 },
        event_count: 3,
      }),
    );
    (el.querySelector('[data-evidence]') as HTMLButtonElement).click();
    await settle(fixture, () => el.querySelector('#inbox-panel pre') !== null);

    expect(panel.hidden).toBe(false);
    expect((el.querySelector('[data-evidence]') as HTMLElement).getAttribute('aria-pressed')).toBe('true');
    const pre = el.querySelector('#inbox-panel pre') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.querySelector('.j-key'), 'the panel pre is jsonHi-highlighted').toBeTruthy();
    // reading never disturbed the card: the form is still there, untouched
    expect(el.querySelector('form[data-raise]')).toBeTruthy();
  });
});
