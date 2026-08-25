import { NgClass, NgTemplateOutlet } from '@angular/common';
import {
  Component,
  ElementRef,
  type Signal,
  ViewChild,
  computed,
  effect,
  inject,
  signal,
} from '@angular/core';

import {
  AgentRegistryService,
  GraphRunService,
  RunsService,
  createConnectionStateMachine,
} from '../../core/api';
import { ForkIntentService } from '../../core/fork-intent';
import { ViewService } from '../../core/view';
import { focusWhenRendered } from '../../core/focus';
import { SERVER_CAPABILITIES } from '../inspector/capability';
import {
  FIELDS,
  MENU_KEYS,
  VALUE_KEYS,
  type Term,
  matchesAll,
  parseQuery,
} from './filter-vocab';
import {
  type AgentIdentity,
  type Group,
  type RunRow,
  age,
  agentIdentity,
  durationLabel,
  groupOf,
  hourKey,
  isHash,
  isWaiting,
  labelOf,
  toRunRow,
} from './run-model';
import {
  type RunGroup,
  buildRunGroups,
  canCollapse as clusterCanCollapse,
} from './run-groups';

type SortKey = 'status' | 'id' | 'events' | 'age' | 'agent';
/** The Runs ledger's grouping mode: persisted, default Grouped. */
type GroupMode = 'grouped' | 'flat';
const GROUP_MODE_KEY = 'salvor.groupMode';

interface AcItem {
  readonly id: string;
  readonly primary: string;
  readonly secondary: string;
  readonly term?: string;
  readonly key?: string;
}

const WHY_COST =
  'GET /v1/runs now carries usage, but not the model id of each call, and the price table is ' +
  'keyed by model. Dollars need the per-call model id from each ModelCallCompleted, so they can ' +
  'still only be folded from the log. Tokens are shown here; see Spend, which pays that cost ' +
  'deliberately.';
const WHY_UNREADABLE =
  'GET /v1/runs omits this field for a run whose log it could not fold. An omission is not a ' +
  'zero: the endpoint declines to answer rather than report a count it does not have.';

/** Read the persisted group mode; defaults to Grouped, including when storage is unavailable
 * (private browsing, a locked-down embed); a read failure is never a reason to crash the view. */
function readGroupMode(): GroupMode {
  try {
    return localStorage.getItem(GROUP_MODE_KEY) === 'flat' ? 'flat' : 'grouped';
  } catch {
    return 'grouped';
  }
}
function writeGroupMode(mode: GroupMode): void {
  try {
    localStorage.setItem(GROUP_MODE_KEY, mode);
  } catch {
    /* persistence is a nicety, not a requirement: the in-memory signal still governs this session */
  }
}

/** The minute-one teaching strip is shown until the reader dismisses it; the dismissal persists so
 * a returning operator is not lectured every visit. A read failure (private browsing, a locked-down
 * embed) is never a reason to crash the view, and it defaults to SHOWN: the teaching is the safe
 * fallback, not the suppression. */
const INTRO_KEY = 'salvor.introDismissed';
function readIntroDismissed(): boolean {
  try {
    return localStorage.getItem(INTRO_KEY) === '1';
  } catch {
    return false;
  }
}
function writeIntroDismissed(dismissed: boolean): void {
  try {
    if (dismissed) localStorage.setItem(INTRO_KEY, '1');
    else localStorage.removeItem(INTRO_KEY);
  } catch {
    /* persistence is a nicety; the in-memory signal still governs this session */
  }
}

/**
 * The Runs ledger, complete: the 6-column table with the state rail / zebra / attention wash /
 * outline-selection channels, attention-first sort, the token filter field (pills, `@` combobox,
 * honest refusal pill), the fold-derived health strip, and the docked run-detail panel.
 *
 * It reads {@link RunsService} (wrapping `GET /v1/runs`). The list carries
 * `usage`, `step_count` and `agent_def_hash`, so the detail panel renders those as REAL values;
 * only `cost` remains a hyphen, with WHY_COST verbatim.
 */
