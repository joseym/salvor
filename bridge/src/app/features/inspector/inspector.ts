import {
  AfterViewInit,
  Component,
  DestroyRef,
  ElementRef,
  ViewChild,
  computed,
  effect,
  inject,
  signal,
} from '@angular/core';
import type { SalvorEvent } from '@salvor/client';

import { RunEventsService, type RunEventsChannel } from '../../core/api';
import { PillService } from '../../core/pill';
import { RunsService } from '../../core/api';
import { ViewService } from '../../core/view';
import { groupOf, labelOf } from '../runs/run-model';
import { SERVER_CAPABILITIES, forkOffered } from './capability';
import { KINDS, clock, renderStripHtml, renderTimelineHtml } from './event-model';
import { esc } from './json-hi';
import { type CostTotal, int, usd } from './pricing';
import {
  agentOf,
  costOfPrefix,
  isHash,
  isTerminalState,
  isWaitingState,
  pendingLabel,
  statusHtml,
  statusStateOf,
  stepsOf,
} from './state-model';
import { FoldService, type RunStateJson, toWireLog } from './wasm-fold';

type KindKey = 'all' | 'model' | 'tool' | 'context' | 'lifecycle';

const COPY_ICO = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 3.5v-1h-8v8h1"/></svg>`;
const WARN_ICO = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><path d="M8 1.9 15.1 14H0.9z"/><path d="M8 6.2v3.4M8 11.4v.7"/></svg>`;
const FAIL_ICO = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><circle cx="8" cy="8" r="6.4"/><path d="M5.6 5.6l4.8 4.8M10.4 5.6l-4.8 4.8"/></svg>`;

function info(why: string): string {
  return `<button class="info" type="button" title="${esc(why)}" aria-label="${esc(why)}">i</button>`;
}

/**
 * The Inspector: one run read from its log. Timeline over the REAL event stream (S2's
 * {@link RunEventsService}), VIRTUALIZED with `content-visibility` so painting is bounded while
 * every recorded event stays in the DOM and scrub-and-keyboard reachable; the scrubber folds
 * prefixes through the REAL wasm fold (S1, {@link FoldService}); JSON highlighting via the ported
 * `jsonHi`. Hold-on-scrub-back during live SSE appends, the live ticker driven by the channel's
 * connection state, the run-stats hero, the note apparatus, the empty state, and the
 * capability-gated fork offer (off in this build — the server has no fork runtime).
 *
 * The complex, byte-exact DOM (the `.levent` grid, kchip dots, effect badges, highlighted panes,
 * the derived panel) is built as HTML strings and written into stable container refs with
 * delegated event handling — the prototype's architecture, chosen so the DOM matches the suite's
 * contract exactly. Signals drive WHEN each container re-renders; the containers themselves are
 * fixed, so click/pointer wiring is attached once.
 */
@Component({
  selector: 'bridge-inspector',
  templateUrl: './inspector.html',
})
export class Inspector implements AfterViewInit {
  private readonly runEvents = inject(RunEventsService);
  private readonly runsService = inject(RunsService);
  private readonly fold = inject(FoldService);
  private readonly viewService = inject(ViewService);
  private readonly pill = inject(PillService);
  private readonly destroyRef = inject(DestroyRef);
  private readonly caps = inject(SERVER_CAPABILITIES);

  @ViewChild('statsEl') private statsEl?: ElementRef<HTMLElement>;
  @ViewChild('lineageEl') private lineageEl?: ElementRef<HTMLElement>;
  @ViewChild('bandEl') private bandEl?: ElementRef<HTMLElement>;
  @ViewChild('timelineEl') private timelineEl?: ElementRef<HTMLElement>;
  @ViewChild('stripEl') private stripEl?: ElementRef<HTMLElement>;
  @ViewChild('playheadEl') private playheadEl?: ElementRef<HTMLElement>;
  @ViewChild('derivedEl') private derivedEl?: ElementRef<HTMLElement>;
  @ViewChild('liveBarEl') private liveBarEl?: ElementRef<HTMLElement>;
  @ViewChild('forkHereEl') private forkHereEl?: ElementRef<HTMLElement>;
  @ViewChild('panelClose') private panelClose?: ElementRef<HTMLButtonElement>;
  @ViewChild('panelTab') private panelTab?: ElementRef<HTMLButtonElement>;

