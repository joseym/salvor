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
import { groupOf } from './features/runs/run-model';
import { Runs } from './features/runs/runs';

type NavLink = { readonly view: ViewName; readonly label: string };

const NAV_LINKS: readonly NavLink[] = [
  { view: 'runs', label: 'Runs' },
  { view: 'inspector', label: 'Inspector' },
  { view: 'inbox', label: 'Inbox' },
  { view: 'workflows', label: 'Workflows' },
  { view: 'spend', label: 'Spend' },
];

const SUBS: Readonly<Record<ViewName, string>> = {
  runs: 'A snapshot of GET /v1/runs — waiting-first.',
  inspector: 'One run, read from its log.',
  inbox: 'Runs waiting on a human decision.',
  workflows: 'Graph documents — author and validate.',
  spend: 'Folded usage, by agent and by day.',
};

/**
 * The app shell: desktop sidebar nav (brand, five view links with the Inbox count badge, theme
 * toggle, rail collapse), the ≤860px compact strip (measured `--nav-h`), the per-view topbar, and
 * the five `<section id="view-*">` panels that toggle `.is-active` from {@link ViewService}.
 */
@Component({
  selector: 'bridge-root',
  imports: [Runs, Inspector, Inbox],
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

  openPalette(): void {
    (document.getElementById('palette') as HTMLElement & { showPopover?: () => void })?.showPopover?.();
  }
  openKeys(): void {
    (document.getElementById('keys') as HTMLElement & { showPopover?: () => void })?.showPopover?.();
  }

  @ViewChild('paletteInput') private paletteInput?: ElementRef<HTMLInputElement>;

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