@Component({
  selector: 'bridge-runs',
  imports: [NgClass, NgTemplateOutlet],
  templateUrl: './runs.html',
})
export class Runs {
  private readonly runsService = inject(RunsService);
  private readonly viewService = inject(ViewService);
  private readonly agentRegistry = inject(AgentRegistryService);
  private readonly graphRuns = inject(GraphRunService);
  private readonly forkIntent = inject(ForkIntentService);
  private readonly caps = inject(SERVER_CAPABILITIES);

  /** run_id → graph_hash, populated lazily when a GRAPH run is selected in the detail panel (its
   * hash lives behind GET /v1/runs/{id}/graph, never on the list row; see agentIdentity's 'graph'
   * rendering). A run already fetched stays cached; a failed fetch simply leaves the panel showing
   * the plain "graph run" marker, no fabricated hash. */
  private readonly graphHashes = signal<ReadonlyMap<string, string>>(new Map());

  /** The Runs list is a REST snapshot: the pill's honest state here is Snapshot, driven by the
   * connection machine (no public setter; toSnapshot is called on each fetch). */
  private readonly conn = createConnectionStateMachine();
  readonly connState = this.conn.state;

  @ViewChild('qInput') private qInputRef?: ElementRef<HTMLInputElement>;

  // ── the query, as tokens (the truth) + the pending text being typed ──
  private readonly qTok = signal<string[]>([]);
  private readonly pendingText = signal<string>('');
  private readonly terms = signal<Term[]>([]);
  readonly qErr = signal<string | undefined>(undefined);
  readonly qInvalid = computed(() => this.qErr() !== undefined);

  // ── autocomplete / @ menu ──
  readonly acMode = signal<'values' | 'keys'>('values');
  readonly acItems = signal<AcItem[]>([]);
  readonly acIdx = signal<number>(-1);
  readonly acOpen = signal<boolean>(false);

  // ── table state ──
  private readonly sortKey = signal<SortKey>('status');
  private readonly sortDir = signal<-1 | 1>(-1);
  readonly runSel = signal<string | undefined>(undefined);
  readonly panelOpen = signal<boolean>(true);
  readonly dotKeyOpen = signal<boolean>(false);

  /** The minute-one teaching strip above the table: open until dismissed, dismissal persisted. */
  readonly introOpen = signal<boolean>(!readIntroDismissed());
  private seeded = false;

  // ── GROUPING: a persisted mode, default Grouped. Collapse state is
  // per-key and in-memory: a fresh load starts fully expanded, the honest default (nothing
  // hidden until the operator collapses a group themselves). ──
  readonly groupMode = signal<GroupMode>(readGroupMode());
  private readonly collapsed = signal<ReadonlySet<string>>(new Set());

  readonly loading = this.runsService.loading;
  readonly loadError = this.runsService.error;
  readonly listLoaded = computed(() => !this.loading() && this.runsService.lastLoadedAt() !== undefined);

  // One `now` per fold so every row's stalled derivation reads the same clock
  // edge (and so `.map` never leaks the array index in as `now`).
  readonly rows: Signal<RunRow[]> = computed(() => {
    const now = Date.now();
    return this.runsService.runs().map((s) => toRunRow(s, now));
  });

  /** hash → resolved name, fed by {@link AgentRegistryService}. */
  readonly agentNames = this.agentRegistry.names;

  readonly visible = computed<RunRow[]>(() => {
    const t = this.terms();
    const names = this.agentNames();
    const filtered = this.rows().filter((r) => matchesAll(r, t, names));
    const key = this.sortKey();
    const dir = this.sortDir();
    return [...filtered].sort((a, b) => {
      const av = this.sortVal(a, key, names);
      const bv = this.sortVal(b, key, names);
      const cmp = av < bv ? -1 : av > bv ? 1 : 0;
      return cmp * dir || this.statusRank(b) - this.statusRank(a);
    });
  });