  readonly KIND_CHIPS: readonly { key: KindKey; label: string }[] = [
    { key: 'all', label: 'All' },
    { key: 'model', label: 'Model' },
    { key: 'tool', label: 'Tool' },
    { key: 'context', label: 'Context' },
    { key: 'lifecycle', label: 'Lifecycle' },
  ];

  // ── reactive state ──
  private readonly channelSig = signal<RunEventsChannel | undefined>(undefined);
  readonly events = signal<SalvorEvent[]>([]);
  readonly prefixN = signal(0);
  private readonly foldReady = signal(false);
  readonly kindFilter = signal<KindKey>('all');
  readonly nodeFilter = signal<string | null>(null);
  readonly allOpen = signal(false);
  readonly panelOpen = signal(true);

  /** The seq of the event that JUST arrived, for the arrive/wash motion. A plain field, not a
   *  signal: it is read at render time and cleared after, never a render trigger of its own. */
  private arrivedSeq: number | null = null;
  private prevLen = 0;
  private liveMode: 'live' | 'held' | 'done' | null = null;

  readonly hasRun = computed(() => this.viewService.runId() !== undefined);
  readonly eventCount = computed(() => this.events().length);
  readonly lastSeq = computed(() => Math.max(0, this.events().length - 1));
  readonly following = computed(() => this.prefixN() === this.events().length);
  readonly readoutLabel = computed(() => `n = ${this.prefixN()} / ${this.events().length}`);

  private readonly wireLog = computed(() => toWireLog(this.events()));

  readonly headGroup = computed<string | null>(() => {
    if (!this.foldReady() || this.events().length === 0) return null;
    const st = this.foldSafe(this.events().length);
    return st ? groupOf(statusStateOf(st.status)) : null;
  });

  readonly logCountLabel = computed(() => {
    const evs = this.events();
    if (evs.length === 0) return '';
    const kf = this.kindFilter();
    const nf = this.nodeFilter();
    if (kf === 'all' && !nf) return `${evs.length} event${evs.length === 1 ? '' : 's'}`;
    const shown = evs.filter(
      (e) => (kf === 'all' || (KINDS[e.kind]?.cat ?? 'context') === kf) && (!nf || e.payload['node'] === nf),
    ).length;
    return `${shown} of ${evs.length} events`;
  });

  readonly foldAllDisabled = computed(
    () => this.events().length === 0 || this.prefixN() === this.events().length,
  );
  readonly foldAllTitle = computed(() =>
    this.foldAllDisabled()
      ? 'The whole log is already folded — the playhead is at the end.'
      : 'Fold every event — return the playhead to the end of the log.',
  );

  constructor() {
    // (1) load the run named in the URL
    effect(() => {
      const id = this.viewService.runId();
      this.loadRun(id);
    });

    // (2) surface the channel's growing event list, holding the playhead on live appends
    effect(() => {
      const ch = this.channelSig();
      if (!ch) {
        this.prevLen = 0;
        this.events.set([]);
        return;
      }
      this.onEvents(ch.events());
    });

    // (3) the connection pill: mirror the stream while the Inspector is the active view
    effect(() => {
      const active = this.viewService.view() === 'inspector';
      const ch = this.channelSig();
      const asOf = this.runsService.lastLoadedAt();
      if (active && ch && this.events().length > 0) {
        this.pill.set(ch.state(), ch.runId);
      } else {
        this.pill.toSnapshot(asOf);
      }
    });

    // (4) full body render (header, timeline, strip) when the log or fold-readiness changes
    effect(() => {
      this.events();
      this.foldReady();
      if (!this.hasRun()) return;
      this.renderHeader();
      this.renderTimeline();
      this.renderStrip();
      this.renderScrubber();
    });

    // (5) scrub-only render (derived panel, dim/cut, playhead, ticks, fork offer)
    effect(() => {
      this.prefixN();
      this.events();
      this.foldReady();
      this.renderScrubber();
    });

    // (6) the live ticker bar
    effect(() => {
      this.channelSig()?.state();
      this.following();
      this.events();
      this.prefixN();
      this.renderLive();
    });

    // (7) the kind/node filter over the rendered timeline
    effect(() => {
      this.kindFilter();
      this.nodeFilter();
      this.events();
      this.applyFilter();
    });
  }

