import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { App } from './app';
import { routes } from './app.routes';
import { provideSalvorApi } from './core/api';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
}

describe('App', () => {
  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes), provideSalvorApi({ baseUrl: '' })],
    }).compileComponents();
  });

  it('should create the app', () => {
    const fixture = TestBed.createComponent(App);
    const app = fixture.componentInstance;
    expect(app).toBeTruthy();
  });

  it('should render the shell brand, the five nav links and the theme toggle', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();
    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('#app-nav')).toBeTruthy();
    expect(compiled.textContent).toContain('Salvor');
    expect(compiled.textContent).toContain('bridge');
    // All five views are nav destinations — Workflows joined when the graph engine shipped.
    expect(compiled.querySelectorAll('.nav-link').length).toBe(5);
    const labels = Array.from(compiled.querySelectorAll('.nav-link .nav-text')).map((n) => n.textContent?.trim());
    expect(labels).toEqual(['Runs', 'Inspector', 'Inbox', 'Workflows', 'Spend']);
    expect(compiled.querySelector('#theme-toggle')).toBeTruthy();
  });

  describe('the About panel build colophon (#wf-server-note) — reads the same capability probe as the honesty chip', () => {
    let fetchMock: ReturnType<typeof vi.fn>;

    beforeEach(() => {
      fetchMock = vi.fn();
      vi.stubGlobal('fetch', fetchMock);
    });

    afterEach(() => {
      vi.unstubAllGlobals();
    });

    it('FALLBACK: shows the honest dash before the probe resolves, or against a server with no server block', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: { fork: true } }));
      const fixture = TestBed.createComponent(App);
      fixture.detectChanges();
      await fixture.whenStable();
      const note = fixture.nativeElement.querySelector('#wf-server-version') as HTMLElement;
      expect(note.textContent?.trim()).toBe('—');
    });

    it('renders the real version and commit once the probe resolves', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({ capabilities: { fork: true }, server: { version: '0.1.0', commit: 'a1b2c3d' } }),
      );
      const fixture = TestBed.createComponent(App);
      fixture.detectChanges();
      await fixture.whenStable();
      const note = fixture.nativeElement.querySelector('#wf-server-version') as HTMLElement;
      expect(note.textContent?.trim()).toBe('server v0.1.0 (a1b2c3d)');
    });

    it('omits the parenthetical when the build has no commit (a tarball or no-git build)', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: { fork: true }, server: { version: '0.1.0' } }));
      const fixture = TestBed.createComponent(App);
      fixture.detectChanges();
      await fixture.whenStable();
      const note = fixture.nativeElement.querySelector('#wf-server-version') as HTMLElement;
      expect(note.textContent?.trim()).toBe('server v0.1.0');
    });
  });
});
