import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';
import type { RunSummary } from '@salvor-run/client';

import { SALVOR_CLIENT } from '../../core/api';
import { SuspensionCard } from './suspension-card';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

const APPROVAL_SCHEMA = {
  title: 'Approve the refund',
  required: ['approved'],
  properties: {
    approved: { type: 'boolean', title: 'Approved' },
    note: { type: 'string', title: 'Note', default: 'looks fine' },
  },
};

const SUSPENDED_ROW: RunSummary = {
  run: 'run-suspended-1',
  status: {
    state: 'suspended',
    reason: 'a human must approve the refund',
    inputSchema: APPROVAL_SCHEMA,
    raw: { state: 'suspended', reason: 'a human must approve the refund', input_schema: APPROVAL_SCHEMA },
  },
  eventCount: 4,
  raw: {},
};

const UNGENERATABLE_ROW: RunSummary = {
  run: 'run-suspended-2',
  status: {
    state: 'suspended',
    reason: 'needs a free-form decision',
    inputSchema: { type: 'string' }, // no `properties` — the raw-JSON fallback trigger
    raw: {},
  },
  eventCount: 2,
  raw: {},
};

describe('SuspensionCard', () => {
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

  it('generates one field per schema property — never hand-written — and marks the run-proposed default', () => {
    const fixture = TestBed.createComponent(SuspensionCard);
    fixture.componentRef.setInput('row', SUSPENDED_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    expect(el.querySelector('form[data-resume]')).toBeTruthy();
    expect(el.querySelector('input[type="radio"][value="true"]')).toBeTruthy();
    expect(el.querySelector('input[type="radio"][value="false"]')).toBeTruthy();
    const noteInput = el.querySelector('input[type="text"]') as HTMLInputElement;
    expect(noteInput.value).toBe('looks fine'); // the schema's own proposed default
    expect(el.textContent).toContain('proposed by the run');
  });

  it('falls back to a raw-JSON textarea when the schema has no properties to generate from', () => {
    const fixture = TestBed.createComponent(SuspensionCard);
    fixture.componentRef.setInput('row', UNGENERATABLE_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    expect(el.querySelector('input[type="radio"]')).toBeNull();
    expect(el.querySelector('textarea')).toBeTruthy();
  });

  it('resumes with the extracted input, then renders the receipt with a real seq/kind/recorded_at/payload', async () => {
    const fixture = TestBed.createComponent(SuspensionCard);
    fixture.componentRef.setInput('row', SUSPENDED_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;

    (el.querySelector('input[type="radio"][value="true"]') as HTMLInputElement).click();
    fixture.detectChanges();

    fetchMock
      .mockResolvedValueOnce(jsonResponse({ run: SUSPENDED_ROW.run, outcome: 'driving', status: 'running' })) // client.resume()
      .mockResolvedValueOnce(
        // RunDetailService.resume() re-fetches internally (its own consequence-re-rendering contract)
        jsonResponse({ run: SUSPENDED_ROW.run, status: { state: 'running' }, event_count: 5 }),
      )
      .mockResolvedValueOnce(
        new Response(
          `data: ${JSON.stringify({
            run_id: SUSPENDED_ROW.run,
            seq: 4,
            recorded_at: '2026-07-16T00:00:03Z',
            event: { kind: 'Resumed', payload: { input: { approved: true, note: 'looks fine' } } },
          })}\n\n`,
          { status: 200, headers: { 'Content-Type': 'text/event-stream' } },
        ),
      )
      .mockResolvedValueOnce(jsonResponse({ run: SUSPENDED_ROW.run, status: { state: 'running' }, event_count: 5 }));

    const form = el.querySelector('form[data-resume]') as HTMLFormElement;
    form.dispatchEvent(new Event('submit', { cancelable: true }));
    await fixture.whenStable();
    for (let i = 0; i < 5; i++) await Promise.resolve();
    fixture.detectChanges();

    expect(el.querySelector('form.committed')).toBeTruthy();
    expect(el.textContent).toContain('Resumed');
    expect(el.textContent).toContain('4'); // seq
    const endpoint = el.querySelector('.actions .endpoint') as HTMLElement;
    expect(endpoint.closest('details')).toBeNull();
  });

  it('demotes the endpoint behind <details class="api"><summary>show API</summary>, closed by default', () => {
    const fixture = TestBed.createComponent(SuspensionCard);
    fixture.componentRef.setInput('row', SUSPENDED_ROW);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const details = el.querySelector('details.api') as HTMLDetailsElement;
    expect(details).toBeTruthy();
    expect(details.open).toBe(false);
    expect(details.querySelector('summary')?.textContent).toBe('show API');
    expect(details.querySelector('.endpoint')).toBeTruthy();
  });
});
