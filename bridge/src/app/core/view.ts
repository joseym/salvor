import { DestroyRef, Injectable, type Signal, computed, inject, signal } from '@angular/core';
import { NavigationEnd, Router } from '@angular/router';
import { filter } from 'rxjs/operators';

export type ViewName = 'runs' | 'inspector' | 'inbox' | 'workflows' | 'spend';

const VIEWS: readonly ViewName[] = ['runs', 'inspector', 'inbox', 'workflows', 'spend'];
const TITLES: Readonly<Record<ViewName, string>> = {
  runs: 'Runs',
  inspector: 'Inspector',
  inbox: 'Inbox',
  workflows: 'Workflows',
  spend: 'Spend',
};

/**
 * The one authority on "which view is active", derived from the Angular Router's URL.
 *
 * Canonical URLs are PATHs (`/inspector/:runId`, `/inbox`, `/workflows/:hashPrefix`, `/spend`,
 * `/runs` default), not hashes. Old-style hash links are still honoured: {@link redirectLegacyHash}
 * converts a pasted `#inspector/<id>` (etc.) into the path equivalent, so a deep link from before
 * the switch to path routing still lands on the right view.
 *
 * The five `<section id="view-*">` elements all live in the shell at once and toggle `.is-active`,
 * rather than a router-outlet swap, so every view section stays addressable by id with an
 * `is-active` class and an `aria-current="page"` nav link at all times, not only while its route
 * is the active one.
 */
@Injectable({ providedIn: 'root' })
export class ViewService {
  private readonly router = inject(Router);
  private readonly destroyRef = inject(DestroyRef);

  private readonly _view = signal<ViewName>('runs');
  private readonly _runId = signal<string | undefined>(undefined);
  private readonly _query = signal<string>('');
  private readonly _externalFilter = signal<{ readonly q: string; readonly nonce: number } | undefined>(
    undefined,
  );
  private readonly _inboxFocus = signal<{ readonly runId: string; readonly nonce: number } | undefined>(
    undefined,
  );

  readonly view: Signal<ViewName> = this._view.asReadonly();
  readonly runId: Signal<string | undefined> = this._runId.asReadonly();
  /** The filter query carried in `?q=` on the Runs view. */
  readonly query: Signal<string> = this._query.asReadonly();
  /** A filter applied from OUTSIDE the Runs view (Spend's hour-bucket click today): Runs reads
   * `query()` only once, at construction, since the Runs section is mounted for the app's whole
   * life and a later `?q=` change is otherwise never re-read. This is the one other channel: a
   * fresh object every call (the `nonce`), so clicking the same hour twice still re-applies it. */
  readonly externalFilter = this._externalFilter.asReadonly();
  /** A request from another view (the Runs side panel) to open the Inbox landed on a specific run's
   * action card. A fresh object every call (the `nonce`), so re-clicking the same run re-focuses it
   * even though the Inbox stays mounted. The Inbox is the single action surface; this is a signpost
   * to the right card, not a second place to act. */
  readonly inboxFocus = this._inboxFocus.asReadonly();
  readonly title = computed(() => TITLES[this._view()]);

  /** A legacy hash captured at boot, redirected on the FIRST NavigationEnd so it wins the race
   * with the router's own initial navigation (which would otherwise land on the default view). */
  private pendingLegacyHash: string | null =
    typeof location !== 'undefined' ? location.hash : null;

  /** False until the router's first NavigationEnd. A mounted-at-boot view (Runs) can call
   * {@link setQuery} while this is still false (before the initial navigation has committed), when
   * the router's current route is still the default `/`, not the deep link being resolved. See
   * {@link setQuery} for how the reflection is aimed at the real path until this flips true. */
  private _navigationSettled = false;

  constructor() {
    this.readUrl();
    const sub = this.router.events
      .pipe(filter((e): e is NavigationEnd => e instanceof NavigationEnd))
      .subscribe(() => {
        // The router has now committed its initial navigation, so a deep link's own URL is in
        // place: only from here may the app reflect the Runs filter back into the URL. Reflecting
        // it any earlier (a mounted-at-boot Runs writing `?q=` before this fires) issues a
        // navigate([]) against the pre-navigation `/`, which clobbers the very deep link the router
        // is mid-way to resolving. See {@link setQuery}.
        this._navigationSettled = true;
        if (this.pendingLegacyHash !== null) {
          const hash = this.pendingLegacyHash;
          this.pendingLegacyHash = null;
          if (this.redirectLegacyHash(hash)) return; // its own NavigationEnd re-reads the URL
        }
        this.readUrl();
      });
    this.destroyRef.onDestroy(() => sub.unsubscribe());

    if (typeof window !== 'undefined') {
      const onHash = () => {
        this.redirectLegacyHash(location.hash);
      };
      window.addEventListener('hashchange', onHash);
      this.destroyRef.onDestroy(() => window.removeEventListener('hashchange', onHash));
    }
  }

