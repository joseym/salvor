import {
  type AfterViewInit,
  Component,
  ElementRef,
  ViewChild,
  computed,
  effect,
  inject,
  signal,
} from '@angular/core';

import { RunsService } from '../../core/api';
import { focusWhenRendered } from '../../core/focus';
import { ViewService } from '../../core/view';
import { RunRef } from '../inbox/run-ref';
import { callCost, int, usd } from '../inspector/pricing';
import { groupOf, hourKey, labelOf, toRunRow, type RunRow } from '../runs/run-model';
import {
  type ActivityWindow,
  activityDescText,
  bucketEvents,
  hourTermOf,
  renderActivityHtml,
} from './activity';
import { type Ceiling, type FoldedRun, CostFoldService, ceilingFor } from './cost-fold';

/** One burn-down row: a run paired with its fold, once folding has reached it. */
interface MeterRow {
  readonly row: RunRow;
  readonly folded: FoldedRun | undefined;
  readonly ceiling: Ceiling;
  readonly spent: number | null;
  readonly known: boolean;
  readonly over: boolean;
  readonly pct: number;
  readonly pctText: string;
}

interface AgentRow {
  readonly key: string;
  readonly isHash: boolean;
  readonly runs: number;
  readonly tokens: number;
  readonly spend: number;
  readonly complete: boolean;
}

interface DayRow {
  readonly day: string;
  readonly runs: number;
  readonly tokens: number;
  readonly spend: number;
  readonly complete: boolean;
}

const WHY_CROSSED =
  'The ceiling that was actually in force, read from the run’s own BudgetExceeded event.';
const WHY_UNKNOWN =
  'This build has no agent registry to read a declared ceiling from — only a ceiling a run ' +
  'actually crossed enters the log, so an uncrossed ceiling reads unknown here.';

/**
 * Spend: the burn-down against declared budgets, by-agent and by-day ledgers, and the activity
 * histogram — all of it folded from the real log, deliberately. `GET /v1/runs` carries usage and
 * step counts now, but never the per-call model id a dollar figure needs, so every figure here
 * reads a run's full log through {@link CostFoldService} (the shared wasm fold) and prices it
 * through the shared table. A model absent from that table degrades the whole figure to
 * tokens-only rather than quoting a number that silently omits spend; a log that could not be
 * read in full degrades further, honestly, rather than guessing at the rest.
 */
@Component({
  selector: 'bridge-spend',
  imports: [RunRef],
  templateUrl: './spend.html',
})
export class Spend implements AfterViewInit {
  private readonly runsService = inject(RunsService);
  private readonly viewService = inject(ViewService);
  private readonly costFold = inject(CostFoldService);

  /** The histogram is generated markup ({@link renderActivityHtml}), written straight to the
   * `<svg>` element's own `.innerHTML` — an Angular `[innerHTML]` TEMPLATE BINDING goes through
   * `DomSanitizer`, which sanitizes as HTML, not SVG: it parses the fragment outside any `<svg>`
   * context and strips or mis-namespaces `<g>`/`<rect>`/`<title>` on the way back out, so the
   * chart would sanitize itself into nothing. Writing to the real element's `.innerHTML` via
   * `ElementRef` bypasses the template binding (and its sanitizer) entirely — the same split
   * `event-model.ts`'s renderer and the Inspector's own timeline/strip wiring already use. */
  @ViewChild('activityEl') private activityEl?: ElementRef<SVGSVGElement>;

  readonly listLoaded = computed(() => this.runsService.lastLoadedAt() !== undefined);
  readonly rows = computed<RunRow[]>(() => this.runsService.runs().map(toRunRow));

  private readonly folded = signal<ReadonlyMap<string, FoldedRun>>(new Map());
  readonly folding = signal(true);

  /** Cold-load COLLAPSED (audit item 15): the breakdown duplicates a burn-down row the operator
   * already sees, so it earns its width only on request. */
  readonly panelOpen = signal(false);
  readonly spendSel = signal<string | undefined>(undefined);

