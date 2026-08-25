import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { SalvorClient } from '@salvor-run/client';

import { routes } from '../../app.routes';
import { SALVOR_CLIENT, provideSalvorApi } from '../../core/api';
import { Runs } from './runs';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

/**
 * Routes every fetch by URL rather than call order, so a test can freely mix `GET /v1/runs` with
 * however many `GET /v1/agents/{hash}` lookups the agent column's registry resolution fires.
 */
function routeFetch(byPath: Record<string, unknown>): ReturnType<typeof vi.fn> {
  return vi.fn((input: unknown) => {
    const url = String(input);
    for (const [path, body] of Object.entries(byPath)) {
      if (url.includes(path)) return Promise.resolve(jsonResponse(body));
    }
    return Promise.resolve(jsonResponse({ error: { code: 'unknown_agent', message: 'no agent registered' } }, 404));
  });
}

describe('Runs: agent column + grouping', () => {
  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [provideRouter(routes), provideSalvorApi({ baseUrl: 'http://test.local' })],
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    try {
      localStorage.clear();
    } catch {
      /* jsdom localStorage may be unavailable in some CI sandboxes: not this test's concern */
    }
  });

  async function mount() {
    const fixture = TestBed.createComponent(Runs);
    fixture.detectChanges();
    await fixture.whenStable();
    for (let i = 0; i < 8; i++) await Promise.resolve();
    fixture.detectChanges();
    return fixture;
  }

  it('renders the three honest agent-cell kinds: a caller label as-is, an unregistered hash truncated+copyable, and a registry-resolved name', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'r-label', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aarg_jd_parser_v1' },
            { run: 'r-hash', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'sha256:9f2caabbccdd' },
            { run: 'r-name', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'sha256:e8a1d362ffff' },
          ],
        },
        '/v1/agents/sha256:e8a1d362ffff': { agent: 'sha256:e8a1d362ffff', format: 'json', definition: JSON.stringify({ name: 'support-triage' }) },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;

    const labelRow = el.querySelector('tr[data-id="r-label"] td.c-agent')!;
    expect(labelRow.querySelector('.agent-id.is-label')?.textContent?.trim()).toBe('aarg_jd_parser_v1');

    const hashRow = el.querySelector('tr[data-id="r-hash"] td.c-agent')!;
    const hashSpan = hashRow.querySelector('.runref .runid')!;
    expect(hashSpan.textContent?.trim()).toBe('sha256:9f2caabb');
    expect(hashSpan.getAttribute('title')).toBe('sha256:9f2caabbccdd');
    expect(hashRow.querySelector('.copy')).toBeTruthy();

    const nameRow = el.querySelector('tr[data-id="r-name"] td.c-agent')!;
    const nameSpan = nameRow.querySelector('.agent-id.is-name')!;
    expect(nameSpan.textContent?.trim()).toBe('support-triage');
    expect(nameSpan.getAttribute('title')).toContain('sha256:e8a1d362ffff');
  });

  it('the Agent header sorts', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'r-z', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'zzz_agent' },
            { run: 'r-a', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aaa_agent' },
          ],
        },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;
    const runs = fixture.componentInstance;
    runs.setGroupMode('flat');
    fixture.detectChanges();

    const agentHeaderBtn = el.querySelector('#runs-table th[data-sort="agent"] button') as HTMLButtonElement;
    // first click: descending, same convention every non-id column defaults to (newest/most first)
    agentHeaderBtn.click();
    fixture.detectChanges();
    expect([...el.querySelectorAll('#runs-body tr[data-id]')].map((tr) => tr.getAttribute('data-id'))).toEqual([
      'r-z',
      'r-a',
    ]);

    // second click on the same header flips to ascending
    agentHeaderBtn.click();
    fixture.detectChanges();
    expect([...el.querySelectorAll('#runs-body tr[data-id]')].map((tr) => tr.getAttribute('data-id'))).toEqual([
      'r-a',
      'r-z',
    ]);
  });

  it('defaults to Grouped mode, clusters by build_id then agent, and flat mode shares the same row template', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'r1', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aarg_jd_parser_v1', labels: { build_id: 'bld_1' } },
            { run: 'r2', status: { state: 'running' }, event_count: 1, agent_def_hash: 'aarg_durable_retailor_v1', labels: { build_id: 'bld_1' } },
            { run: 'r3', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aarg_jd_parser_v1', labels: { build_id: 'bld_2' } },
          ],
        },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;

    expect(el.querySelector('.grpmode .gmode[aria-pressed="true"]')?.textContent?.trim()).toBe('Grouped');
    const headers = el.querySelectorAll('tr.grp-head');
    expect(headers.length).toBe(2); // bld_1, bld_2: plural, not degenerate
    const bld1 = [...headers].find((h) => h.textContent?.includes('bld_1'))!;
    expect(bld1.textContent).toContain('2 runs');
    expect(bld1.querySelector('.grp-kind')?.textContent?.trim()).toBe('build');

    // flip to Flat: same 6-column row template, no group headers
    const flatBtn = [...el.querySelectorAll('.grpmode .gmode')].find((b) => b.textContent?.trim() === 'Flat') as HTMLButtonElement;
    flatBtn.click();
    fixture.detectChanges();
    await fixture.whenStable();

    expect(el.querySelectorAll('tr.grp-head').length).toBe(0);
    expect(el.querySelectorAll('#runs-body tr[data-id]').length).toBe(3);
    expect(el.querySelectorAll('#runs-body tr[data-id] td').length).toBe(3 * 6);
  });

  it('WAITING RUNS MUST NOT HIDE: a group holding a waiting run cannot be collapsed', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'w1', status: { state: 'suspended', reason: 'needs input', input_schema: { type: 'object' } }, event_count: 1, agent_def_hash: 'sha256:triage000000' },
            { run: 'w2', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'sha256:triage000000' },
            { run: 'done1', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'sha256:otherxxxxxxx' },
          ],
        },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;

    const waitingHeader = [...el.querySelectorAll('tr.grp-head')].find((h) =>
      h.textContent?.includes('waiting on you'),
    )! as HTMLElement;
    const toggle = waitingHeader.querySelector('.grp-toggle') as HTMLButtonElement;
    expect(toggle.disabled, 'a group with a waiting run must have a disabled collapse toggle').toBe(true);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(toggle.getAttribute('title')).toMatch(/never hidden/);

    // both of its runs stay in the DOM: never hidden, even without clicking anything
    expect(el.querySelector('tr[data-id="w1"]')).toBeTruthy();
    expect(el.querySelector('tr[data-id="w2"]')).toBeTruthy();

    // a non-waiting group's toggle IS enabled and DOES collapse its rows on click
    const doneHeader = [...el.querySelectorAll('tr.grp-head')].find((h) => !h.textContent?.includes('waiting on you'))!;
    const doneToggle = doneHeader.querySelector('.grp-toggle') as HTMLButtonElement;
    expect(doneToggle.disabled).toBe(false);
    doneToggle.click();
    fixture.detectChanges();
    expect(el.querySelector('tr[data-id="done1"]')).toBeNull();
  });

  it('agent: and build: filter terms narrow the list', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'r1', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aarg_jd_parser_v1', labels: { build_id: 'bld_7f3a2c' } },
            { run: 'r2', status: { state: 'completed' }, event_count: 1, agent_def_hash: 'aarg_cover_writer_v1' },
          ],
        },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const runs = fixture.componentInstance;

    expect(runs.visible().map((r) => r.id)).toEqual(['r1', 'r2']);

    runs.toggleTerm('agent:jd_parser');
    fixture.detectChanges();
    expect(runs.visible().map((r) => r.id)).toEqual(['r1']);

    runs.toggleTerm('agent:jd_parser'); // un-toggle
    runs.toggleTerm('build:bld_7f3a2c');
    fixture.detectChanges();
    expect(runs.visible().map((r) => r.id)).toEqual(['r1']);
  });

  it('an overdue sleeping row takes the same attention paint a stalled row does, but one still short of its deadline does not', async () => {
    vi.stubGlobal(
      'fetch',
      routeFetch({
        '/v1/runs': {
          runs: [
            { run: 'overdue1', status: { state: 'sleeping', wake_at: '2020-01-01T00:00:00Z' }, event_count: 1 },
            { run: 'notdue1', status: { state: 'sleeping', wake_at: '2099-01-01T00:00:00Z' }, event_count: 1 },
          ],
        },
      }),
    );
    TestBed.overrideProvider(SALVOR_CLIENT, { useValue: new SalvorClient('http://test.local') });
    const fixture = await mount();
    const el = fixture.nativeElement as HTMLElement;
    const runs = fixture.componentInstance;
    runs.setGroupMode('flat'); // both rows sit in one progress group; flat mode skips the header
    fixture.detectChanges();

    const overdueRow = el.querySelector('tr[data-id="overdue1"]')!;
    expect(overdueRow.classList.contains('attention')).toBe(true);
    // the group classification does not move: it is still a progress-group sleeper, not waiting
    expect(overdueRow.classList.contains('g-progress')).toBe(true);
    expect(overdueRow.querySelector('.overdue-note')).toBeTruthy();

    const notDueRow = el.querySelector('tr[data-id="notdue1"]')!;
    expect(notDueRow.classList.contains('attention')).toBe(false);
    expect(notDueRow.querySelector('.overdue-note')).toBeNull();
  });
});