  go(view: ViewName): void {
    if (view === 'inspector' && this._runId()) {
      void this.router.navigate(['/inspector', this._runId()], { queryParamsHandling: 'preserve' });
      return;
    }
    void this.router.navigate(['/' + view], { queryParamsHandling: 'preserve' });
  }

  openRun(runId: string): void {
    void this.router.navigate(['/inspector', runId]);
  }

  /** Open the Inbox landed on a run's action card. Navigates to `/inbox` (the single action
   * surface) and flags which run's card to focus: the Runs side panel's signpost for a waiting run. */
  openInboxCard(runId: string): void {
    this._inboxFocus.set({ runId, nonce: Date.now() });
    void this.router.navigate(['/inbox']);
  }

  /** Open the canvas on a stored graph: the same `/workflows/<hashPrefix>` door a fork lands on. */
  openGraph(hash: string): void {
    void this.router.navigate(['/workflows', hash.replace(/^sha256:/, '').slice(0, 12)]);
  }

  /** Navigate to Runs with `q` applied: the same filter mechanism Runs' own pills write, reached
   * from another view (Spend's activity chart). */
  filterRuns(q: string): void {
    this._externalFilter.set({ q, nonce: Date.now() });
    void this.router.navigate(['/runs'], { queryParams: { q } });
  }

  /** Reflect the Runs filter into `?q=` without adding a history entry.
   *
   * A mounted-at-boot Runs reflects its (empty) query at construction, BEFORE the router has
   * committed its initial navigation, so `navigate([])` there resolves against the pre-navigation
   * default `/` and, with `replaceUrl`, overwrites the very deep link the router is still resolving
   * (`/inspector/<id>` cold-loads bounced to `/runs`). Until the first NavigationEnd, target the
   * ACTUAL browser path instead of the router's stale current route, so the reflection lands the
   * query on the real deep-link URL rather than clobbering it back to `/`. Once settled, `[]` (the
   * committed current route) is canonical. */
  setQuery(q: string): void {
    this._query.set(q);
    const commands =
      this._navigationSettled || typeof location === 'undefined' ? [] : [location.pathname];
    void this.router.navigate(commands, {
      queryParams: { q: q || null },
      queryParamsHandling: 'merge',
      replaceUrl: true,
    });
  }

  private readUrl(): void {
    let route = this.router.routerState.root.snapshot;
    while (route.firstChild) route = route.firstChild;
    const view = (route.data['view'] as ViewName | undefined) ?? 'runs';
    this._view.set(VIEWS.includes(view) ? view : 'runs');
    this._runId.set((route.paramMap.get('runId') as string | null) ?? undefined);
    this._query.set((route.queryParamMap.get('q') as string | null) ?? '');
  }

  /**
   * Convert a legacy hash deep link to its path equivalent. Returns true if a redirect was
   * issued. `#inspector/<id>` → `/inspector/<id>`; `#inbox`/`#spend`/`#workflows`/`#runs` →
   * `/<view>`; an unknown hash falls back to `/runs`, never an error.
   */
  private redirectLegacyHash(hash: string): boolean {
    if (!hash || !hash.startsWith('#')) return false;
    const body = hash.replace(/^#/, '');
    if (!body || body.startsWith('view-') || body === '/') return false;
    const [view, id] = body.split('/');
    if (view === 'inspector' && id) {
      void this.router.navigate(['/inspector', id], { replaceUrl: true });
      return true;
    }
    if (view === 'workflows' && id) {
      void this.router.navigate(['/workflows', id], { replaceUrl: true });
      return true;
    }
    const target = (VIEWS as readonly string[]).includes(view) ? view : 'runs';
    void this.router.navigate(['/' + target], { replaceUrl: true });
    return true;
  }
}