  readonly meterRows = computed<MeterRow[]>(() => {
    const f = this.folded();
    return this.rows().map((row) => {
      const folded = f.get(row.id);
      const ceiling = ceilingFor(folded?.state);
      const spent = folded?.cost.complete ? (folded.cost.usd ?? 0) : null;
      const known = ceiling.usd !== null;
      const over = known && spent !== null && spent >= (ceiling.usd as number);
      const pct = known && spent !== null ? Math.min(100, (spent / (ceiling.usd as number)) * 100) : 0;
      const pctText =
        known && spent !== null ? (pct < 1 && spent > 0 ? '<1%' : Math.round(pct) + '%') : '—';
      return { row, folded, ceiling, spent, known, over, pct, pctText };
    });
  });

  readonly selectedFolded = computed<FoldedRun | undefined>(() => {
    const id = this.spendSel();
    return id ? this.folded().get(id) : undefined;
  });
  readonly selectedRow = computed<RunRow | undefined>(() =>
    this.rows().find((r) => r.id === this.spendSel()),
  );
  readonly selectedCeiling = computed<Ceiling>(() => ceilingFor(this.selectedFolded()?.state));

  readonly byAgent = computed<AgentRow[]>(() => {
    const agg = new Map<string, { runs: number; tokens: number; spend: number; complete: boolean }>();
    for (const f of this.folded().values()) {
      const key = f.agent ?? '(no agent recorded)';
      const a = agg.get(key) ?? { runs: 0, tokens: 0, spend: 0, complete: true };
      a.runs++;
      a.tokens += f.usage.inputTokens + f.usage.outputTokens;
      if (f.cost.complete) a.spend += f.cost.usd ?? 0;
      else a.complete = false;
      agg.set(key, a);
    }
    return [...agg.entries()]
      .sort((a, b) => b[1].tokens - a[1].tokens)
      .map(([key, a]) => ({ key, isHash: /^sha256:/.test(key), ...a }));
  });

  readonly byDay = computed<DayRow[]>(() => {
    const agg = new Map<string, { runs: Set<string>; tokens: number; spend: number; complete: boolean }>();
    for (const f of this.folded().values()) {
      for (const e of f.events) {
        if (e.kind !== 'ModelCallCompleted') continue;
        const usage = (e.payload['usage'] ?? {}) as Record<string, unknown>;
        const inTok = Number(usage['input_tokens'] ?? 0);
        const outTok = Number(usage['output_tokens'] ?? 0);
        const day = e.recordedAt.slice(0, 10);
        const d = agg.get(day) ?? { runs: new Set<string>(), tokens: 0, spend: 0, complete: true };
        d.runs.add(f.id);
        d.tokens += inTok + outTok;
        const model = String(e.payload['model'] ?? '');
        const priced = callCost(model, inTok, outTok);
        if (priced !== null) d.spend += priced;
        else d.complete = false;
        agg.set(day, d);
      }
    }
    return [...agg.entries()]
      .sort((a, b) => (a[0] < b[0] ? -1 : 1))
      .map(([day, d]) => ({ day, runs: d.runs.size, tokens: d.tokens, spend: d.spend, complete: d.complete }));
  });

  readonly activityWindow = computed<ActivityWindow | undefined>(() =>
    bucketEvents([...this.folded().values()].map((f) => f.events)),
  );

  /** The `hour:` value currently applied on Runs, if any — read from the one shared `?q=` signal,
   * so a bucket that is the live selection washes without a second filter state. */
  readonly selectedHourTerm = computed<string | undefined>(() => {
    const m = this.viewService.query().match(/(?:^|\s)hour:(\S+)/i);
    return m ? m[1].toLowerCase() : undefined;
  });

  readonly activityDesc = computed(() => {
    const win = this.activityWindow();
    return win ? activityDescText(win, (hr) => this.lastActiveCount(hr)) : '';
  });
  readonly pickableHours = computed(() => {
    const win = this.activityWindow();
    if (!win) return 0;
    let n = 0;
    for (let i = 0; i < win.nBuckets; i++) {
      const b = win.buckets[i];
      if (b.model + b.tool + b.other && this.lastActiveCount(hourTermOf(win.lo, i))) n++;
    }
    return n;
  });

