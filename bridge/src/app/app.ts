import {
  AfterViewInit,
  Component,
  DestroyRef,
  ElementRef,
  type Signal,
  ViewChild,
  computed,
  inject,
  signal,
} from '@angular/core';

import { RunsService } from './core/api';
import { PillService } from './core/pill';
import { ThemeService } from './core/theme';
import { ViewService, type ViewName } from './core/view';
import { Inbox } from './features/inbox/inbox';
import { Inspector } from './features/inspector/inspector';
import { groupOf, labelOf } from './features/runs/run-model';
import { Runs } from './features/runs/runs';
import { Spend } from './features/spend/spend';

type NavLink = { readonly view: ViewName; readonly label: string };

/** One row in the ⌘K palette: a named view to switch to, or a live run to open by id. */
type PaletteItem =
  | { readonly kind: 'view'; readonly id: string; readonly label: string; readonly hint: string; readonly view: ViewName }
  | { readonly kind: 'run'; readonly id: string; readonly label: string; readonly hint: string; readonly runId: string };

// Workflows is not a navigable view: the graph canvas ships with the v0.4 engine. It is absent
// from the nav, but /workflows still resolves (see routes) so a deep link lands on an honest note
// rather than a dead end.
const NAV_LINKS: readonly NavLink[] = [
  { view: 'runs', label: 'Runs' },
  { view: 'inspector', label: 'Inspector' },
  { view: 'inbox', label: 'Inbox' },
  { view: 'spend', label: 'Spend' },
];

const SUBS: Readonly<Record<ViewName, string>> = {
  runs: 'A snapshot of GET /v1/runs — waiting-first.',
  inspector: 'One run, read from its log.',
  inbox: 'Runs waiting on a human decision.',
  workflows: 'Ships with the graph engine.',
  spend: 'Folded usage, by agent and by day.',
};

/**
 * The app shell: desktop sidebar nav (brand, the four view links with the Inbox count badge, theme
 * toggle, rail collapse), the ≤860px compact strip (measured `--nav-h`), the per-view topbar, and
 * the `<section id="view-*">` panels that toggle `.is-active` from {@link ViewService}. Workflows
 * is not among the nav links, but its section stays mounted so a `/workflows` deep link resolves.
 */
@Component({
  selector: 'bridge-root',
  imports: [Runs, Inspector, Inbox, Spend],
  templateUrl: './app.html',
  host: { '(document:keydown)': 'onGlobalKeydown($event)' },
})
export class App implements AfterViewInit {
  private readonly runsService = inject(RunsService);
  private readonly viewService = inject(ViewService);
  private readonly destroyRef = inject(DestroyRef);
  private readonly pill = inject(PillService);
  protected readonly theme = inject(ThemeService);

  @ViewChild('appNav') private appNav?: ElementRef<HTMLElement>;

  readonly navLinks = NAV_LINKS;
  readonly view = this.viewService.view;
  readonly title = this.viewService.title;
  readonly sub = computed(() => SUBS[this.view()]);

  readonly navCollapsed = signal(false);
  readonly dotKeyOpen = signal(false);

  /** The Inbox badge is the count of runs waiting on a human — the same fold the health strip and
   * chips read, so the number can never disagree. */
  readonly inboxCount = computed(
    () => this.runsService.runs().filter((r) => groupOf(r.status.state) === 'waiting').length,
  );

  constructor() {
    // apply the collapse choice to the document root so the CSS rail rules fire
    this.applyNav();
  }

  ngAfterViewInit(): void {
    // publish the strip's measured height so the ≤860 selection drawer starts BELOW it
    this.measureNav();
    if (typeof ResizeObserver !== 'undefined' && this.appNav) {
      const ro = new ResizeObserver(() => this.measureNav());
      ro.observe(this.appNav.nativeElement);
      this.destroyRef.onDestroy(() => ro.disconnect());
    }
    if (typeof window !== 'undefined') {
      const onResize = () => this.measureNav();
      window.addEventListener('resize', onResize);
      this.destroyRef.onDestroy(() => window.removeEventListener('resize', onResize));
    }
  }

  private measureNav(): void {
    if (typeof document === 'undefined' || !this.appNav) return;
    const wide = typeof matchMedia !== 'undefined' ? matchMedia('(min-width: 861px)').matches : true;
    const px = wide ? '0px' : Math.round(this.appNav.nativeElement.getBoundingClientRect().height) + 'px';
    const root = document.documentElement;
    if (px !== root.style.getPropertyValue('--nav-h')) root.style.setProperty('--nav-h', px);
  }

  isActive(view: ViewName): boolean {
    return this.view() === view;
  }
  ariaCurrent(view: ViewName): 'page' | null {
    return this.isActive(view) ? 'page' : null;
  }
  go(view: ViewName): void {
    this.viewService.go(view);
    // land focus in the new view for keyboard / screen-reader users
    setTimeout(() => document.getElementById('view-title')?.focus(), 0);
  }

  toggleTheme(): void {
    this.theme.toggle();
  }
  toggleNav(): void {
    this.navCollapsed.update((v) => !v);
    this.applyNav();
  }
  private applyNav(): void {
    if (typeof document === 'undefined') return;
    document.documentElement.dataset['nav'] = this.navCollapsed() ? 'collapsed' : 'expanded';
  }

  onSkip(e: Event): void {
    e.preventDefault();
    document.getElementById('view-title')?.focus();
  }

