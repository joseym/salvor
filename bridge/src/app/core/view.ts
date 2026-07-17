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

  readonly view: Signal<ViewName> = this._view.asReadonly();
  readonly runId: Signal<string | undefined> = this._runId.asReadonly();
  /** The filter query carried in `?q=` on the Runs view. */
  readonly query: Signal<string> = this._query.asReadonly();
  readonly title = computed(() => TITLES[this._view()]);

  /** A legacy hash captured at boot, redirected on the FIRST NavigationEnd so it wins the race
   * with the router's own initial navigation (which would otherwise land on the default view). */
  private pendingLegacyHash: string | null =
    typeof location !== 'undefined' ? location.hash : null;

  constructor() {
    this.readUrl();
    const sub = this.router.events
      .pipe(filter((e): e is NavigationEnd => e instanceof NavigationEnd))
      .subscribe(() => {
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

  /** Reflect the Runs filter into `?q=` without adding a history entry. */
  setQuery(q: string): void {
    this._query.set(q);
    void this.router.navigate([], {
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