  ngAfterViewInit(): void {
    this.wireTimeline();
    this.wireStrip();
    // first paint of whatever is already loaded
    this.renderHeader();
    this.renderTimeline();
    this.renderStrip();
    this.renderScrubber();
  }

  // ── run loading ──────────────────────────────────────────────────────────
  private loadRun(id: string | undefined): void {
    const prev = this.channelSig();
    if (prev && prev.runId === id) return; // same run — nothing to do
    prev?.disconnect();
    this.nodeFilter.set(null);
    this.arrivedSeq = null;
    this.prevLen = 0;

    if (!id) {
      this.channelSig.set(undefined);
      this.events.set([]);
      this.prefixN.set(0);
      return;
    }

    void this.fold.ready().then(() => this.foldReady.set(true));
    const channel = this.runEvents.open(id, { fromSeq: 0 });
    this.channelSig.set(channel);
    this.destroyRef.onDestroy(() => channel.disconnect());
    channel.connect();
  }

  private onEvents(evs: SalvorEvent[]): void {
    const wasAtHead = this.prefixN() === this.prevLen;
    if (evs.length > this.prevLen && this.prevLen > 0) {
      this.arrivedSeq = evs[evs.length - 1]?.seq ?? null;
    }
    this.prevLen = evs.length;
    this.events.set(evs);
    if (wasAtHead) this.prefixN.set(evs.length); // follow the head; otherwise HOLD where scrubbed
  }

  // ── folding ──────────────────────────────────────────────────────────────
  private foldSafe(n: number): RunStateJson | undefined {
    if (!this.foldReady()) return undefined;
    try {
      return this.fold.deriveState(this.wireLog(), n);
    } catch {
      return undefined;
    }
  }