  toggleDotKey(): void {
    this.dotKeyOpen.update((v) => !v);
  }

  @ViewChild('paletteInput') private paletteInput?: ElementRef<HTMLInputElement>;

  // ── ⌘K palette: jump to one of the views by name, or to a live run by id prefix ──
  readonly paletteQuery = signal('');
  readonly paletteActive = signal(0);
  /** Whether the palette popover is open. A closed palette holds no rows, no query and no active
   * descendant — the list is genuinely empty, so nothing lingers behind the dismissed dialog. */
  readonly paletteOpen = signal(false);

  /** The visible palette rows: the named views that match, then live runs whose id starts with the
   * query. An empty query offers every view and the most recent runs, so ⌘K then Enter is useful
   * before a single key is typed. Empty while the palette is closed. */
  readonly paletteItems = computed<readonly PaletteItem[]>(() => {
    if (!this.paletteOpen()) return [];
    const q = this.paletteQuery().trim().toLowerCase();
    const views: PaletteItem[] = NAV_LINKS.map((l) => ({
      kind: 'view' as const,
      id: 'view:' + l.view,
      label: l.label,
      hint: 'view',
      view: l.view,
    }));
    const viewMatches = q ? views.filter((v) => v.label.toLowerCase().includes(q)) : views;
    const runs = this.runsService.runs();
    const runMatches: PaletteItem[] = (q ? runs.filter((r) => r.run.toLowerCase().startsWith(q)) : runs.slice(0, 6))
      .slice(0, 8)
      .map((r) => ({
        kind: 'run' as const,
        id: 'run:' + r.run,
        label: r.run.slice(0, 12),
        hint: labelOf(r.status.state),
        runId: r.run,
      }));
    return [...viewMatches, ...runMatches];
  });

  /** The id the input's `aria-activedescendant` points at, clamped to the current result count.
   * Null while closed (no rows) so a dismissed palette leaves no dangling reference. */
  readonly paletteActiveId = computed(() => {
    const n = this.paletteItems().length;
    return n ? 'pal-' + Math.min(this.paletteActive(), n - 1) : null;
  });

  openPalette(): void {
    this.paletteQuery.set('');
    this.paletteActive.set(0);
    this.paletteOpen.set(true);
    (document.getElementById('palette') as HTMLElement & { showPopover?: () => void })?.showPopover?.();
    setTimeout(() => this.paletteInput?.nativeElement.focus(), 0);
  }
  private closePalette(): void {
    (document.getElementById('palette') as HTMLElement & { hidePopover?: () => void })?.hidePopover?.();
  }

  /** The popover's own toggle event is the authority on open/closed — it fires for the light-dismiss
   * (Escape, click-away) that never routes through {@link closePalette}. Closing clears the query,
   * the active row and the open flag together, so the list empties and no state survives. */
  onPaletteToggle(e: Event): void {
    const open = (e as ToggleEvent).newState === 'open';
    this.paletteOpen.set(open);
    if (!open) {
      this.paletteQuery.set('');
      this.paletteActive.set(0);
    }
  }
  openKeys(): void {
    (document.getElementById('keys') as HTMLElement & { showPopover?: () => void })?.showPopover?.();
  }

  onPaletteInput(e: Event): void {
    this.paletteQuery.set((e.target as HTMLInputElement).value);
    this.paletteActive.set(0);
  }

  onPaletteKeydown(e: KeyboardEvent): void {
    const items = this.paletteItems();
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      this.paletteActive.set(items.length ? Math.min(items.length - 1, this.paletteActive() + 1) : 0);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      this.paletteActive.set(Math.max(0, this.paletteActive() - 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const item = items[Math.min(this.paletteActive(), items.length - 1)];
      if (item) this.activatePaletteItem(item);
    }
    // Escape is left to the popover's own light-dismiss.
  }

  activatePaletteItem(item: PaletteItem): void {
    this.closePalette();
    if (item.kind === 'view') this.viewService.go(item.view);
    else this.viewService.openRun(item.runId);
    setTimeout(() => document.getElementById('view-title')?.focus(), 0);
  }

  onGlobalKeydown(e: KeyboardEvent): void {
    const inField = (e.target as HTMLElement)?.closest?.('input, textarea, select, [contenteditable]');
    if ((e.key === 'k' || e.key === 'K') && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      this.openPalette();
      return;
    }
    if (e.key === '?' && !inField) {
      e.preventDefault();
      this.openKeys();
      return;
    }
    if (e.key === '/' && !inField) {
      e.preventDefault();
      this.viewService.go('runs');
      setTimeout(() => document.getElementById('q')?.focus(), 0);
    }
  }

  // ── the connection pill, driven by whatever stream is actually open (PillService) ──
  private readonly hms = new Intl.DateTimeFormat('en-GB', {
    timeZone: 'UTC',
    hour12: false,
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });

  readonly connState: Signal<string> = computed(() => this.pill.state().kind);

  readonly connLabel: Signal<string> = computed(() => {
    const s = this.pill.state();
    if (s.kind === 'idle') return `Snapshot · as of ${this.hms.format(new Date(s.asOf))}Z`;
    return s.label;
  });

  readonly connRun: Signal<string> = computed(() => {
    const id = this.pill.runId();
    const kind = this.pill.state().kind;
    return id && (kind === 'connected' || kind === 'ended') ? 'run ' + id.slice(0, 8) : '';
  });
}
