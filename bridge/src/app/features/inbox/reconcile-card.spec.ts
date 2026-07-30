import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';
import type { RunSummary } from '@salvor-run/client';

import { SALVOR_CLIENT } from '../../core/api';
import { ReconcileCard } from './reconcile-card';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

const RECONCILE_ROW: RunSummary = {
  run: 'run-needs-recon-1',
  status: { state: 'needs_reconciliation', raw: { state: 'needs_reconciliation' } },
  eventCount: 5,
  raw: {},
};

/* The evidence load is two plain 200 reads, never a 409 (a browser logs any non-2xx fetch to the
 * console unconditionally, which the suite's zero-console-errors gate treats as a failure; see
 * inbox-model.ts's reconcileIntentFrom doc comment for the full story). */
function pendingRunResponse(): Response {
  return jsonResponse({
    run: RECONCILE_ROW.run,
    status: { state: 'needs_reconciliation' },
    event_count: 5,
    pending: {
      kind: 'tool',
      seq: 4,
      tool: 'charge',
      input: { amount: 10 },
      effect: 'write',
      idempotency_key: null,
    },
  });
}

function pendingEventSse(): Response {
  return new Response(
    `data: ${JSON.stringify({
      run_id: RECONCILE_ROW.run,
      seq: 4,
      recorded_at: '2026-07-16T00:00:00Z',
      event: {
        kind: 'ToolCallRequested',
        payload: { tool: 'charge', input: { amount: 10 }, effect: 'write' },
      },
    })}\n\n`,
    { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
  );
}

describe('ReconcileCard, the [hidden]-trap lesson: branch visibility by state machine (item 14 / spec 14)', () => {
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

  async function flush(): Promise<void> {
    for (let i = 0; i < 10; i++) await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
    for (let i = 0; i < 10; i++) await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
  }

  async function mountCard() {
    fetchMock
      .mockResolvedValueOnce(pendingRunResponse()) // getRun() on ngOnInit
      .mockResolvedValueOnce(pendingEventSse()); // fetchAppendedEvent(pending.seq)
    const fixture = TestBed.createComponent(ReconcileCard);
    fixture.componentRef.setInput('row', RECONCILE_ROW);
    fixture.detectChanges();
    await fixture.whenStable();
    await flush();
    fixture.detectChanges();
    return fixture;
  }

  it('loads the recorded intent via two plain reads (getRun + the event at pending.seq) and renders it as evidence, including recorded_at', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('form[data-resolve]')).toBeTruthy();
    expect(el.textContent).toContain('charge');
    expect(el.textContent).toContain('2026-07-16T00:00:00Z');
    expect(el.querySelector('.badge.e-write')).toBeTruthy();
  });

  it('before any choice, BOTH branches carry [hidden], never both rendered', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    const reached = el.querySelector('[data-when="reached"]') as HTMLElement;
    const notReached = el.querySelector('[data-when="not_reached"]') as HTMLElement;
    expect(reached.hidden).toBe(true);
    expect(notReached.hidden).toBe(true);
  });

  it('choosing "reached" reveals the output field and hides the lock block, exclusively', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    const reachedRadio = el.querySelector('input[type="radio"][value="reached"]') as HTMLInputElement;
    reachedRadio.click();
    fixture.detectChanges();

    const reached = el.querySelector('[data-when="reached"]') as HTMLElement;
    const notReached = el.querySelector('[data-when="not_reached"]') as HTMLElement;
    expect(reached.hidden).toBe(false);
    expect(notReached.hidden, 'the lock block must be gone once "reached" is chosen').toBe(true);
  });

  it('choosing "not_reached" swaps exclusively: the lock block shows, the output field hides', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('input[type="radio"][value="reached"]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('input[type="radio"][value="not_reached"]') as HTMLInputElement).click();
    fixture.detectChanges();

    const reached = el.querySelector('[data-when="reached"]') as HTMLElement;
    const notReached = el.querySelector('[data-when="not_reached"]') as HTMLElement;
    expect(notReached.hidden).toBe(false);
    expect(reached.hidden, 'the output branch must be gone once "not_reached" is chosen').toBe(true);
  });

  it('the "not_reached" off-ramp offers the exact call to copy and says where a tool runs, without weakening the refusal', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('input[type="radio"][value="not_reached"]') as HTMLInputElement).click();
    fixture.detectChanges();

    const offramp = el.querySelector('[data-offramp="not_reached"]') as HTMLElement;
    expect(offramp).toBeTruthy();
    expect(offramp.hidden).toBe(false);
    // the copyable invocation carries the tool AND its exact recorded input
    const call = offramp.querySelector('.call') as HTMLElement;
    expect(call.textContent).toContain('charge');
    expect(call.textContent).toContain('amount');
    expect(offramp.querySelector('.copy-call')).toBeTruthy();
    // one truthful mechanism sentence on where such a tool executes
    expect(offramp.textContent).toContain('the host that registered it');
    // the refusal itself is untouched; resolve still cannot record an absence
    expect(el.querySelector('.readonly')!.textContent).toContain('Resolve cannot record');

    // Copy lifts the tool + input verbatim, formatted for reuse.
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    (offramp.querySelector('.copy-call') as HTMLButtonElement).click();
    await flush();
    expect(writeText).toHaveBeenCalledWith(JSON.stringify({ tool: 'charge', input: { amount: 10 } }, null, 2));
  });

  it('submit stays disabled until "reached" is chosen AND the immutable-history checkbox is confirmed', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    const submit = el.querySelector('button.serious[type="submit"]') as HTMLButtonElement;
    expect(submit.disabled).toBe(true);

    (el.querySelector('input[type="radio"][value="reached"]') as HTMLInputElement).click();
    fixture.detectChanges();
    expect(submit.disabled, 'a choice alone is not enough; the confirmation is required too').toBe(true);

    (el.querySelector('input[type="checkbox"]') as HTMLInputElement).click();
    fixture.detectChanges();
    expect(submit.disabled).toBe(false);

    // switching to "not_reached" must re-disable it: there is no submit path for that branch
    (el.querySelector('input[type="radio"][value="not_reached"]') as HTMLInputElement).click();
    fixture.detectChanges();
    expect(submit.disabled).toBe(true);
  });

  it('a committed resolve renders the receipt (seq, event.kind, recorded_at, payload, status now) and the endpoint stays outside any <details>', async () => {
    const fixture = await mountCard();
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('input[type="radio"][value="reached"]') as HTMLInputElement).click();
    (el.querySelector('input[type="checkbox"]') as HTMLInputElement).click();
    fixture.detectChanges();
    const textarea = el.querySelector('textarea') as HTMLTextAreaElement;
    textarea.value = '{"status":"succeeded"}';
    textarea.dispatchEvent(new Event('input'));
    fixture.detectChanges();

    fetchMock
      .mockResolvedValueOnce(jsonResponse({ run: RECONCILE_ROW.run, status: { state: 'running' } })) // resolve()
      .mockResolvedValueOnce(
        // the appended-event SSE fetch
        new Response(
          `data: ${JSON.stringify({
            run_id: RECONCILE_ROW.run,
            seq: 5,
            recorded_at: '2026-07-16T00:00:02Z',
            event: { kind: 'ToolCallCompleted', payload: { tool: 'charge', output: { status: 'succeeded' } } },
          })}\n\n`,
          { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
        ),
      )
      .mockResolvedValueOnce(
        jsonResponse({ run: RECONCILE_ROW.run, status: { state: 'running' }, event_count: 6 }),
      ); // getRun() re-derive

    const form = el.querySelector('form[data-resolve]') as HTMLFormElement;
    form.dispatchEvent(new Event('submit', { cancelable: true }));
    await fixture.whenStable();
    fixture.detectChanges();

    expect(el.querySelector('form.committed')).toBeTruthy();
    expect(el.textContent).toContain('Appended to the log');
    expect(el.textContent).toContain('ToolCallCompleted');
    const endpoint = el.querySelector('.endpoint') as HTMLElement;
    expect(endpoint.closest('details')).toBeNull();
  });
});