  // ── header (run-stats hero) ───────────────────────────────────────────────
  private renderHeader(): void {
    const el = this.statsEl?.nativeElement;
    const evs = this.events();
    if (!el || evs.length === 0) return;
    const st = this.foldSafe(evs.length);
    if (!st) return;
    const state = statusStateOf(st.status);
    const steps = stepsOf(evs, evs.length);
    const first = evs[0].recordedAt;
    const last = evs[evs.length - 1].recordedAt;
    const secs = Math.round((Date.parse(last) - Date.parse(first)) / 1000);
    const dur = secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`;
    const cost = costOfPrefix(evs, evs.length);
    const agent = agentOf(evs) ?? '';
    const agentKind: 'hash' | 'label' = isHash(agent) ? 'hash' : 'label';

    // The ceiling: crossed (read from this run's own BudgetExceeded) or unknown — this build has
    // no GET /v1/agents registry, so a declared-but-uncrossed ceiling cannot be fetched. Honest.
    let ceiling: string;
    if (st.status.kind === 'BudgetExceeded') {
      ceiling = `crossed the ${usd(st.status.budget.limit)} ceiling${info('The ceiling that was actually in force, read from this run’s own BudgetExceeded event.')}`;
    } else {
      ceiling = `ceiling unknown${info('The agent’s declared ceiling is not fetched in this build (no GET /v1/agents registry wired). A ceiling only enters the log when it is crossed.')}`;
    }

    el.innerHTML = `
      <div class="stat hero"><dt>Status</dt>
        <dd>${statusHtml(state)}<span class="sub">${evs.length} events · ${steps} model turn${steps === 1 ? '' : 's'}</span></dd></div>
      <div class="stat"><dt>Agent</dt>
        <dd style="font-size:15px">${this.runRefHtml(agent, agentKind)}
          <span class="sub">${
            agentKind === 'hash'
              ? 'agent_def_hash — the log records no human name for an agent'
              : 'a label the driver recorded, not a hash — shown in full'
          }</span></dd></div>
      <div class="stat"><dt>Cost</dt>
        <dd class="figure">${cost.complete ? usd(cost.usd ?? 0) : '<span class="tokens-only">tokens only</span>'}
          <span class="sub">${cost.complete ? ceiling : `${esc((cost.unpriced as string[]).join(', '))} is not in the price table`}</span></dd></div>
      <div class="stat"><dt>Tokens</dt>
        <dd class="figure">${int(st.usage.input_tokens)} / ${int(st.usage.output_tokens)}
          <span class="sub">in / out · exact from the fold</span></dd></div>
      <div class="stat"><dt>Duration</dt>
        <dd class="figure">${dur}<span class="sub">${clock(first)} → ${clock(last)}</span></dd></div>`;

    if (this.bandEl) this.bandEl.nativeElement.innerHTML = this.bandHtml(st, state);
    if (this.lineageEl) this.lineageEl.nativeElement.innerHTML = ''; // no fork lineage in real logs
  }

  private runRefHtml(value: string, kind: 'hash' | 'label'): string {
    const shown = kind === 'hash' ? 'sha256:' + value.replace(/^sha256:/, '').slice(0, 8) : value;
    const cls = kind === 'label' ? 'agent-label' : 'runid';
    return `<span class="runref"><span class="${cls}" title="${esc(value)}">${esc(shown)}</span>
      <button class="copy" type="button" data-copy="${esc(value)}"><span class="sr">Copy ${esc(value)}</span>${COPY_ICO}</button></span>`;
  }

  private bandHtml(st: RunStateJson, state: string): string {
    if (state === 'failed') {
      const err = st.status.kind === 'Failed' ? st.status.error : '';
      return `<div class="band is-fail">${FAIL_ICO}
        <span><b>Failed.</b>${err ? ` ${esc(err)}` : ''} This run is terminal; its log is closed.</span></div>`;
    }
    if (!isWaitingState(state)) return '';
    const action: Record<string, [string, string]> = {
      needs_reconciliation: [
        'Resolve in inbox',
        'A write was recorded as intended, with no completion. The fold parks the run rather than guess whether the side effect landed.',
      ],
      suspended: [
        'Approve in inbox',
        st.status.kind === 'Suspended'
          ? esc(st.status.reason)
          : 'The run recorded an input schema and is waiting for a person to fill it.',
      ],
      budget_exceeded: [
        'Raise limit in inbox',
        st.status.kind === 'BudgetExceeded'
          ? `Crossed the ${usd(st.status.budget.limit)} ceiling at an observed ${usd(st.status.observed)}.`
          : '',
      ],
    };
    const a = action[state] ?? ['Open in inbox', ''];
    return `<div class="band">${WARN_ICO}
      <span><b>${esc(labelOf(state))}.</b> ${a[1]}</span>
      <button class="link-btn" type="button" data-goto="inbox">${a[0]}</button></div>`;
  }

  // ── timeline ──────────────────────────────────────────────────────────────
  private renderTimeline(): void {
    const el = this.timelineEl?.nativeElement;
    if (!el) return;
    el.innerHTML = renderTimelineHtml(this.events(), this.arrivedSeq);
    this.applyFilter();
    if (this.allOpen()) this.setAllOpen(true);
  }

  private renderStrip(): void {
    const el = this.stripEl?.nativeElement;
    if (!el) return;
    el.innerHTML = renderStripHtml(this.events(), this.arrivedSeq);
  }

  private applyFilter(): void {
    const el = this.timelineEl?.nativeElement;
    if (!el) return;
    const kf = this.kindFilter();
    const nf = this.nodeFilter();
    el.querySelectorAll<HTMLElement>('.levent').forEach((row) => {
      const cat = row.dataset['cat'];
      const node = row.dataset['node'] ?? '';
      const on = (kf === 'all' || cat === kf) && (!nf || node === nf);
      row.hidden = !on;
    });
    el.querySelectorAll<HTMLElement>('section.turn').forEach((s) => {
      s.hidden = !s.querySelector('.levent:not([hidden])');
    });
  }

  // ── scrubber (derived panel, dim/cut, playhead, ticks, fork offer) ─────────
  private renderScrubber(): void {
    const n = this.prefixN();
    const evs = this.events();
    const st = this.foldSafe(n);

    // derived-state field list
    const derived = this.derivedEl?.nativeElement;
    if (derived && st) {
      const state = statusStateOf(st.status);
      const cost: CostTotal = costOfPrefix(evs, n);
      derived.innerHTML = `
        <div class="drow"><dt>status</dt><dd>${statusHtml(state)}</dd></div>
        <div class="drow"><dt>group</dt><dd>${groupOf(state)}</dd></div>
        <div class="drow"><dt>next_seq</dt><dd>${st.next_seq}</dd></div>
        <div class="drow"><dt>usage.input</dt><dd>${int(st.usage.input_tokens)}</dd></div>
        <div class="drow"><dt>usage.output</dt><dd>${int(st.usage.output_tokens)}</dd></div>
        <div class="drow"><dt>cost</dt><dd>${cost.complete ? usd(cost.usd ?? 0) : '<span class="tokens-only">tokens only</span>'}</dd></div>
        <div class="drow"><dt>pending</dt><dd>${esc(pendingLabel(st, evs))}</dd></div>`;
    }

    this.renderForkHere(n);

    // dim/cut on timeline rows
    this.timelineEl?.nativeElement.querySelectorAll<HTMLElement>('.levent').forEach((row) => {
      const seq = Number(row.dataset['seq']);
      row.classList.toggle('dim', seq >= n);
      row.classList.toggle('cut', seq === n && n > 0);
    });
    // future on ticks
    this.stripEl?.nativeElement.querySelectorAll<HTMLElement>('.etick').forEach((t) => {
      t.classList.toggle('future', Number(t.dataset['scrub']) >= n);
    });
    // playhead
    const ph = this.playheadEl?.nativeElement;
    if (ph) {
      const len = evs.length || 1;
      ph.style.left = `${(n / len) * 100}%`;
      ph.hidden = evs.length === 0;
    }
    this.arrivedSeq = null; // the arrival has been drawn once
  }

  private renderForkHere(n: number): void {
    const el = this.forkHereEl?.nativeElement;
    if (!el) return;
    // CAPABILITY GATE: render the offer only if the server advertises a fork API. It does not in
    // this build, so this is '' in production — the path stays, honesty-gated, for v0.4.
    if (!forkOffered(this.caps) || this.events().length === 0) {
      el.innerHTML = '';
      return;
    }
    const at = this.events().find((e) => e.seq === n - 1);
    const node = (at?.payload['node'] as string | undefined) ?? '';
    el.innerHTML = `<button class="btn ghost" type="button" id="fork-here-btn">Fork this run…</button>
      <p class="gloss">${
        node
          ? `From <span class="mono">${esc(node)}</span> — the node under the playhead.`
          : 'The event under the playhead names no node, so this opens the canvas to let you pick the fork point.'
      }</p>`;
  }

  // ── live ticker bar ───────────────────────────────────────────────────────
  private renderLive(): void {
    const el = this.liveBarEl?.nativeElement;
    if (!el) return;
    const ch = this.channelSig();
    const evs = this.events();
    if (!ch || evs.length === 0) {
      el.innerHTML = '';
      this.liveMode = null;
      return;
    }
    const kind = ch.state().kind;
    const following = this.following();
    const n = this.prefixN();
    const behind = evs.length - n;

    let mode: 'live' | 'held' | 'done';
    if (kind === 'ended' && following) mode = 'done';
    else if (following) mode = 'live';
    else mode = 'held';
    const enter = mode !== this.liveMode ? ' enter' : '';
    this.liveMode = mode;

    if (mode === 'done') {
      el.innerHTML = `<span class="live-tag done${enter}">Run complete · ${evs.length} events</span>`;
      return;
    }
    if (mode === 'live') {
      el.innerHTML = `<span class="live-tag${enter}"><span class="live-dot"></span>Live · following the head</span>`;
      return;
    }
    el.innerHTML = `<span class="live-tag held${enter}">Held at seq ${n} — ${behind} newer event${behind === 1 ? '' : 's'} recorded</span>
      <button class="link-btn" type="button" id="to-live">Jump to live</button>`;
    const b = el.querySelector<HTMLButtonElement>('#to-live');
    if (b) b.addEventListener('click', () => this.foldAll());
  }

  // ── scrub controls ────────────────────────────────────────────────────────
  private setPrefix(n: number): void {
    const clamped = Math.max(0, Math.min(this.events().length, n));
    this.prefixN.set(clamped);
  }
  onRangeInput(e: Event): void {
    this.setPrefix(Number((e.target as HTMLInputElement).value));
  }
  onRangeGrab(): void {
    this.scrubEl()?.classList.add('dragging');
  }
  onRangeRelease(): void {
    this.scrubEl()?.classList.remove('dragging');
  }
  private scrubEl(): HTMLElement | undefined {
    return this.stripEl?.nativeElement.closest('.scrub') as HTMLElement | undefined;
  }

  foldAll(): void {
    this.setPrefix(this.events().length);
  }

  private scrubTo(seq: number): void {
    this.setPrefix(seq + 1);
  }

  /** Which tick is under `clientX` — asked of the ticks' own geometry, not a proportion of the
   *  strip (the off-by-one fix). Returns -1 for "off the left edge: fold nothing". */
  private tickAt(clientX: number): number {
    const strip = this.stripEl?.nativeElement;
    const n = this.events().length;
    if (!strip || n === 0) return -1;
    const box = strip.getBoundingClientRect();
    if (clientX < box.left) return -1;
    const ticks = strip.children;
    const pitch =
      n > 1 ? (ticks[1] as HTMLElement).offsetLeft - (ticks[0] as HTMLElement).offsetLeft : box.width;
    return Math.min(n - 1, Math.floor((clientX - box.left) / pitch));
  }
  private scrubFromX(clientX: number): void {
    if (this.events().length === 0) return;
    this.setPrefix(this.tickAt(clientX) + 1);
  }

  private wireStrip(): void {
    const strip = this.stripEl?.nativeElement;
    if (!strip) return;
    let dragging = false;
    strip.addEventListener('pointerdown', (e) => {
      dragging = true;
      strip.setPointerCapture(e.pointerId);
      this.scrubEl()?.classList.add('dragging');
      this.scrubFromX(e.clientX);
    });
    strip.addEventListener('pointermove', (e) => {
      if (dragging) this.scrubFromX(e.clientX);
    });
    strip.addEventListener('pointerup', (e) => {
      dragging = false;
      this.scrubEl()?.classList.remove('dragging');
      try {
        strip.releasePointerCapture(e.pointerId);
      } catch {
        /* capture may already be gone */
      }
    });
    // every tick is also a real button: a click (mouse or keyboard Enter) folds through its seq
    strip.addEventListener('click', (e) => {
      const t = (e.target as HTMLElement).closest<HTMLElement>('[data-scrub]');
      if (t) this.scrubTo(Number(t.dataset['scrub']));
    });
  }

  private wireTimeline(): void {
    const tl = this.timelineEl?.nativeElement;
    if (!tl) return;
    tl.addEventListener('click', (e) => {
      const target = e.target as HTMLElement;
      const fold = target.closest<HTMLElement>('[data-scrub]');
      if (fold) {
        this.scrubTo(Number(fold.dataset['scrub']));
        return;
      }
      if (target.closest('[data-copy]')) return;
      const row = target.closest('.lrow');
      if (!row) return;
      const levent = row.closest('.levent');
      if (levent) this.setOpen(levent as HTMLElement, !levent.classList.contains('open'));
    });
    // copy buttons live in the header (agent hash); delegate from stats
    this.statsEl?.nativeElement.addEventListener('click', (e) => {
      const b = (e.target as HTMLElement).closest<HTMLElement>('[data-copy]');
      if (b) void navigator.clipboard?.writeText(b.dataset['copy'] ?? '').catch(() => {});
    });
    // the band's "…in inbox" link
    this.bandEl?.nativeElement.addEventListener('click', (e) => {
      const b = (e.target as HTMLElement).closest<HTMLElement>('[data-goto]');
      if (b) this.viewService.go('inbox');
    });
  }

  private setOpen(levent: HTMLElement, open: boolean): void {
    levent.classList.toggle('open', open);
    levent.querySelector('.iact.expand')?.setAttribute('aria-expanded', String(open));
  }
  private setAllOpen(open: boolean): void {
    this.timelineEl?.nativeElement
      .querySelectorAll<HTMLElement>('.levent')
      .forEach((el) => this.setOpen(el, open));
  }

  // ── toolbar ───────────────────────────────────────────────────────────────
  setKindFilter(key: KindKey): void {
    this.kindFilter.set(key);
  }
  clearNodeFilter(): void {
    this.nodeFilter.set(null);
  }
  toggleExpandAll(): void {
    const open = !this.allOpen();
    this.allOpen.set(open);
    this.setAllOpen(open);
  }

  // ── the scrubber panel dock ───────────────────────────────────────────────
  closePanel(): void {
    this.panelOpen.set(false);
    setTimeout(() => this.panelTab?.nativeElement.focus(), 0);
  }
  openPanel(): void {
    this.panelOpen.set(true);
    setTimeout(() => this.panelClose?.nativeElement.focus(), 0);
  }

  // exposed for the template
  isTerminalState = isTerminalState;
  readonly KINDS = KINDS;
}
