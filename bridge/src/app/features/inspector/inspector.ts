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

import {
  GraphRunService,
  RunEventsService,
  SALVOR_CLIENT,
  type ForkListEntry,
  type ForkOrigin,
  type RunEventsChannel,
} from '../../core/api';
import { FirstReceiptsService } from '../../core/first-receipts';
import { ForkIntentService } from '../../core/fork-intent';
import { PillService } from '../../core/pill';
import { RunsService } from '../../core/api';
import { ViewService } from '../../core/view';
import { age, derivedStatus, groupOf, labelOf } from '../runs/run-model';
import { SERVER_CAPABILITIES, forkOffered } from './capability';
import { KINDS, clock, estripGapPct, renderStripHtml, renderTimelineHtml, zoneOf } from './event-model';
import { esc } from '../../shared/json-hi';
import { focusWhenRendered } from '../../core/focus';
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
// A hollow-slash mark (matching the muted `⊘` on the abandoned pill): a terminal, non-error close.
const ABANDON_ICO = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><circle cx="8" cy="8" r="6.4"/><path d="M3.6 3.6l8.8 8.8"/></svg>`;

function info(why: string): string {
  return `<button class="info" type="button" title="${esc(why)}" aria-label="${esc(why)}">i</button>`;
}

/**
 * The Inspector: one run read from its log. Timeline over the REAL event stream
 * ({@link RunEventsService}), VIRTUALIZED with `content-visibility` so painting is bounded while
 * every recorded event stays in the DOM and scrub-and-keyboard reachable; the scrubber folds
 * prefixes through the REAL wasm fold ({@link FoldService}); JSON highlighting via `jsonHi`.
 * Hold-on-scrub-back during live SSE appends, the live ticker driven by the channel's
 * connection state, the run-stats hero, the note apparatus, the empty state, and the
 * capability-gated fork offer (off in this build — the server has no fork runtime).
 *
 * The complex, byte-exact DOM (the `.levent` grid, kchip dots, effect badges, highlighted panes,
 * the derived panel) is built as HTML strings and written into stable container refs with
 * delegated event handling, so the DOM stays byte-exact and selector-addressable rather than
 * drifting through Angular's own template diffing. Signals drive WHEN each container re-renders;
 * the containers themselves are fixed, so click/pointer wiring is attached once.
 */
@Component({
  selector: 'bridge-inspector',
  templateUrl: './inspector.html',
})
export class Inspector implements AfterViewInit {
  private readonly runEvents = inject(RunEventsService);
  private readonly runsService = inject(RunsService);
  private readonly client = inject(SALVOR_CLIENT);
  private readonly graphRuns = inject(GraphRunService);
  private readonly fold = inject(FoldService);
  private readonly viewService = inject(ViewService);
  private readonly pill = inject(PillService);
  private readonly destroyRef = inject(DestroyRef);
  private readonly caps = inject(SERVER_CAPABILITIES);
  private readonly forkIntent = inject(ForkIntentService);
  private readonly firstReceipts = inject(FirstReceiptsService);

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

  /** A graph run's `graph_hash`, fetched from GET /v1/runs/{id}/graph (it is never on the log or
   * the list row). Undefined until the projection loads — the AGENT card shows the honest "graph
   * run" identity with no fabricated hash until then. */
  private readonly graphHash = signal<string | undefined>(undefined);

  /** The run's LIVENESS evidence from GET /v1/runs/{id}: the server's `driver` field and the newest
   * event's `last_recorded_at`. The log fold tells us a run IS `running`; only this tells us whether
   * anyone is DRIVING it. Fetched once per run (reset in `loadRun`); undefined until it lands, so
   * the stalled treatment appears only once there is genuine evidence for it — never a guess. */
  private readonly runLiveness = signal<{ driver?: string; last?: string } | undefined>(undefined);

  /** The stalled verdict for the open run: its log folds to some state in the IN-PROGRESS family
   * (`running`, `awaiting_model`, `awaiting_tool`, `not_started` — see `run-model.ts#derivedStatus`
   * for why the family is wider than literal `running`), yet the server reports no driver and its
   * last event has gone stale. This is the one place the Inspector reads the same derivation the
   * ledger and Inbox do. */
  readonly stalled = computed<boolean>(() => {
    const rest = this.restingState();
    const live = this.runLiveness();
    if (!rest || groupOf(rest) !== 'progress' || !live) return false;
    return derivedStatus(rest, live.driver, live.last) === 'stalled';
  });

  /** The relative age of the last recorded event, for the "last event 10m ago" phrasing. */
  readonly lastEventAge = computed<string>(() => age(this.runLiveness()?.last));

  /** Fork LINEAGE, both directions, read from the server's derived surfaces (never the log):
   *  - `forkedFrom` — this run's own projection carries it when it IS a forked child (the header
   *    chip links back to the parent).
   *  - `forks` — GET /v1/runs/{id}/forks, the derived index of children forked FROM this run.
   * Both are honestly labelled derived where shown; both undefined until fetched, and reset per run. */
  private readonly forkedFrom = signal<ForkOrigin | undefined>(undefined);
  private readonly forks = signal<readonly ForkListEntry[] | undefined>(undefined);

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

  /** A graph run by construction: its log opens with `GraphRunStarted`, not the `RunStarted` every
   * agent run records first, so it never carried a single `agent_def_hash`. Same classification the
   * Runs table's `agentIdentity` makes, read here from the log the Inspector already holds. */
  readonly isGraphRun = computed(() => this.events()[0]?.kind === 'GraphRunStarted');

  /** The run's TRUE resting state slug, folded from the whole log — what the run actually IS
   * (`suspended`, `completed`, `budget_exceeded`, …), not what the transport did. The pill and the
   * live ticker read this so a caught-up stream over a parked run is never labeled as ended. */
  readonly restingState = computed<string | null>(() => {
    if (!this.foldReady() || this.events().length === 0) return null;
    const st = this.foldSafe(this.events().length);
    return st ? statusStateOf(st.status) : null;
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
        this.pill.set(ch.state(), ch.runId, this.restingState() ?? undefined);
      } else {
        this.pill.toSnapshot(asOf);
      }
    });

    // (4) full body render (header, timeline, strip) when the log, fold-readiness, or the graph
    // run's loaded graph_hash changes (the AGENT card names the graph_hash once the projection lands)
    effect(() => {
      this.events();
      this.foldReady();
      this.graphHash();
      this.runLiveness();
      if (!this.hasRun()) return;
      this.renderHeader();
      this.renderTimeline();
      this.renderStrip();
      this.renderScrubber();
    });

    // (4a) The run's liveness evidence (GET /v1/runs/{id}: `driver` + `last_recorded_at`). The log
    // fold says whether the run IS running; this says whether anyone is driving it, the two facts a
    // stall is derived from. Fetched once per run, silent on failure (no stalled treatment without
    // evidence — honesty over a guess), and guarded so a late response for a since-closed run is
    // dropped rather than shown against the wrong run.
    effect(() => {
      const id = this.viewService.runId();
      if (!id || this.runLiveness() !== undefined) return;
      this.client
        .getRun(id)
        .then((state) => {
          if (this.viewService.runId() === id) {
            this.runLiveness.set({ driver: state.driver, last: state.lastRecordedAt });
          }
        })
        .catch(() => {
          /* liveness unreadable: the header simply carries no stalled treatment, never a fake one */
        });
    });

    // (4b) a graph run carries no agent_def_hash; its graph_hash lives only behind
    // GET /v1/runs/{id}/graph. Fetch it once per graph run so the AGENT card can name it — silent
    // on failure (the card keeps the plain "graph run" identity, never a fabricated hash).
    effect(() => {
      const id = this.viewService.runId();
      if (!id || !this.isGraphRun() || this.graphHash() !== undefined) return;
      this.graphRuns
        .loadProjection(id)
        .then((p) => {
          if (this.viewService.runId() === id) {
            this.graphHash.set(p.graphHash);
            // A forked child's projection names its origin; the header chip links back to it.
            this.forkedFrom.set(p.forkedFrom);
          }
        })
        .catch(() => {
          /* projection unavailable: the AGENT card keeps the plain "graph run" identity, no fake hash */
        });
    });

    // (4c) The forks OF this run — the server's derived index (GET /v1/runs/{id}/forks). Graph runs
    // only (the fork API is graph-run shaped; asking about a plain agent run would 404 and the
    // browser logs that to the console). Fetched once per run, silent on failure.
    effect(() => {
      const id = this.viewService.runId();
      if (!id || !this.isGraphRun() || this.forks() !== undefined) return;
      this.graphRuns
        .listForks(id)
        .then((idx) => {
          if (this.viewService.runId() === id) this.forks.set(idx.forks);
        })
        .catch(() => {
          /* forks index unreachable: the lineage panel simply does not render */
        });
    });

    // (4d) render the fork lineage (the forked-from chip and the forks-of-this-run panel)
    effect(() => {
      this.forkedFrom();
      this.forks();
      this.renderLineage();
    });

    // (5) scrub-only render (derived panel, dim/cut, playhead, ticks, fork offer). Also re-runs
    // when the capability probe answers — the fork offer is gated on it, and the probe resolves
    // after the first paint.
    effect(() => {
      this.prefixN();
      this.events();
      this.foldReady();
      this.caps();
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
    this.graphHash.set(undefined); // a new run: forget the previous run's graph_hash
    this.forkedFrom.set(undefined);
    this.forks.set(undefined);
    this.runLiveness.set(undefined); // and its liveness evidence, re-fetched below

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

    // The ceiling: crossed (read from this run's own BudgetExceeded) or unknown — this build has
    // no GET /v1/agents registry, so a declared-but-uncrossed ceiling cannot be fetched. Honest.
    let ceiling: string;
    if (st.status.kind === 'BudgetExceeded') {
      ceiling = `crossed the ${usd(st.status.budget.limit)} ceiling${info('The ceiling that was actually in force, read from this run’s own BudgetExceeded event.')}`;
    } else {
      ceiling = `ceiling unknown${info('The agent’s declared ceiling is not fetched in this build (no GET /v1/agents registry wired). A ceiling only enters the log when it is crossed.')}`;
    }

    // The hero pill reflects the DERIVED state: a run whose log folds to `running` but that the
    // server reports no driver for, gone stale, reads `stalled` here — the same verdict the ledger
    // pill makes, so the Inspector never asserts "running" for a run going nowhere.
    const displayState = this.stalled() ? 'stalled' : state;
    el.innerHTML = `
      <div class="stat hero"><dt>Status</dt>
        <dd>${statusHtml(displayState)}<span class="sub">${evs.length} events · ${steps} model turn${steps === 1 ? '' : 's'}</span></dd></div>
      ${this.agentStatHtml(evs)}
      <div class="stat"><dt>Cost</dt>
        <dd class="figure">${cost.complete ? usd(cost.usd ?? 0) : '<span class="tokens-only">tokens only</span>'}
          <span class="sub">${cost.complete ? ceiling : `${esc((cost.unpriced as string[]).join(', '))} is not in the price table`}</span></dd></div>
      <div class="stat"><dt>Tokens</dt>
        <dd class="figure">${int(st.usage.input_tokens)} / ${int(st.usage.output_tokens)}
          <span class="sub">in / out · exact from the fold</span></dd></div>
      <div class="stat"><dt>Duration</dt>
        <dd class="figure">${dur}<span class="sub">${clock(first)} → ${clock(last)}</span></dd></div>`;

    if (this.bandEl) this.bandEl.nativeElement.innerHTML = this.bandHtml(st, state);
  }

  /** The fork lineage rail (`#run-lineage`), both directions, from the server's derived surfaces:
   * a "forked from <run> at <node>" chip when this run IS a fork, and a "Forks of this run" panel
   * listing the children forked from it. Both honestly labelled derived. Empty when neither holds. */
  private renderLineage(): void {
    const el = this.lineageEl?.nativeElement;
    if (!el) return;
    const parent = this.forkedFrom();
    const children = this.forks() ?? [];
    let html = '';
    if (parent) {
      html += `<div class="lineage-chip" data-forked-from>
        <span class="k">forked from</span>
        <button class="lineage-link" type="button" data-goto-run="${esc(parent.runId)}">
          <span class="runid">${esc(parent.runId.slice(0, 8))}</span> at <span class="mono">${esc(parent.fromNode)}</span>
        </button>
        <span class="lineage-derived" title="Recorded on the fork, not on the origin's log">derived</span>
      </div>`;
    }
    if (children.length) {
      const rows = children
        .map(
          (f) => `<li><button class="lineage-link" type="button" data-goto-run="${esc(f.run)}">
            <span class="runid">${esc(f.run.slice(0, 8))}</span></button>
            <span class="lineage-at">at <span class="mono">${esc(f.fromNode)}</span></span></li>`,
        )
        .join('');
      html += `<div class="forks-panel" data-forks-panel>
        <div class="forks-head">Forks of this run
          <span class="lineage-derived" title="A derived index; not a fact this run recorded">derived</span></div>
        <ul class="forks-list">${rows}</ul>
        <p class="gloss">The server's derived index (<span class="mono">GET /v1/runs/${esc((this.viewService.runId() ?? '').slice(0, 8))}…/forks</span>) — not a fact the origin's log recorded.</p>
      </div>`;
    }
    el.innerHTML = html;
  }

  /** The AGENT hero card. A graph run records no `agent_def_hash`, so the card renders the same
   * honest "graph run" identity the Runs table uses, naming its `graph_hash` once the
   * projection has loaded rather than a blank value. An agent run renders its recorded hash/label. */
  private agentStatHtml(evs: SalvorEvent[]): string {
    if (evs[0]?.kind === 'GraphRunStarted') {
      const gh = this.graphHash();
      const value = gh
        ? `graph run · ${this.runRefHtml(gh, 'hash')}`
        : 'graph run';
      const sub = gh
        ? 'graph_hash — a graph run has no single agent_def_hash; this is the graph it ran'
        : 'a graph run — its log has no single agent_def_hash; its graph_hash is loading';
      return `<div class="stat"><dt>Agent</dt>
        <dd style="font-size:15px">${value}<span class="sub">${sub}</span></dd></div>`;
    }
    const agent = agentOf(evs) ?? '';
    const agentKind: 'hash' | 'label' = isHash(agent) ? 'hash' : 'label';
    return `<div class="stat"><dt>Agent</dt>
      <dd style="font-size:15px">${this.runRefHtml(agent, agentKind)}
        <span class="sub">${
          agentKind === 'hash'
            ? 'agent_def_hash — the log records no human name for an agent'
            : 'a label the driver recorded, not a hash — shown in full'
        }</span></dd></div>`;
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
    // ABANDONED — a terminal, operator-retired run. Muted, never the fail band's danger red:
    // abandonment is a deliberate retirement, not an error (state-not-status ink). States the
    // reason when one was recorded, and — when the run was abandoned while parked at a dangling
    // write — the unresolved-write honesty line: the abandonment recorded the outstanding write
    // rather than claiming it settled.
    if (state === 'abandoned') {
      const reason = st.status.kind === 'Abandoned' ? st.status.reason : undefined;
      const uw = st.status.kind === 'Abandoned' ? st.status.unresolved_write : undefined;
      const reasonText = reason ? ` ${esc(reason)}` : '';
      const honesty = uw
        ? ` The write at seq ${uw.seq} (<span class="mono">${esc(uw.tool)}</span>) was left unresolved and is recorded as such; its effect remains unknown.`
        : '';
      return `<div class="band is-abandoned">${ABANDON_ICO}
        <span><b>Abandoned.</b>${reasonText} This run was retired by an operator; it is terminal and its log is closed.${honesty}</span></div>`;
    }
    // STALLED — the expert's exact phrasing family: "<resting state> — last event 10m ago, no
    // driver attached." The resting state named is the run's ACTUAL fold (e.g. `awaiting_model`
    // for a run that died mid model-call), never a hardcoded "running" — the in-progress family is
    // wider than that one state (see `run-model.ts#derivedStatus`). Attention family (amber), never
    // the fail band's danger red. Its "action" is a signpost to the Inbox card, whose guidance is
    // external (restart the host driver) — there is no fix button, here or there.
    if (this.stalled()) {
      return `<div class="band is-stalled">${WARN_ICO}
        <span><b>Stalled.</b> <span class="mono">${esc(labelOf(state))}</span> — last event ${esc(this.lastEventAge())} ago, no driver attached. No task is driving this run and no client lease is current; restart the driver that owns it, or resolve/abandon.</span>
        <button class="link-btn" type="button" data-goto="inbox">See in inbox</button></div>`;
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
    const evs = this.events();
    // The gap is density-scaled so it can never itself overflow the strip (see
    // event-model.ts#estripGapPct) — set inline because it depends on the event count, which
    // renderStripHtml's returned markup (the ticks alone) has no way to carry onto the container.
    el.style.gap = `${estripGapPct(evs.length)}%`;
    el.innerHTML = renderStripHtml(evs, this.arrivedSeq);
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
      // TIME-TRAVEL PREVIEW: while the playhead HOLDS before the log's end, the whole rail is a
      // derived PAST state, not live truth. Carry a visibly distinct preview treatment on the rail
      // (a tint + hatch, the .preview class) and a pinned banner that names the run's TRUE resting
      // state, so a past `completed`/`running` pill is never mistaken for what the run IS now. The
      // status pill itself gets an outlined "at seq N" variant. Extends the timeline's own
      // boundary/dimmed-future vocabulary rather than inventing a new one. At the head (following)
      // the rail is plain — nothing is being previewed.
      const held = n < evs.length;
      derived.classList.toggle('preview', held);
      let banner = '';
      if (held) {
        const rest = this.restingState();
        const restLabel = rest ? labelOf(rest) : 'at rest';
        banner = `<div class="preview-note" data-preview-banner>Preview of past state — the run is still <b>${esc(restLabel)}</b>. <button class="link-btn" type="button" data-to-live>Jump to live.</button></div>`;
      }
      const statusDd = held
        ? `<span class="at-seq" data-at-seq>at seq ${n}</span>${statusHtml(state)}`
        : statusHtml(state);
      derived.innerHTML = `
        ${banner}
        <div class="drow"><dt>status</dt><dd>${statusDd}</dd></div>
        <div class="drow"><dt>group</dt><dd>${groupOf(state)}</dd></div>
        <div class="drow"><dt>next_seq</dt><dd>${st.next_seq}</dd></div>
        <div class="drow"><dt>usage.input</dt><dd>${int(st.usage.input_tokens)}</dd></div>
        <div class="drow"><dt>usage.output</dt><dd>${int(st.usage.output_tokens)}</dd></div>
        <div class="drow"><dt>cost</dt><dd>${cost.complete ? usd(cost.usd ?? 0) : '<span class="tokens-only">tokens only</span>'}</dd></div>
        <div class="drow"><dt>pending</dt><dd>${esc(pendingLabel(st, evs))}</dd></div>`;
    }

    this.renderForkHere(n);

    // The three scrub zones on timeline rows — folded (no class), boundary (the event under the
    // playhead, seq n-1), beyond (dimmed, seq >= n). `zoneOf` is the single source of truth so
    // this toggle and its unit tests cannot drift apart.
    let boundaryRow: HTMLElement | undefined;
    this.timelineEl?.nativeElement.querySelectorAll<HTMLElement>('.levent').forEach((row) => {
      const seq = Number(row.dataset['seq']);
      const zone = zoneOf(seq, n);
      row.classList.toggle('boundary', zone === 'boundary');
      row.classList.toggle('dim', zone === 'beyond');
      if (zone === 'boundary') boundaryRow = row;
    });
    // FOLLOW — keep the boundary row in view WHILE SCRUBBING, and only then. `.dragging` on the
    // scrub element is the single "actively scrubbing" predicate: the range's pointer + keyboard
    // handlers (onRangeGrab/onRangeRelease) and the tick strip (wireStrip) both set it; a
    // re-render triggered by a live append or a filter change never does, so browsing the log by
    // hand is never hijacked. This is the same respect-the-reading-position principle as the live
    // ticker's hold-on-scrub-back: THAT decides whether the PLAYHEAD advances when a new event
    // arrives; THIS decides whether the VIEWPORT moves during a drag. They cannot fight — the
    // ticker is not a drag and so never sets `.dragging`. A boundary row hidden by the kind/node
    // filter is not chased.
    if (n > 0 && boundaryRow && !boundaryRow.hidden && this.scrubEl()?.classList.contains('dragging')) {
      this.followBoundary(boundaryRow);
    }
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

  /** The node under the playhead. A real log attributes events to nodes as RECORDED SPANS —
   * `NodeEntered`/`NodeExited` pairs — not as a denormalised field on every envelope, so this
   * folds the prefix: the node whose open span contains the playhead is the node under it. That
   * is reading recorded structure, never guessing. Outside any span (RunStarted, the gap after an
   * exit, a budget event) it is null — the canvas lets the operator point at a node instead. */
  private forkNodeAt(n: number): string | null {
    let current: string | null = null;
    for (const e of this.events()) {
      if (e.seq >= n) break;
      const node = e.payload['node'] as string | undefined;
      if (e.kind === 'NodeEntered' && node !== undefined) current = node;
      else if (e.kind === 'NodeExited') current = null;
    }
    return current;
  }

  private renderForkHere(n: number): void {
    const el = this.forkHereEl?.nativeElement;
    if (!el) return;
    // CAPABILITY GATE: render the offer only if the server genuinely advertises a fork API
    // (GET /v1/capabilities, probed). A server without one gets no dead button.
    if (!forkOffered(this.caps()) || this.events().length === 0) {
      el.innerHTML = '';
      return;
    }
    const node = this.forkNodeAt(n) ?? '';
    el.innerHTML = `<button class="btn ghost" type="button" id="fork-here-btn">Fork this run…</button>
      <p class="gloss">${
        node
          ? `From <span class="mono">${esc(node)}</span> — the node under the playhead. Opens the hazard review on the canvas: every write recorded after that node will happen again, and each one needs your say-so.`
          : 'The event under the playhead names no node, so this opens the canvas to let you pick the fork point. It will not choose one for you.'
      }</p>`;
  }

  /** The scrubber's fork offer, into the ONE fork door the canvas owns. The canvas resolves the
   * run's graph, lands on the canonical `/workflows/<hashPrefix>` URL and opens the hazard review
   * (or its refusal) — byte-identical to a canvas-originated fork. */
  onForkHere(event: Event): void {
    if (!(event.target as HTMLElement).closest('#fork-here-btn')) return;
    const runId = this.viewService.runId();
    if (runId === undefined) return;
    const node = this.forkNodeAt(this.prefixN());
    this.forkIntent.request(runId, node ?? undefined);
    this.viewService.go('workflows');
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
      // The stream is at its terminal frame with the playhead at the head — but "at rest" is not
      // "complete". State the run's TRUE resting state (a suspended run is parked, not finished).
      const rest = this.restingState();
      const restText = rest === 'completed' || rest === null ? 'Run complete' : labelOf(rest);
      el.innerHTML = `<span class="live-tag done${enter}">${esc(restText)} · ${evs.length} events</span>`;
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
    // First Receipts step 2 reads the REAL playhead position — this genuine scrub, never a
    // tutorial event. Ticks silently the first time the playhead sits before the log's end.
    this.firstReceipts.noteScrub(clamped, this.events().length);
  }
  onRangeInput(e: Event): void {
    this.setPrefix(Number((e.target as HTMLInputElement).value));
  }
  onRangeGrab(): void {
    clearTimeout(this.releaseTimer);
    this.scrubEl()?.classList.add('dragging');
  }
  onRangeRelease(): void {
    // Zoneless rendering flushes on the NEXT animation frame, so the render a keypress queued can
    // land after its own keyup. Clearing the class synchronously would make that render read "not
    // scrubbing" and skip the follow — a keyboard scrub that never chases its boundary. Deferring
    // the removal past the flush keeps the predicate true for the gesture's own render, and a
    // re-grab (the next arrow keydown) cancels the pending release.
    clearTimeout(this.releaseTimer);
    this.releaseTimer = setTimeout(() => this.scrubEl()?.classList.remove('dragging'), 80);
  }
  private releaseTimer?: ReturnType<typeof setTimeout>;
  private scrubEl(): HTMLElement | undefined {
    return this.stripEl?.nativeElement.closest('.scrub') as HTMLElement | undefined;
  }

  /**
   * Scroll the DOCUMENT so the boundary row sits in a comfortable upper band — never
   * `scrollIntoView`, which can drag an unrelated ancestor's scroll position along with it. It
   * only nudges when the row has drifted OUT of the band, so a steady drag does not thrash the
   * page. `topGuard` clears the sticky column-head (`.lhead`) rather than a hardcoded pixel
   * figure, since it is measured, not assumed. `bandBottom` tracks the scrubber panel's own top
   * IF it is ever docked at the viewport bottom (this build always renders it as the sticky
   * 340px side column, so that branch is inert today but keeps the contract if a narrow layout
   * ever docks it) — otherwise the floor is the viewport bottom. Smooth unless reduced motion.
   */
  private followBoundary(row: HTMLElement): void {
    const r = row.getBoundingClientRect();
    const lhead = document.querySelector('.lhead');
    const topGuard = lhead ? lhead.getBoundingClientRect().bottom + 12 : 130;
    let bandBottom = window.innerHeight;
    const panel = document.querySelector('.cpanel[data-panel="inspector"]');
    if (panel) {
      const p = panel.getBoundingClientRect();
      if (p.top > window.innerHeight * 0.4 && p.bottom >= window.innerHeight - 2) bandBottom = p.top;
    }
    if (r.top >= topGuard && r.bottom <= bandBottom - 8) return; // already comfortably in view
    const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
    const target = r.top + window.scrollY - (topGuard + 40);
    window.scrollTo({ top: Math.max(0, target), behavior: reduced ? 'auto' : 'smooth' });
  }

  foldAll(): void {
    this.setPrefix(this.events().length);
  }

  private scrubTo(seq: number): void {
    this.setPrefix(seq + 1);
  }

  /** Which tick is under `clientX` — asked of the strip's own geometry, not a proportion of a
   *  DOM measurement (the off-by-one fix this replaced). Returns -1 for "off the left edge: fold
   *  nothing".
   *
   *  Pitch is `box.width / n` — the strip's floating-point `getBoundingClientRect().width`
   *  divided by the tick count — matching `estripTickBox`'s own algebra (`event-model.ts`): `n`
   *  equal-share ticks tile the strip, so each occupies `1/n` of it (the tiny density-scaled gap
   *  budget is within ESTRIP_GAP_MAX_PCT ≈ 0.5% and negligible here). This used to be measured
   *  empirically as `ticks[1].offsetLeft - ticks[0].offsetLeft`, but `offsetLeft` is an INTEGER
   *  (rounded to the nearest CSS pixel) — once the geometry fix let a dense run's true per-tick
   *  pitch drop under ~2px (a 206-event run in the Scrubber's ~316px column pitches at ~1.3px),
   *  that rounding was a large fraction of the true pitch, and the quantized value it produced
   *  resolved a center-click on tick i to some OTHER tick — the same off-by-a-few defect the strip
   *  overflow was masking. The analytic pitch has no such rounding: it stays accurate at any n. */
  private tickAt(clientX: number): number {
    const strip = this.stripEl?.nativeElement;
    const n = this.events().length;
    if (!strip || n === 0) return -1;
    const box = strip.getBoundingClientRect();
    if (clientX < box.left) return -1;
    const pitch = box.width / n;
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
    const release = (e: PointerEvent): void => {
      dragging = false;
      this.scrubEl()?.classList.remove('dragging');
      try {
        strip.releasePointerCapture(e.pointerId);
      } catch {
        /* capture may already be gone */
      }
    };
    strip.addEventListener('pointerup', release);
    strip.addEventListener('pointercancel', release);
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
    // the time-travel preview banner's "Jump to live" — folds back to the head of the log
    this.derivedEl?.nativeElement.addEventListener('click', (e) => {
      if ((e.target as HTMLElement).closest('[data-to-live]')) this.foldAll();
    });
    // the fork lineage: open a linked parent or child run in the Inspector
    this.lineageEl?.nativeElement.addEventListener('click', (e) => {
      const b = (e.target as HTMLElement).closest<HTMLElement>('[data-goto-run]');
      const runId = b?.getAttribute('data-goto-run');
      if (runId) this.viewService.openRun(runId);
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

  /** The keyboard skip path (see the `.skip` link in the template): jump focus straight to the
   * scrubber slider, opening the panel first if it was collapsed. Frees a keyboard user from
   * tabbing past every event row and strip tick to reach the slider and the Fork control after it. */
  skipToScrubber(e: Event): void {
    e.preventDefault();
    this.panelOpen.set(true);
    focusWhenRendered('#prefix');
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