  /** GROUPED-mode presentation: the same {@link visible} rows, clustered worst-first. Attention
   * survives grouping: any group holding a waiting run floats to the top, exactly as a waiting
   * run would in the flat list (see `run-groups.ts#buildRunGroups`). */
  readonly groups: Signal<RunGroup[]> = computed(() => buildRunGroups(this.visible(), this.agentNames()));

  readonly counts = computed(() => {
    const n = { waiting: 0, progress: 0, terminal: 0 } as Record<Group, number>;
    for (const r of this.rows()) n[groupOf(r.status, r.waitingOn)]++;
    return n;
  });
  readonly runningCount = computed(() => this.rows().filter((r) => r.status === 'running').length);
  readonly failedCount = computed(() => this.rows().filter((r) => r.status === 'failed').length);
  readonly totalCount = computed(() => this.rows().length);
  readonly asOf = computed(() => {
    const iso = this.runsService.lastLoadedAt();
    return iso ? iso.replace('T', ' ').replace(/:\d\d\.\d+Z$/, 'Z') : '-';
  });
  readonly countLabel = computed(() => {
    const v = this.visible().length;
    const total = this.totalCount();
    if (!this.listLoaded()) return 'Loading…';
    return v === total ? `${total} runs` : `${v} of ${total} runs`;
  });

  readonly selectedRow = computed<RunRow | undefined>(() =>
    this.rows().find((r) => r.id === this.runSel()),
  );

  readonly waitingActionable = computed(() => this.counts().waiting);

  /** The strip's honest gestalt. A run that is over budget or awaiting reconciliation is trouble,
   * not calm: it sits in the waiting group, so a soothing "0 failed" beside it must never let the
   * strip read all-clear. `attention` whenever any run is waiting on a person OR has failed;
   * `clear` only when the census is genuinely quiet; `empty` before any run exists. */
  readonly healthState = computed<'attention' | 'clear' | 'empty'>(() => {
    if (!this.totalCount()) return 'empty';
    return this.counts().waiting > 0 || this.failedCount() > 0 ? 'attention' : 'clear';
  });

  constructor() {
    // read a filter carried in ?q= on entry
    const initial = this.viewService.query();
    if (initial) {
      this.qTok.set(initial.trim().split(/\s+/).filter(Boolean));
      this.applyQuery();
    } else {
      this.applyQuery();
    }
    void this.load();

    // cold-load: seed the rail with the top waiting run, once (mode-aware since item 15b made
    // Grouped the default DISPLAY order). In grouped mode the top waiting run is the first run of
    // the top-ranked group: groups float worst-first (see run-groups.ts#buildRunGroups), and a
    // group's own runs keep visible()'s attention-sorted order, so a rank-2 (waiting) group's
    // first run is guaranteed to be a waiting run itself. Without this, the seeded selection could
    // point at a row buried under a different group than the one actually on top of the screen.
    effect(() => {
      if (this.seeded || !this.listLoaded()) return;
      const top =
        this.groupMode() === 'grouped'
          ? this.groups()[0]?.runs[0]
          : this.visible().find((r) => isWaiting(r.status, r.waitingOn));
      this.seeded = true;
      if (top) {
        this.runSel.set(top.id);
        this.panelOpen.set(true);
      }
    });

    // keep the Snapshot pill's as-of honest with each fetch
    effect(() => {
      const iso = this.runsService.lastLoadedAt();
      if (iso) this.conn.driver.toSnapshot(iso);
    });

    // AGENT COLUMN (item 15a): resolve every hash-shaped agent_def_hash the list carries. Batched
    // and cached inside AgentRegistryService: a hash already attempted (resolved or not) is never
    // re-fetched. Fire-and-forget: the service's own `names` signal (aliased as `agentNames`
    // above) is what the template and the computed signals above actually read.
    effect(() => {
      const hashes = [
        ...new Set(
          this.rows()
            .map((r) => r.agentDefHash)
            .filter((h): h is string => h !== undefined && isHash(h)),
        ),
      ];
      if (hashes.length) this.agentRegistry.resolve(hashes);
    });

    // GRAPH RUN panel enrichment: a graph run carries no agent_def_hash on the list row, so when
    // one is selected in the detail panel we read its graph_hash from GET /v1/runs/{id}/graph (the
    // one endpoint that carries it) and cache it. Fire-and-forget, cached per run, and silent on
    // failure: the panel keeps its honest "graph run" marker with no hash rather than inventing one.
    effect(() => {
      const r = this.selectedRow();
      if (!r || agentIdentity(r).kind !== 'graph' || this.graphHashes().has(r.id)) return;
      const id = r.id;
      this.graphRuns
        .loadProjection(id)
        .then((p) => this.graphHashes.update((m) => new Map(m).set(id, p.graphHash)))
        .catch(() => {
          /* projection unavailable: the panel keeps the plain "graph run" marker, no fake hash */
        });
    });

    // a filter applied from another view (Spend's hour bucket): Runs is mounted once for the
    // app's whole life, so this is the one channel that reaches it after that initial read
    effect(() => {
      const ext = this.viewService.externalFilter();
      if (!ext) return;
      this.qTok.set(ext.q.trim().split(/\s+/).filter(Boolean));
      this.pendingText.set('');
      this.applyQuery();
    });
  }