  constructor() {
    void this.load();
    // Every view section is mounted for the app's whole life (the shell toggles `hidden`, never
    // destroys), so a plain `effect` over `runsService.runs()` would pay the fold cost — a full
    // log read per run — on EVERY page load, for every view, whether or not anyone ever looks at
    // Spend. Gate it on Spend actually being the active view: the same "earn the cost on request"
    // rule the collapsed cpanel default already states for this view.
    effect(() => {
      const rows = this.runsService.runs();
      if (this.viewService.view() !== 'spend') return;
      void this.refold(rows.map((r) => ({ id: r.run, eventCount: r.eventCount })));
    });
    // the histogram: re-render whenever the fold or the live hour-filter selection changes
    effect(() => {
      const win = this.activityWindow();
      this.selectedHourTerm();
      this.renderActivity(win);
    });
  }

  ngAfterViewInit(): void {
    // first paint of whatever the fold has already produced, matching the Inspector's own
    // ngAfterViewInit — the constructor's effect may have run before the view existed
    this.renderActivity(this.activityWindow());
  }

  private renderActivity(win: ActivityWindow | undefined): void {
    const el = this.activityEl?.nativeElement;
    if (!el) return;
    el.innerHTML = win
      ? renderActivityHtml(win, (hr) => this.lastActiveCount(hr), this.selectedHourTerm())
      : '';
  }

  private async load(): Promise<void> {
    try {
      await this.runsService.refresh();
    } catch {
      /* error surfaced via runsService.error, matching Runs' and Inbox's own load() */
    }
  }

  private refoldToken = 0;
  private async refold(rows: readonly { id: string; eventCount: number }[]): Promise<void> {
    const token = ++this.refoldToken;
    if (rows.length === 0) {
      this.folded.set(new Map());
      this.folding.set(false);
      return;
    }
    this.folding.set(true);
    const entries = await Promise.all(
      rows.map(async (r) => {
        try {
          return [r.id, await this.costFold.foldRun(r.id, r.eventCount)] as const;
        } catch {
          return [r.id, undefined] as const; // a genuinely unreadable log — honestly absent
        }
      }),
    );
    if (token !== this.refoldToken) return; // superseded by a fresher list
    const map = new Map<string, FoldedRun>();
    for (const [id, f] of entries) if (f) map.set(id, f);
    this.folded.set(map);
    this.folding.set(false);
  }

  private lastActiveCount(hourTerm: string): number {
    return this.rows().filter((r) => hourKey(r.last).toLowerCase() === hourTerm.toLowerCase()).length;
  }

  // ── template helpers ──────────────────────────────────────────────────────
  group = groupOf;
  label = labelOf;
  int = int;
  usd = usd;

  whyFor(ceiling: Ceiling): string {
    return ceiling.src === 'crossed' ? WHY_CROSSED : WHY_UNKNOWN;
  }

  toggleBreakdown(id: string): void {
    this.spendSel.update((cur) => (cur === id ? undefined : id));
    this.openPanel();
  }
  openRun(id: string): void {
    this.viewService.openRun(id);
  }

  pickBucket(hourTerm: string): void {
    if (this.lastActiveCount(hourTerm) === 0) return; // nothing to land on — say nothing
    this.viewService.filterRuns('hour:' + hourTerm);
  }

  /** The buckets are generated markup ({@link renderActivityHtml}), so their click/Enter/Space
   * handling is delegated from the `<svg>` element the binding lives on — the same split
   * `event-model.ts`'s renderer and the Inspector's wiring use. */
  onActivityClick(e: Event): void {
    const g = (e.target as Element).closest('.hbucket');
    const hour = g?.getAttribute('data-hour');
    if (hour) this.pickBucket(hour);
  }
  onActivityKeydown(e: KeyboardEvent): void {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const g = (e.target as Element).closest('.hbucket');
    const hour = g?.getAttribute('data-hour');
    if (!hour) return;
    e.preventDefault(); // Space would otherwise scroll the view
    this.pickBucket(hour);
  }

  closePanel(): void {
    this.panelOpen.set(false);
    focusWhenRendered('.cpanel-tab[data-panel="spend"]');
  }
  openPanel(): void {
    this.panelOpen.set(true);
    focusWhenRendered('.cpanel[data-panel="spend"] .cpanel-x');
  }

  async copy(value: string, ev: Event): Promise<void> {
    ev.stopPropagation();
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      /* clipboard blocked — no fallback theater */
    }
  }
}
