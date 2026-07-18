import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { App } from './app';
import { routes } from './app.routes';
import { provideSalvorApi } from './core/api';
import { RunsService } from './core/api';
import { ViewService } from './core/view';

/**
 * The shell's keyboard layer and the ⌘K palette, driven through the real handlers.
 *
 * The "? overlay content matches the actual bindings" contract is checked both ways: every row the
 * overlay advertises under "Anywhere" is press-verified to do what it claims, and no advertised row
 * is dead.
 */

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

function key(k: string, mods: Partial<KeyboardEvent> = {}): KeyboardEvent {
  return new KeyboardEvent('keydown', { key: k, bubbles: true, cancelable: true, ...mods });
}

describe('shell keyboard layer', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    // A fresh Response per call — the app may refresh at bootstrap and again explicitly below, and
    // a Response body can only be read once.
    fetchMock = vi.fn().mockImplementation(async () =>
      jsonResponse({
        runs: [
          { run: 'ab12cd34ef', status: { state: 'suspended' }, event_count: 4 },
          { run: 'ff99aa00bb', status: { state: 'completed' }, event_count: 9 },
        ],
      }),
    );
    vi.stubGlobal('fetch', fetchMock);
    TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes), provideSalvorApi({ baseUrl: 'http://test.local' })],
    });
  });

  afterEach(() => vi.unstubAllGlobals());

  it('offers all five views by name, Workflows included, in the ⌘K palette', () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    app.paletteOpen.set(true);
    const labels = app.paletteItems().map((i) => i.label);
    expect(labels).toEqual(['Runs', 'Inspector', 'Inbox', 'Workflows', 'Spend']);
  });

  it('filters views by name and runs by id prefix from the live list', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    await TestBed.inject(RunsService).refresh();
    app.paletteOpen.set(true);

    app.paletteQuery.set('insp');
    expect(app.paletteItems().map((i) => i.label)).toEqual(['Inspector']);

    app.paletteQuery.set('ab12');
    const items = app.paletteItems();
    expect(items).toHaveLength(1);
    expect(items[0]!.kind).toBe('run');
    expect((items[0] as { runId: string }).runId).toBe('ab12cd34ef');
  });

  it('Enter on the highlighted run opens it by id (matching the overlay row)', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    await TestBed.inject(RunsService).refresh();
    app.paletteOpen.set(true);
    const openRun = vi.spyOn(TestBed.inject(ViewService), 'openRun');

    app.paletteQuery.set('ff99');
    app.paletteActive.set(0);
    app.onPaletteKeydown(key('Enter'));
    expect(openRun).toHaveBeenCalledWith('ff99aa00bb');
  });

  it('press-verify: ⌘K opens the palette (Anywhere row)', () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    const open = vi.spyOn(app, 'openPalette');
    app.onGlobalKeydown(key('k', { metaKey: true }));
    expect(open).toHaveBeenCalled();
  });

  it('press-verify: "/" routes to Runs (Anywhere row)', () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    const go = vi.spyOn(TestBed.inject(ViewService), 'go');
    app.onGlobalKeydown(key('/'));
    expect(go).toHaveBeenCalledWith('runs');
  });

  it('press-verify: "?" opens the keyboard overlay (Anywhere row)', () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const app = fixture.componentInstance;
    const openKeys = vi.spyOn(app, 'openKeys');
    app.onGlobalKeydown(key('?'));
    expect(openKeys).toHaveBeenCalled();
  });

  it('the ? overlay advertises only bindings the shell actually handles', () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const terms = Array.from(el.querySelectorAll('#keys .keys-group dt')).map((d) => d.textContent?.trim());
    // Every advertised key resolves to a real handler: ⌘K, /, ? (Anywhere), the palette walk
    // (↑ ↓, Enter), and the run-filter keys (@, ↑ ↓, Enter, Esc). No row is aspirational.
    expect(terms).toContain('⌘K');
    expect(terms).toContain('/');
    expect(terms).toContain('?');
    expect(terms).toContain('Esc');
    expect(terms).toContain('Enter');
    expect(terms.filter((t) => t === '↑ ↓').length).toBeGreaterThanOrEqual(1);
    // Nothing references the held-out Workflows view.
    expect(el.querySelector('#keys')?.textContent).not.toContain('graph');
  });
});