  private async load(): Promise<void> {
    try {
      await this.runsService.refresh();
    } catch {
      /* error surfaced via runsService.error; renderListError-equivalent shows in template */
    }
  }

  /** The committed tokens, for the template's pill rendering. */
  qTokens(): string[] {
    return this.qTok();
  }

  /** A plain-cell click opens the run; interactive controls stopPropagation so they never reach
   * here, and a text selection the user is making is never hijacked into a navigation. */
  onRowClick(e: MouseEvent, r: RunRow): void {
    const t = e.target as HTMLElement;
    if (t.closest('.c-caret, .copy, .iact, .status-btn')) return;
    if (String(getSelection() ?? '')) return;
    this.openRun(r.id);
  }

  /** The row's keyboard path to the Inspector (the pointer affordance is the row body). Only fires
   * when the ROW itself holds focus; a child control (the filter pill, the copy button, the panel
   * toggle) handles its own Enter/Space and must not also open the run. Space is prevented from
   * scrolling the page, matching the row's button-like activation. */
  onRowKeydown(e: KeyboardEvent, r: RunRow): void {
    if (e.target !== e.currentTarget) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      this.openRun(r.id);
    }
  }

  // ── query plumbing ──
  getQuery(): string {
    return [...this.qTok(), this.pendingText().trim()].filter(Boolean).join(' ');
  }

  private applyQuery(): void {
    try {
      this.terms.set(parseQuery(this.getQuery()));
      this.qErr.set(undefined);
    } catch (ex) {
      this.qErr.set(ex instanceof Error ? ex.message : String(ex));
      // keep the last good result on screen
    }
    // reflect into ?q= (replaceUrl: a filter is not a history entry)
    this.viewService.setQuery([...this.qTok()].join(' '));
  }

  isStructured(t: string): boolean {
    return /^[a-z_]+[:<>]/i.test(t);
  }
  tokenBad(t: string): boolean {
    try {
      parseQuery(t);
      return false;
    } catch {
      return true;
    }
  }
  pillKey(t: string): string {
    const m = t.match(/^([a-z_]+)([:<>])(.*)$/i);
    return m ? m[1] + m[2] : t;
  }
  pillVal(t: string): string {
    const m = t.match(/^([a-z_]+)([:<>])(.*)$/i);
    return m ? m[3] : '';
  }

  onInput(el: HTMLInputElement): void {
    this.pendingText.set(el.value);
    this.applyQuery();
    if (this.acMode() === 'keys') this.openKeys();
    else this.autocomplete();
  }

  onKeydown(e: KeyboardEvent, el: HTMLInputElement): void {
    if (e.key === '@' && el.value === '' && el.selectionStart === 0) {
      e.preventDefault();
      this.openKeys();
      return;
    }
    if (e.key === 'Escape' && this.acOpen()) {
      e.preventDefault();
      e.stopPropagation();
      this.acClose();
      return;
    }
    if (e.key === 'Escape' && !this.acOpen()) {
      // no menu open: clear the whole query
      e.preventDefault();
      this.clearQuery();
      return;
    }
    if (this.acOpen() && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
      e.preventDefault();
      this.acMove(e.key === 'ArrowDown' ? 1 : -1);
      // Reflect the active option onto the input now, not on the next change-detection pass, so a
      // screen reader hears the move immediately as the arrow key is pressed.
      const active = this.activeDescendant();
      if (active) el.setAttribute('aria-activedescendant', active);
      return;
    }
    if (e.key === ' ' || e.key === 'Tab') {
      if (this.acOpen() && this.acIdx() >= 0) {
        e.preventDefault();
        this.acTake(el);
        return;
      }
      if (e.key === ' ' && el.value.trim()) {
        e.preventDefault();
        this.commitPending(el);
        this.applyQuery();
      }
      return;
    }
    if (e.key === 'Enter') {
      if (this.acOpen() && this.acIdx() >= 0) {
        e.preventDefault();
        this.acTake(el);
        return;
      }
      if (el.value.trim()) {
        e.preventDefault();
        this.commitPending(el);
        this.applyQuery();
      }
      // otherwise: open the first run (keyboard contract)
      const first = this.visible()[0];
      if (first && !el.value.trim()) this.openRun(first.id);
      return;
    }
    if (e.key === 'Backspace' && el.selectionStart === 0 && el.selectionEnd === 0 && this.qTok().length) {
      e.preventDefault();
      this.qTok.update((t) => t.slice(0, -1));
      this.applyQuery();
      return;
    }
  }

  private commitPending(el: HTMLInputElement): boolean {
    const t = el.value.trim();
    if (!t) return false;
    this.qTok.update((toks) => [...toks, t]);
    el.value = '';
    this.pendingText.set('');
    return true;
  }

  private suggestions(): AcItem[] {
    const t = this.pendingText();
    const m = t.match(new RegExp(`^(${VALUE_KEYS.join('|')}):(.*)$`, 'i'));
    if (!m) return [];
    const key = m[1].toLowerCase();
    const part = m[2].toLowerCase();
    const read = FIELDS[key];
    if (!read) return [];
    const names = this.agentNames();
    const countByValue = new Map<string, number>();
    for (const r of this.rows()) {
      const v = String(read(r, names));
      countByValue.set(v, (countByValue.get(v) ?? 0) + 1);
    }
    return [...countByValue.keys()]
      .filter((v) => v.startsWith(part))
      .sort()
      .map((v, i) => ({
        id: `qac-${i}`,
        primary: v,
        secondary: `${countByValue.get(v)} run${countByValue.get(v) === 1 ? '' : 's'}`,
        term: `${key}:${v}`,
      }));
  }

  autocomplete(): void {
    const items = this.suggestions();
    this.acIdx.set(-1);
    if (!items.length) {
      this.acClose();
      return;
    }
    this.acMode.set('values');
    this.acItems.set(items);
    this.acOpen.set(true);
  }

  openKeys(): void {
    this.acMode.set('keys');
    const part = this.pendingText().toLowerCase().replace(/^@/, '');
    const items: AcItem[] = MENU_KEYS.map((v) => ({ insert: v.key + v.op, desc: v.desc }))
      .filter((it) => !part || it.insert.toLowerCase().startsWith(part))
      .map((it, i) => ({ id: `qac-${i}`, primary: it.insert, secondary: it.desc, key: it.insert }));
    this.acIdx.set(-1);
    if (!items.length) {
      this.acClose();
      return;
    }
    this.acItems.set(items);
    this.acOpen.set(true);
  }

  acMove(d: number): void {
    const items = this.acItems();
    if (!items.length) return;
    const from = this.acIdx();
    const idx = (from + d + items.length) % items.length;
    this.acIdx.set(idx);
  }

  acTake(el: HTMLInputElement): boolean {
    const item = this.acItems()[this.acIdx()];
    if (!item) return false;
    if (this.acMode() === 'keys' && item.key) {
      this.chooseKey(item.key, el);
      return true;
    }
    if (item.term) {
      el.value = item.term;
      this.pendingText.set(item.term);
      this.commitPending(el);
      this.acClose();
      this.applyQuery();
    }
    return true;
  }

  chooseKey(insert: string, el: HTMLInputElement): void {
    el.value = insert;
    this.pendingText.set(insert);
    this.acMode.set('values');
    this.autocomplete();
    el.focus();
  }

  onAcMousedown(e: MouseEvent, item: AcItem, el: HTMLInputElement): void {
    e.preventDefault();
    if (this.acMode() === 'keys' && item.key) {
      this.chooseKey(item.key, el);
      return;
    }
    if (item.term) {
      el.value = item.term;
      this.pendingText.set(item.term);
      this.commitPending(el);
      this.acClose();
      this.applyQuery();
      el.focus();
    }
  }

  acClose(): void {
    this.acOpen.set(false);
    this.acMode.set('values');
    this.acIdx.set(-1);
  }

  onBlur(): void {
    setTimeout(() => this.acClose(), 120);
  }

  activeDescendant(): string | null {
    const i = this.acIdx();
    return this.acOpen() && i >= 0 ? `qac-${i}` : null;
  }

  clearQuery(): void {
    this.qTok.set([]);
    this.pendingText.set('');
    if (this.qInputRef) this.qInputRef.nativeElement.value = '';
    this.acClose();
    this.applyQuery();
  }

  delToken(i: number): void {
    this.qTok.update((t) => t.filter((_, idx) => idx !== i));
    this.applyQuery();
    this.qInputRef?.nativeElement.focus();
  }

  // ── group chips + status labels: one toggle behind every affordance ──
  toggleTerm(term: string): void {
    const family = term.slice(0, term.indexOf(':') + 1);
    const parts = this.getQuery().trim().split(/\s+/).filter(Boolean);
    const at = parts.indexOf(term);
    if (at >= 0) parts.splice(at, 1);
    else {
      const other = parts.findIndex((p) => p.startsWith(family));
      if (other >= 0) parts.splice(other, 1);
      parts.push(term);
    }
    this.qTok.set(parts);
    this.pendingText.set('');
    if (this.qInputRef) this.qInputRef.nativeElement.value = '';
    this.applyQuery();
  }

  chipPressed(term: string): boolean {
    return this.getQuery().toLowerCase().split(/\s+/).includes(term);
  }

  // ── sorting ──
  sortBy(key: SortKey): void {
    if (this.sortKey() === key) this.sortDir.update((d) => (d === 1 ? -1 : 1));
    else this.sortDir.set(key === 'id' ? 1 : -1);
    this.sortKey.set(key);
  }
  ariaSort(key: SortKey): 'ascending' | 'descending' | 'none' {
    if (this.sortKey() !== key) return 'none';
    return this.sortDir() === 1 ? 'ascending' : 'descending';
  }
  private sortVal(r: RunRow, key: SortKey, names: ReadonlyMap<string, string>): number | string {
    switch (key) {
      case 'status':
        return this.statusRank(r);
      case 'id':
        return r.id;
      case 'events':
        return r.eventCount;
      case 'age':
        return -(r.last ? Date.parse(r.last) : 0);
      case 'agent':
        return agentIdentity(r, names).text.toLowerCase();
    }
  }
  private statusRank(r: RunRow): number {
    return isWaiting(r.status, r.waitingOn) ? 2 : r.status === 'failed' ? 1 : 0;
  }

  // ── row classes / cells ──
  rowClasses(r: RunRow, i: number): Record<string, boolean> {
    return {
      z: i % 2 === 1,
      [`g-${groupOf(r.status, r.waitingOn)}`]: true,
      attention: isWaiting(r.status, r.waitingOn),
      'is-failed': r.status === 'failed',
      'is-done': r.status === 'completed',
      'is-sel': this.runSel() === r.id,
    };
  }
  statusClass(state: string): string {
    return `status status-btn s-${state}`;
  }
  group(state: string): Group {
    return groupOf(state);
  }
  label(state: string): string {
    return labelOf(state);
  }
  /** "overdue by 2h", for a sleeping row whose wake_at has passed (see run-model.ts#overdueOf);
   *  callers only reach this after checking `r.overdue`, so an absent `overdueSeconds` here would
   *  be the rare case a source says overdue but can't say for how long. */
  overdueLabel(r: RunRow): string {
    return r.overdueSeconds !== undefined ? durationLabel(r.overdueSeconds) : '';
  }
  short(id: string): string {
    return id.slice(0, 8);
  }
   age(iso: string | undefined): string {
    return age(iso);
  }
  // ── AGENT COLUMN (item 15a): replaces the old "Last event" cell; see run-model.ts#agentIdentity
  // for the three honest renderings this reads. A resolved NAME's title reveals its hash and
  // provenance; a truncated LABEL's title reveals its full string: the CSS truncates visually
  // (max-width + ellipsis), nothing the cell shows is ever unrecoverable. ──
  identity(r: RunRow): AgentIdentity {
    return agentIdentity(r, this.agentNames());
  }
  agentCellTitle(id: AgentIdentity): string {
    if (id.kind === 'none') return 'No agent recorded on this run';
    if (id.kind === 'graph')
      return 'A graph run: its log has no single agent_def_hash; expand the row for its graph_hash';
    if (id.kind === 'name') return `${id.hash}: resolved via GET /v1/agents/{hash}`;
    return id.text;
  }
  /** The graph_hash of a selected GRAPH run once its projection has loaded (see the panel effect). */
  graphHashOf(r: RunRow): string | undefined {
    return this.graphHashes().get(r.id);
  }

  // ── FORK (audit item 6): a list row names no node, so this goes through the ONE fork door and
  // lands the canvas in its node-picking state; the operator points at the fork point there. ──
  forkOffered(r: RunRow): boolean {
    return this.caps().fork && agentIdentity(r).kind === 'graph';
  }
  forkRun(runId: string): void {
    this.forkIntent.request(runId);
    this.viewService.go('workflows');
  }

  // ── GROUPING (item 15b) ──
  setGroupMode(mode: GroupMode): void {
    if (this.groupMode() === mode) return;
    this.groupMode.set(mode);
    writeGroupMode(mode);
  }
  groupModePressed(mode: GroupMode): boolean {
    return this.groupMode() === mode;
  }
  /** WAITING RUNS MUST NOT HIDE: the same predicate the template disables the toggle with. */
  groupCanCollapse(g: RunGroup): boolean {
    return clusterCanCollapse(g);
  }
  groupCollapsed(g: RunGroup): boolean {
    return this.groupCanCollapse(g) && this.collapsed().has(g.key);
  }
  toggleGroupCollapse(g: RunGroup): void {
    if (!this.groupCanCollapse(g)) return; // enforced at the control: a waiting group cannot hide
    this.collapsed.update((set) => {
      const next = new Set(set);
      next.has(g.key) ? next.delete(g.key) : next.add(g.key);
      return next;
    });
  }
  /** The header's worst-first aggregate: a waiting count when present (the signal that must never
   * hide), otherwise the plainest true summary of the group's distinct states. */
  groupAggregate(g: RunGroup): string {
    if (g.waiting) return `${g.waiting} waiting on you`;
    return g.states.map((s) => labelOf(s)).join(' · ');
  }
  /** The header rail gets its OWN classes (`g-waiting`/`g-failed`, not the run row's own
   * `attention`/`is-failed`) deliberately: several existing specs scope `tr.attention` /
   * `tr.is-failed` to mean "a waiting/failed RUN row" (e.g. the cold-load-seed acceptance check,
   * which reads the first `tr.attention` to find the top waiting RUN). Reusing those classes on a
   * `tr.grp-head` (which carries no `data-id` at all) silently broke that query. Same amber/red
   * rail, same worst-first meaning, disjoint vocabulary. */
  groupRowClasses(g: RunGroup): Record<string, boolean> {
    return {
      'g-waiting': g.waiting > 0,
      'g-failed': g.waiting === 0 && g.rank === 1,
    };
  }
  groupKindTag(g: RunGroup): string {
    return g.kind === 'build' || g.kind === 'agent' ? g.kind : '';
  }

  // ── detail panel ──
  selectDetail(id: string): void {
    this.runSel.update((cur) => (cur === id ? undefined : id));
    this.panelOpen.set(true);
  }
  isSelected(id: string): boolean {
    return this.runSel() === id;
  }
  // The dock hand-off: opening focuses the panel's dismiss control and closing focuses the
  // restore tab, so keyboard focus is never stranded on a control that just disappeared.
  closePanel(): void {
    this.panelOpen.set(false);
    focusWhenRendered('.cpanel-tab[data-panel="runs"]');
  }
  openPanel(): void {
    this.panelOpen.set(true);
    focusWhenRendered('.cpanel[data-panel="runs"] .cpanel-x');
  }
  panelSub(): string {
    const r = this.selectedRow();
    return r ? `${labelOf(r.status)} · ${r.eventCount} events` : 'Nothing selected';
  }
  panelTitle(): string {
    const r = this.selectedRow();
    return r ? r.id.slice(0, 8) : 'Run detail';
  }
  has(v: unknown): boolean {
    return v !== undefined && v !== null;
  }
  int(n: number): string {
    return n.toLocaleString('en-US');
  }
  hashShort(h: string): string {
    return 'sha256:' + h.replace(/^sha256:/, '').slice(0, 8);
  }
  readonly whyCost = WHY_COST;
  readonly whyUnreadable = WHY_UNREADABLE;

  // ── navigation ──
  openRun(id: string): void {
    this.viewService.openRun(id);
  }
  goInbox(): void {
    this.viewService.go('inbox');
  }

  /** Whether this run is waiting on a human: the panel shows the inbox signpost only for these. */
  waiting(r: RunRow): boolean {
    return isWaiting(r.status, r.waitingOn);
  }
  /** The signpost's label, matching the Inspector amber-banner precedent per waiting state. The
   * Inbox stays the single action surface; this button only routes to the run's card there. */
  inboxAction(state: string): string {
    switch (state) {
      case 'needs_reconciliation':
        return 'Resolve in inbox';
      case 'suspended':
        return 'Approve in inbox';
      case 'budget_exceeded':
        return 'Raise limit in inbox';
      case 'stalled':
        // A stalled run has no in-app action: its driver is an external host
        // process. The signpost routes to the Inbox card's guidance, never a
        // fake fix button.
        return 'See in inbox';
      default:
        return 'Open in inbox';
    }
  }
  openInInbox(id: string): void {
    // Deselect the run first: the signpost lives in this run-detail panel, and once we route to the
    // Inbox the whole Runs section goes `[hidden]`; a still-selected panel would leave the very
    // same `[data-inbox-run]` button sitting in the hidden DOM, a duplicate of the action the
    // operator just took. Clearing the selection removes it, so the Inbox card is the single
    // action affordance the arrival lands on.
    this.runSel.set(undefined);
    this.viewService.openInboxCard(id);
  }
  toggleDotKey(): void {
    this.dotKeyOpen.update((v) => !v);
  }

  /** Dismiss the minute-one strip and remember it. The reopen affordance stays, so this is a
   * fold-away, never a one-way door. */
  dismissIntro(): void {
    this.introOpen.set(false);
    writeIntroDismissed(true);
    focusWhenRendered('#intro-reopen');
  }
  /** Bring the strip back: the honest re-entry path from the "i" affordance. */
  reopenIntro(): void {
    this.introOpen.set(true);
    writeIntroDismissed(false);
    focusWhenRendered('#intro-dismiss');
  }

  async copy(value: string, ev: Event): Promise<void> {
    ev.stopPropagation();
    try {
      await navigator.clipboard.writeText(value);
    } catch {
      /* clipboard blocked: no fallback theater */
    }
  }
}
