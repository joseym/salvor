import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';
import type { RunSummary } from '@salvor-run/client';

import { SALVOR_CLIENT } from '../../core/api';
import { AbandonAction } from './abandon-action';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

const ROW: RunSummary = {
  run: 'run-abandon-me-1',
  status: { state: 'running', raw: { state: 'running' } },
  eventCount: 1,
  raw: {},
};

/** The abandon receipt the server returns for a run with no dangling write. */
function abandonResponse(extra: Record<string, unknown> = {}): Response {
  return jsonResponse({
    run: ROW.run,
    abandoned: true,
    appended_seq: 1,
    status: { state: 'abandoned', reason: 'husk is dead forever', ...extra },
  });
}

describe('AbandonAction: the retire affordance', () => {
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
  }

  function mount(variant: 'primary' | 'secondary', reconcile = false) {
    const fixture = TestBed.createComponent(AbandonAction);
    fixture.componentRef.setInput('row', ROW);
    fixture.componentRef.setInput('variant', variant);
    fixture.componentRef.setInput('reconcile', reconcile);
    fixture.detectChanges();
    return fixture;
  }

  it('the trigger never abandons on a single click; it opens a confirm affordance first', async () => {
    const fixture = mount('primary');
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    expect(el.querySelector('[data-abandon-confirm]'), 'confirm opens').toBeTruthy();
    expect(fetchMock, 'no call is made just by opening the confirm').not.toHaveBeenCalled();
  });

  it('the SECONDARY variant is visually subordinate (abandon-secondary); the PRIMARY is not', () => {
    const sec = mount('secondary').nativeElement as HTMLElement;
    expect((sec.querySelector('[data-abandon]') as HTMLElement).classList).toContain('abandon-secondary');
    const pri = mount('primary').nativeElement as HTMLElement;
    expect((pri.querySelector('[data-abandon]') as HTMLElement).classList).not.toContain('abandon-secondary');
  });

  it('submit is disabled until the "I understand" box is confirmed', async () => {
    const fixture = mount('primary');
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    const submit = el.querySelector('[data-abandon-submit]') as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    expect(submit.disabled).toBe(false);
  });

  it('a confirmed submit calls POST /abandon and renders the receipt with the appended seq and re-derived status', async () => {
    fetchMock.mockResolvedValueOnce(abandonResponse());
    const fixture = mount('primary');
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
    await flush();
    fixture.detectChanges();

    const [url, init] = fetchMock.mock.calls[0];
    expect(String(url)).toContain(`/v1/runs/${ROW.run}/abandon`);
    expect(String((init as RequestInit).method)).toBe('POST');

    const receipt = el.querySelector('[data-abandon-receipt]') as HTMLElement;
    expect(receipt).toBeTruthy();
    expect(receipt.textContent).toContain('Appended RunAbandoned at seq 1');
    expect(receipt.textContent).toContain('abandoned');
    expect(receipt.textContent).toContain('Nothing was edited or re-run');
  });

  it('the RECONCILE variant states plainly that the write stays unresolved and is recorded as such', async () => {
    const fixture = mount('secondary', true);
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    const confirm = el.querySelector('[data-abandon-confirm]') as HTMLElement;
    expect(confirm.textContent).toContain('unresolved');
    expect(confirm.textContent?.toLowerCase()).toContain('recorded as such');
  });

  it('a reconcile-abandon receipt surfaces the unresolved-write honesty line from the server', async () => {
    fetchMock.mockResolvedValueOnce(
      abandonResponse({ unresolved_write: { seq: 4, tool: 'charge' } }),
    );
    const fixture = mount('secondary', true);
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
    await flush();
    fixture.detectChanges();

    const honesty = el.querySelector('[data-abandon-unresolved]') as HTMLElement;
    expect(honesty).toBeTruthy();
    expect(honesty.textContent).toContain('seq 4');
    expect(honesty.textContent).toContain('charge');
    expect(honesty.textContent).toContain('unresolved');
  });

  it('emits receipted the instant the receipt lands, BEFORE committed: the parent must be able to pin before the commit-triggered refresh lands', async () => {
    fetchMock.mockResolvedValueOnce(abandonResponse());
    const fixture = mount('primary');
    const order: string[] = [];
    fixture.componentInstance.receipted.subscribe(() => order.push('receipted'));
    fixture.componentInstance.committed.subscribe(() => order.push('committed'));
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
    await flush();

    expect(order).toEqual(['receipted', 'committed']);
  });

  it('the receipt carries an explicit "Done" dismiss, never a timer, which emits retire and nothing else (this component never removes or animates its own card)', async () => {
    fetchMock.mockResolvedValueOnce(abandonResponse());
    const fixture = mount('primary');
    let retired = 0;
    fixture.componentInstance.retire.subscribe(() => retired++);
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
    await flush();
    fixture.detectChanges();

    const done = el.querySelector('[data-abandon-done]') as HTMLButtonElement;
    expect(done, 'the receipt offers an explicit dismiss').toBeTruthy();
    expect(done.textContent?.trim()).toBe('Done');
    // the receipt itself is untouched by dismissing: this component never destroys its own record;
    // the parent decides whether/how to fold the card away.
    expect(el.querySelector('[data-abandon-receipt]'), 'the receipt is still rendered after Done').toBeTruthy();

    done.click();
    fixture.detectChanges();
    expect(retired).toBe(1);
    expect(el.querySelector('[data-abandon-receipt]'), 'still not destroyed: that is the parent\'s call').toBeTruthy();
  });

  it('emits announce and committed after a successful abandon', async () => {
    fetchMock.mockResolvedValueOnce(abandonResponse());
    const fixture = mount('primary');
    const messages: string[] = [];
    let committed = 0;
    fixture.componentInstance.announce.subscribe((m) => messages.push(m));
    fixture.componentInstance.committed.subscribe(() => committed++);
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('[data-abandon]') as HTMLButtonElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-understand]') as HTMLInputElement).click();
    fixture.detectChanges();
    (el.querySelector('[data-abandon-submit]') as HTMLButtonElement).click();
    await flush();

    expect(committed).toBe(1);
    expect(messages.length).toBe(1);
    expect(messages[0]).toContain('Abandoned run');
    expect(messages[0]).toContain('sequence 1');
  });
});
