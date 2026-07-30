import {
  AfterViewInit,
  ChangeDetectionStrategy,
  Component,
  DestroyRef,
  ElementRef,
  ViewChild,
  computed,
  effect,
  inject,
  signal,
  untracked,
} from '@angular/core';
import { Router } from '@angular/router';

import {
  CapabilityProbeService,
  GraphRunService,
  GraphsService,
  RunsService,
  type GraphProjection,
  type ForkOutcome,
} from '../../core/api';
import { focusWhenRendered } from '../../core/focus';
import { ForkIntentService, type ForkIntent } from '../../core/fork-intent';
import { ViewService } from '../../core/view';
import { jsonHi } from '../../shared/json-hi';
import { agentIdentity, toRunRow } from '../runs/run-model';
import { loadDrafts, removeDraft, saveDraft } from './wf-draft';
import {
  DUP_STACK_OFFSET,
  NODE_H,
  NODE_W,
  type WfEdgePath,
  type WfView,
  layoutFor,
  wfFit,
  wfReset,
  wfRoutes,
  wfTopo,
  wfZoom,
  zoomPercent,
} from './wf-geometry';
import { type HazardReview, type HazardRow, forkReady, reviewOf } from './wf-hazard';
import {
  type WfEdge,
  type WfGraph,
  type WfNode,
  type WfPickOption,
  fromServerGraph,
  nodeDoes,
  pickOptions,
  shortHash,
} from './wf-model';
import {
  type WfNodeProjection,
  edgeWalked,
  projectNodeStates,
  projectionUsable,
  reachedState,
} from './wf-projection';
import { startRefusal } from './wf-start';
import { type WfError, applyFix, validateGraph, verdictOf } from './wf-validate';

type WfMode = 'build' | 'run';
type WfTool = 'pan' | 'sel';

interface NodeMenu {
  readonly nodeId: string;
  readonly x: number;
  readonly y: number;
}

interface PositionedNode extends WfNode {
  readonly x: number;
  readonly y: number;
  readonly does: string;
  readonly bad: boolean;
  /** The mono field strip — the input keys this node reads, the way a form shows its fields. */
  readonly fields: readonly string[];
  /** The effect class shown on a tool/map's face (read | write | idempotent), when it has one. */
  readonly eff?: string;
  readonly run?: WfNodeProjection;
}

interface PositionedEdge {
  readonly from: string;
  readonly to: string;
  readonly label?: string;
  /** The orthogonal trace, out of the source's right port into the target's left, elbowed. */
  readonly d: string;
  /** The single fine arrowhead at the target port, as its own filled triangle path. */
  readonly arrow: string;
  /** Where the case label sits — ON the straight run, or riding the vertical of an elbow. */
  readonly lx: number;
  readonly ly: number;
  /** An edge out of a branch is a case; it draws in the firmer case ink. */
  readonly isCase: boolean;
  /** Run mode: the recorded route inks; the road not taken ghosts; everything else is quiet. */
  readonly walked: boolean;
  readonly ghost: boolean;
}

/** A canvas caption for a duplicate-id stack: the shared id, how many cards claim it, and where the
 * chip sits (anchored to the top card of the nudged stack). Derived from the validator's
 * `duplicate_id` errors — the one source of truth — so the cue can never disagree with the panel. */
interface DupStack {
  readonly id: string;
  readonly count: number;
  readonly x: number;
  readonly y: number;
  readonly errorIndex: number;
}

interface ListFrom {
  readonly name: string;
  readonly label?: string;
}

interface ListRow {
  readonly node: WfNode;
  readonly does: string;
  readonly bad: boolean;
  readonly froms: readonly ListFrom[];
}

interface MapTransform {
  readonly pad: number;
  readonly s: number;
  readonly ox: number;
  readonly oy: number;
}

interface MapRect {
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
  readonly id?: string;
}

const KIND_BLURB: Record<string, string> = {
  agent: 'a full agent loop, referenced by hash',
  tool: 'one direct tool invocation',
  gate: 'a human approval that suspends the run',
  branch: 'routes on a recorded decision',
  map: 'fans out a sub-run per element of a list',
  fold: 'iterates a body until it converges, then joins the passes',
};

/** The minimap's drawing units — its viewBox. The CSS box is 160x100, so the CTM (not the CSS
 * rect) converts a pointer to these units; letterboxing would otherwise put the view off. */
const MAP_W = 156;
const MAP_H = 92;

/**
 * THE WORKFLOWS CANVAS — the graph authoring + fork surface. Builder and Run modes; a picker over
 * server graphs AND in-browser drafts; pan/zoom with the {@link wfFit} legibility floor; a live
 * minimap (field click jumps, viewport rectangle drags); per-node ⋯ menus; a docked node
 * inspector whose head is the validator's verdict; a narrow-viewport linear list; and the app's
 * ONE modal, the fork hazard review, driven off the REAL fork API.
 *
 * Fork requests from other views (the Inspector's scrubber offer, the Runs detail panel) arrive
 * through {@link ForkIntentService} and are consumed here, so `openFork` stays the only owner of
 * every refusal, whichever door the operator came through.
 */
@Component({
  selector: 'bridge-workflows',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './workflows.html',
})
export class Workflows implements AfterViewInit {
  private readonly graphsService = inject(GraphsService);
  private readonly graphRuns = inject(GraphRunService);
  private readonly runsService = inject(RunsService);
  private readonly capabilityProbe = inject(CapabilityProbeService);
  private readonly forkIntent = inject(ForkIntentService);
  private readonly viewService = inject(ViewService);
  private readonly router = inject(Router);
  private readonly destroyRef = inject(DestroyRef);

  @ViewChild('wfWork') private workRef?: ElementRef<HTMLElement>;
  @ViewChild('wfCanvas') private canvasRef?: ElementRef<HTMLElement>;
  @ViewChild('wfStage') private stageRef?: ElementRef<HTMLElement>;
  @ViewChild('wfMapSvg') private mapSvgRef?: ElementRef<SVGSVGElement>;
  @ViewChild('forkDlg') private forkDlgRef?: ElementRef<HTMLDialogElement>;

  readonly MAP_W = MAP_W;
  readonly MAP_H = MAP_H;

  // ── graph catalog ──
  private readonly serverGraphs = signal<WfGraph[]>([]);
  private readonly drafts = signal<WfGraph[]>([]);
  readonly currentKey = signal<string>('');
  /** Cached run_id → projection for every graph run (fetched once on entry, a handful of runs). */
  private readonly projections = signal<ReadonlyMap<string, GraphProjection>>(new Map());
  private readonly capability = this.capabilityProbe.capabilities;
  private loaded?: Promise<void>;

  // ── canvas state ──
  readonly mode = signal<WfMode>('build');
  readonly tool = signal<WfTool>('pan');
  readonly view = signal<WfView>({ k: 1, x: 0, y: 0 });
  readonly panning = signal(false);
  readonly selectedNode = signal<string | undefined>(undefined);
  readonly runId = signal<string | undefined>(undefined);
  readonly menu = signal<NodeMenu | undefined>(undefined);
  readonly forked = signal<string>('');
  readonly published = signal<string>('');
  readonly started = signal<string>('');
  /** A start is in flight. One graph run per click: the button reads its own state rather than
   * letting an impatient second click spawn a second run of the same document. */
  readonly starting = signal(false);
  readonly panelOpen = signal(true);
  /** The node-picking state: a fork was requested from a surface that names no node (a Runs list
   * row), so the operator picks the fork point here rather than the app guessing one. */
  readonly pickRun = signal<string | undefined>(undefined);
  /** The canvas's viewport size, published by a ResizeObserver so the minimap math is reactive. */
  private readonly canvasSize = signal<{ width: number; height: number }>({ width: 0, height: 0 });

  // ── fork dialog state ──
  readonly forkReview = signal<HazardReview | undefined>(undefined);
  readonly forkAck = signal<ReadonlySet<number>>(new Set());
  readonly forkTitle = signal<string>('Fork this run');
  readonly forkSub = signal<string>('');
  readonly forkNode = signal<string | undefined>(undefined);
  readonly forkOrigin = signal<string | undefined>(undefined);
  readonly forkRefusal = signal<string | undefined>(undefined);

  // ── history (undo/redo over draft edits) ──
  private readonly past = signal<WfGraph[]>([]);
  private readonly future = signal<WfGraph[]>([]);

  readonly currentGraph = computed<WfGraph | undefined>(() => {
    const key = this.currentKey();
    return [...this.drafts(), ...this.serverGraphs()].find((g) => g.key === key);
  });

  readonly pickerOptions = computed<WfPickOption[]>(() =>
    pickOptions(this.drafts(), this.serverGraphs()),
  );

  readonly graphName = computed(() => this.currentGraph()?.name ?? '—');
  readonly graphState = computed(() => this.currentGraph()?.state ?? 'draft');
  readonly graphHash = computed(() => this.currentGraph()?.hash ?? null);

  /** A published graph with no document name reads AS its short hash, so rendering the hash chip
   * next to that title would state the same identity twice. */
  readonly graphHashChip = computed(() => {
    const hash = this.graphHash();
    if (hash === null) return null;
    return this.graphName() === this.hashShort(hash) ? null : hash;
  });
  readonly zoomLabel = computed(() => zoomPercent(this.view().k));

  /** ONE error list per render — the canvas, the list, the panel and the publish gate all read
   * this, so they can never disagree about an offender. */
  readonly errors = computed<readonly WfError[]>(() => {
    const g = this.currentGraph();
    return g ? validateGraph(g) : [];
  });
  readonly verdict = computed(() => verdictOf(this.errors()));
  private readonly badNodes = computed(
    () => new Set(this.errors().filter((e) => e.node !== undefined).map((e) => e.node)),
  );

  readonly publishDisabled = computed(
    () => this.errors().length > 0 || this.graphState() === 'published',
  );
  readonly publishLabel = computed(() => (this.graphState() === 'published' ? 'Published' : 'Publish'));

  /**
   * Starting needs a STORED graph: `POST /v1/graph-runs` takes a hash, and a draft has none until
   * it is published. NOT gated on the `GET /v1/capabilities` probe the fork button reads, and
   * deliberately so: the probe advertises `fork` and nothing else, so it has no honest answer about
   * graph runs, and one is not needed: a hash in this picker came from `GET /v1/graphs`, which is
   * itself the evidence that this server serves the graph surface. A server too old to have it
   * lists no graphs, so there is no hash here to start and the button is disabled by that fact.
   */
  readonly startDisabled = computed(() => this.graphHash() === null || this.starting());
  readonly startLabel = computed(() => (this.starting() ? 'Starting…' : 'Start run'));

  private readonly layout = computed(() => {
    const g = this.currentGraph();
    return g ? layoutFor(g) : {};
  });

  /** The run's projection, when Run mode is on a run of the shown graph. */
  private readonly activeProjection = computed<GraphProjection | undefined>(() => {
    const id = this.runId();
    return id !== undefined ? this.projections().get(id) : undefined;
  });

  private readonly nodeStates = computed<Record<string, WfNodeProjection>>(() => {
    const g = this.currentGraph();
    const proj = this.activeProjection();
    if (this.mode() !== 'run' || !g || !projectionUsable(g, proj)) return {};
    return projectNodeStates(g, proj as GraphProjection);
  });

  readonly nodes = computed<PositionedNode[]>(() => {
    const g = this.currentGraph();
    if (!g) return [];
    const layout = this.layout();
    const states = this.nodeStates();
    const bad = this.badNodes();
    // Two nodes claiming the SAME id cannot be placed apart by an id-keyed layout — that is the
    // duplicate showing its consequence. Nudge the second so both are visible.
    const dupSeen: Record<string, number> = {};
    return g.nodes.map((n) => {
      const seen = (dupSeen[n.id] = (dupSeen[n.id] ?? 0) + 1) - 1;
      const eff = n.effect ?? n.body?.effect;
      return {
        ...n,
        x: (layout[n.id]?.x ?? 0) + seen * DUP_STACK_OFFSET,
        y: (layout[n.id]?.y ?? 0) + seen * DUP_STACK_OFFSET,
        does: nodeDoes(n),
        fields: nodeFields(n),
        ...(eff !== undefined ? { eff } : {}),
        bad: bad.has(n.id),
        run: states[n.id],
      };
    });
  });

  /**
   * THE DUPLICATE-ID STACK CAPTIONS. When two or more nodes claim one id the layout can only place
   * them at one slot, so the renderer nudges each down-and-right into a small stack — and this chip
   * sits on the top card to SAY so, `id n_charge × 2`, in the same danger ink as the error dot, so
   * the overlap reads as a named collision rather than a rendering glitch. Derived straight from the
   * validator's `duplicate_id` errors (the one detection), so it appears only when ids actually
   * collide and never disagrees with the panel; on a clean graph there are none.
   */
  readonly dupStacks = computed<DupStack[]>(() => {
    const g = this.currentGraph();
    if (!g) return [];
    const layout = this.layout();
    const errs = this.errors();
    const stacks: DupStack[] = [];
    errs.forEach((er, errorIndex) => {
      if (er.code !== 'duplicate_id' || er.node === undefined) return;
      const id = er.node;
      const count = g.nodes.filter((n) => n.id === id).length;
      const base = layout[id] ?? { x: 0, y: 0 };
      // Anchor to the TOP card of the nudged stack, the one drawn last and never occluded.
      const top = (count - 1) * DUP_STACK_OFFSET;
      stacks.push({ id, count, x: base.x + top, y: base.y + top, errorIndex });
    });
    return stacks;
  });

  readonly edges = computed<PositionedEdge[]>(() => {
    const g = this.currentGraph();
    if (!g) return [];
    const layout = this.layout();
    const states = this.nodeStates();
    const inRun = this.mode() === 'run' && Object.keys(states).length > 0;
    const branchSources = new Set(g.nodes.filter((n) => n.kind === 'branch').map((n) => n.id));
    // ONE routing pass over the whole graph: a trace keeps its direct elbow unless it would cut
    // through a card, in which case it reroutes through a clear channel below the band. Aligned to
    // g.edges, so an edge whose endpoint is unlaid (a dangling reference) is simply not drawn.
    const routes = wfRoutes(g, layout);
    return g.edges
      .map((e, i) => ({ e, i, path: routes[i] }))
      .filter((r): r is { e: WfEdge; i: number; path: WfEdgePath } => r.path !== undefined)
      .map(({ e, path }) => {
        const isBranch = branchSources.has(e.from);
        const walked = inRun && edgeWalked(e.from, e.to, e.label, isBranch, states);
        // The road not taken: a decided branch's OTHER arm. Ghosted, never deleted — "we did not
        // go that way" is information the canvas keeps rather than a route it quietly drops.
        const s = states[e.from];
        const decided = isBranch && !!s && s.branchCase !== undefined;
        const ghost = inRun && decided && !walked && e.label !== s?.branchCase && reachedState(s?.state);
        const base = {
          from: e.from,
          to: e.to,
          d: path.d,
          arrow: path.arrow,
          lx: path.lx,
          ly: path.ly,
          isCase: e.label !== undefined,
          walked,
          ghost,
        };
        return e.label !== undefined ? { ...base, label: e.label } : base;
      });
  });

  /** The narrow viewport's one-dimensional rendering — the same graph, the same error list, the
   * same selection, in topological reading order with every incoming edge named. */
  readonly listRows = computed<ListRow[]>(() => {
    const g = this.currentGraph();
    if (!g) return [];
    const byId = new Map(g.nodes.map((n) => [n.id, n] as const));
    const bad = this.badNodes();
    return wfTopo(g)
      .map((id) => byId.get(id))
      .filter((n): n is WfNode => n !== undefined)
      .map((n) => ({
        node: n,
        does: nodeDoes(n),
        bad: bad.has(n.id),
        froms: g.edges
          .filter((e) => e.to === n.id)
          .map((e) =>
            e.label !== undefined
              ? { name: byId.get(e.from)?.name ?? e.from, label: e.label }
              : { name: byId.get(e.from)?.name ?? e.from },
          ),
      }));
  });

  readonly selectedNodeObj = computed<WfNode | undefined>(() => {
    const id = this.selectedNode();
    return id !== undefined ? this.currentGraph()?.nodes.find((n) => n.id === id) : undefined;
  });

  readonly readOnly = computed(() => this.graphState() === 'published');

  readonly panelTitle = computed(() => this.selectedNodeObj()?.name ?? 'Inspector');
  readonly panelSub = computed(() => {
    const n = this.selectedNodeObj();
    if (!n) return this.verdict();
    return `${n.kind} · ${
      this.readOnly()
        ? 'published, so this graph is frozen'
        : 'draft — edits change the bytes, and the bytes are the identity'
    }`;
  });

  /** The runs of the CURRENTLY shown graph — a graph run whose projection names this graph. A
   * hashless draft has none, so its picker is empty and Run mode has nothing to ink (honest). */
  private readonly runsOfGraph = computed<string[]>(() => {
    const g = this.currentGraph();
    if (!g?.hash) return [];
    const out: string[] = [];
    for (const [id, proj] of this.projections()) if (proj.graphHash === g.hash) out.push(id);
    return out;
  });

  /** The run picker's options — the runs OF THE SHOWN GRAPH only, each with its derived status,
   * newest-first as the projection cache holds them. Ported from the prototype's `renderWfRuns`,
   * which lists `runsOfGraph(wfHash)`, NOT every graph run in the system: a run of a DIFFERENT
   * graph could never project onto this one, so offering it is the "Run does nothing" defect. */
  readonly graphRunOptions = computed(() => {
    const byId = new Map(this.runsService.runs().map((s) => toRunRow(s)).map((r) => [r.id, r] as const));
    return this.runsOfGraph().map((id) => ({
      id,
      label: id.slice(0, 8),
      status: byId.get(id)?.status ?? '—',
    }));
  });

  readonly stageTransform = computed(() => {
    const v = this.view();
    return `translate(${v.x}px, ${v.y}px) scale(${v.k})`;
  });

  // ── minimap projection: graph coordinates → map units, published so the pointer handlers run
  //    the exact inverse of what was drawn ──
  readonly mapT = computed<MapTransform | undefined>(() => {
    const g = this.currentGraph();
    if (!g || g.nodes.length === 0) return undefined;
    const layout = this.layout();
    const xs = g.nodes.map((n) => layout[n.id]?.x ?? 0);
    const ys = g.nodes.map((n) => layout[n.id]?.y ?? 0);
    const ox = Math.min(...xs);
    const oy = Math.min(...ys);
    const gw = Math.max(...xs) + NODE_W - ox;
    const gh = Math.max(...ys) + NODE_H - oy;
    const pad = 6;
    const s = Math.min((MAP_W - pad * 2) / gw, (MAP_H - pad * 2) / gh);
    return { pad, s, ox, oy };
  });

  readonly minimapHidden = computed(() => {
    // The minimap is furniture unless the graph overruns the viewport at the current zoom.
    const g = this.currentGraph();
    const box = this.canvasSize();
    if (!g || g.nodes.length === 0 || box.width === 0) return true;
    const layout = this.layout();
    const xs = g.nodes.map((n) => layout[n.id]?.x ?? 0);
    const ys = g.nodes.map((n) => layout[n.id]?.y ?? 0);
    const gw = (Math.max(...xs) + NODE_W - Math.min(...xs)) * this.view().k;
    const gh = (Math.max(...ys) + NODE_H - Math.min(...ys)) * this.view().k;
    return gw <= box.width && gh <= box.height;
  });

  readonly mapNodes = computed<MapRect[]>(() => {
    const t = this.mapT();
    const g = this.currentGraph();
    if (!t || !g) return [];
    const layout = this.layout();
    return g.nodes.map((n) => ({
      id: n.id,
      x: t.pad + ((layout[n.id]?.x ?? 0) - t.ox) * t.s,
      y: t.pad + ((layout[n.id]?.y ?? 0) - t.oy) * t.s,
      w: NODE_W * t.s,
      h: NODE_H * t.s,
    }));
  });

  readonly mapView = computed<MapRect | undefined>(() => {
    const t = this.mapT();
    const box = this.canvasSize();
    if (!t || box.width === 0) return undefined;
    const v = this.view();
    return {
      x: t.pad + (-v.x / v.k - t.ox) * t.s,
      y: t.pad + (-v.y / v.k - t.oy) * t.s,
      w: (box.width / v.k) * t.s,
      h: (box.height / v.k) * t.s,
    };
  });

  constructor() {
    // The canvas is MOUNTED from boot (its dialog, and fork intents, need it present), but its
    // data is loaded on first genuine need — entering the view, or a fork intent — so every other
    // view's page load is not taxed with graph documents and run projections it never shows.
    effect(() => {
      if (this.viewService.view() !== 'workflows') return;
      untracked(() => {
        this.ensureLoaded();
        // The canvas fills the viewport, and only the runtime knows how much chrome sits above it
        // (the topbar wraps), so its height is measured after the view is on screen — then fit.
        requestAnimationFrame(() => {
          this.sizeCanvas();
          this.fit();
        });
      });
    });
    // THE RULED GROUND is drawn BY the canvas, so it must be told the pan and the zoom. Reactive
    // on the view: the grid moves with the pan (background-position) and scales with the zoom
    // (background-size), fading out as you zoom out rather than turning into a grey fog. Ported
    // from `wfGround`. Written to :root so the pure-CSS `.wf-canvas` background reads it.
    effect(() => {
      const v = this.view();
      const fade = Math.max(0, Math.min(1, (v.k - 0.35) / 0.45));
      const s = document.documentElement.style;
      s.setProperty('--wf-minor', `${24 * v.k}px`);
      s.setProperty('--wf-major', `${120 * v.k}px`);
      s.setProperty('--wf-ox', `${v.x}px`);
      s.setProperty('--wf-oy', `${v.y}px`);
      s.setProperty('--wf-rule-1', `color-mix(in oklab, var(--fg), transparent ${100 - 6 * fade}%)`);
      s.setProperty('--wf-rule-5', `color-mix(in oklab, var(--fg), transparent ${100 - 14 * fade}%)`);
    });
    // Fork requests from the Inspector or the Runs panel land whenever their view fires them;
    // consuming is untracked so acting on one never re-runs under its own signal writes.
    effect(() => {
      const intent = this.forkIntent.intent();
      if (!intent) return;
      untracked(() => void this.consumeIntent(intent));
    });
  }

  private ensureLoaded(): Promise<void> {
    this.loaded ??= this.load();
    return this.loaded;
  }

  ngAfterViewInit(): void {
    const canvas = this.canvasEl();
    if (!canvas) return;
    const publish = (): void => {
      const box = canvas.getBoundingClientRect();
      this.canvasSize.set({ width: box.width, height: box.height });
    };
    publish();
    if (typeof ResizeObserver !== 'undefined') {
      const ro = new ResizeObserver(publish);
      ro.observe(canvas);
      this.destroyRef.onDestroy(() => ro.disconnect());
    }
    // The fill height is a function of the window, not of the canvas box, so a window resize
    // (which the ResizeObserver above does not see until AFTER the reflow) re-measures it. Keep
    // the current zoom — a resize is not a request to refit — exactly as the prototype's listener.
    const onResize = (): void => this.sizeCanvas();
    window.addEventListener('resize', onResize);
    this.destroyRef.onDestroy(() => window.removeEventListener('resize', onResize));
  }

  /** Fill the canvas to the bottom of the viewport. Only the runtime knows how much chrome sits
   * above it (the topbar wraps), so this is measured, not a CSS constant. Ported from
   * `wfSizeCanvas`; a no-op below the breakpoint, where the canvas is display:none. */
  private sizeCanvas(): void {
    const canvas = this.canvasEl();
    if (!canvas || getComputedStyle(canvas).display === 'none') return;
    const top = canvas.getBoundingClientRect().top;
    const h = Math.max(360, Math.round(window.innerHeight - top - 24));
    document.documentElement.style.setProperty('--wf-h', `${h}px`);
  }

  private async load(): Promise<void> {
    this.drafts.set(loadDrafts());
    // Probe fork capability for the canvas's own fork entry and the app-wide offers.
    void this.capabilityProbe.probe();
    // Server graphs.
    try {
      const summaries = await this.graphsService.refresh();
      const docs = await Promise.all(
        summaries.map(async (s) => {
          try {
            const rec = await this.graphsService.get(s.graph);
            return fromServerGraph(s.graph, rec.document, s);
          } catch {
            return undefined;
          }
        }),
      );
      this.serverGraphs.set(docs.filter((g): g is WfGraph => g !== undefined));
    } catch {
      /* no server graphs reachable — the picker still offers the local drafts */
    }
    // Default selection: a server graph if any (it has runs to project), else the first draft.
    if (!this.currentKey()) {
      const first = this.serverGraphs()[0]?.key ?? this.drafts()[0]?.key ?? '';
      this.currentKey.set(first);
    }
    // Entry fit NOW, before the slower projection reads: the microtask runs before the first
    // paint of the drawn nodes, so the operator (or a real pointer) can never slip a zoom in
    // between the nodes appearing and the fit landing — a fit that arrived after the projections
    // would quietly reset a zoom they had already chosen.
    queueMicrotask(() => this.fit());
    // Graph-run projections (a handful), so Run mode and the node menu know which runs are real.
    try {
      const runs = await this.runsService.refresh();
      const graphRuns = runs
        .map((s) => toRunRow(s))
        .filter((r) => agentIdentity(r).kind === 'graph')
        .map((r) => r.id);
      const map = new Map<string, GraphProjection>();
      await Promise.all(
        graphRuns.map(async (id) => {
          try {
            map.set(id, await this.graphRuns.loadProjection(id));
          } catch {
            /* a run whose projection cannot be read is simply absent from the picker's usable set */
          }
        }),
      );
      this.projections.set(map);
    } catch {
      /* runs unreachable — Run mode has nothing to project, which the empty picker states */
    }
  }

  private canvasEl(): HTMLElement | undefined {
    return this.canvasRef?.nativeElement;
  }

  // ── picker ──
  pick(value: string): void {
    this.currentKey.set(value);
    this.selectedNode.set(undefined);
    this.menu.set(undefined);
    // A banner belongs to the moment it was made; changing graphs changes what you look at.
    this.forked.set('');
    this.published.set('');
    this.started.set('');
    // A run only projects onto the graph it ran. Leaving Run mode on a graph with no runs keeps
    // the canvas honest; on a graph that HAS runs, re-anchor to one of ITS runs rather than
    // inking the previous graph's — the exact `if (!runs.includes(wfRunId)) wfRunId = runs[0]`
    // the prototype's renderWfRuns does, so the picker can never point at a foreign run.
    if (this.mode() === 'run') {
      const runs = this.runsOfGraph();
      if (runs.length === 0) this.setMode('build');
      else if (!runs.includes(this.runId() ?? '')) this.runId.set(runs[0]);
    }
    queueMicrotask(() => this.fit());
  }

  onPick(event: Event): void {
    this.pick((event.target as HTMLSelectElement).value);
  }

  // ── mode + tool ──
  setMode(mode: WfMode): void {
    this.mode.set(mode);
    this.menu.set(undefined);
    // Run mode projects a run OF THIS GRAPH. Anchor to one of its own runs (the newest), never a
    // run left selected from another graph — the source of the "Run does nothing" report, where
    // the picker offered a foreign run whose projection could never match this graph.
    if (mode === 'run') {
      const runs = this.runsOfGraph();
      if (!this.runId() || !runs.includes(this.runId() as string)) this.runId.set(runs[0]);
    }
  }
  setTool(tool: WfTool): void {
    this.tool.set(tool);
  }
  onPickRun(event: Event): void {
    this.runId.set((event.target as HTMLSelectElement).value);
  }

  // ── zoom / fit ──
  private center(): { cx: number; cy: number } {
    const box = this.canvasEl()?.getBoundingClientRect();
    return { cx: (box?.width ?? 800) / 2, cy: (box?.height ?? 600) / 2 };
  }
  zoomIn(): void {
    const { cx, cy } = this.center();
    this.view.update((v) => wfZoom(v, 1.25, cx, cy));
  }
  zoomOut(): void {
    const { cx, cy } = this.center();
    this.view.update((v) => wfZoom(v, 1 / 1.25, cx, cy));
  }
  fit(): void {
    const g = this.currentGraph();
    const box = this.canvasEl()?.getBoundingClientRect();
    if (!g || !box || box.width === 0) return;
    this.view.set(wfFit(g, this.layout(), { width: box.width, height: box.height }));
  }
  reset(): void {
    const g = this.currentGraph();
    const box = this.canvasEl()?.getBoundingClientRect();
    if (!g || !box) return;
    this.view.set(wfReset(g, this.layout(), { width: box.width, height: box.height }));
  }

  /**
   * Bring a node to the middle at a legible zoom (k in [1, 1.4], a deeper operator-chosen zoom
   * left alone). Centres against the width the canvas SETTLES to once the panel column finishes
   * animating open (340px + the grid gap), never the transient mid-animation width — measuring
   * the live box would leave the node ~150px off when the panel lands.
   */
  private centerOn(id: string): void {
    const at = this.layout()[id];
    const canvas = this.canvasEl();
    if (!at || !canvas) return;
    // Below the breakpoint the canvas is display:none — its box is 0x0 and every number computed
    // from it would be a measurement of nothing. The narrow list still opens the panel.
    if (getComputedStyle(canvas).display === 'none') return;
    const box = canvas.getBoundingClientRect();
    const work = this.workRef?.nativeElement;
    const wide = typeof matchMedia !== 'undefined' ? matchMedia('(min-width: 1101px)').matches : true;
    const gap = wide && work ? parseFloat(getComputedStyle(work).columnGap) || 24 : 0;
    const panelW = wide ? 340 : 0; // a selection opens the panel, so settle to the open width
    const w = Math.max(200, (work?.clientWidth ?? box.width) - panelW - (panelW ? gap : 0));
    const k = Math.min(1.4, Math.max(this.view().k, 1));
    this.animateView({
      k,
      x: w / 2 - (at.x + NODE_W / 2) * k,
      y: box.height / 2 - (at.y + NODE_H / 2) * k,
    });
  }

  private settleTimer?: ReturnType<typeof setTimeout>;
  /** Ease the stage to a new view. The inline transition is cleared once settled so "the ease is
   * done" stays observable; reduced-motion gets the same landing instantly. */
  private animateView(v: WfView): void {
    const stage = this.stageRef?.nativeElement;
    const reduced =
      typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
    if (stage && !reduced) {
      stage.style.transition = 'transform 320ms var(--ease-standard)';
      clearTimeout(this.settleTimer);
      this.settleTimer = setTimeout(() => {
        stage.style.transition = '';
      }, 360);
    }
    this.view.set(v);
  }

  // ── pan (PAN_BLOCK: a pointerdown on a control never starts a canvas pan) ──
  private panStart?: { px: number; py: number; vx: number; vy: number };
  // A pan starts ONLY on the empty canvas paper. Every node and every floating control is blocked
  // here so a pointerdown on it selects/acts (and is never captured by the canvas). A blocklist of
  // everything interactive, so a control added later is exempt by default rather than dead by
  // default. Must not test [tabindex]: the canvas itself carries tabindex="-1".
  private static readonly PAN_BLOCK =
    '.wf-node, .wf-chips, .wf-bar, .wf-map, .wf-menu, .wf-note, [data-more], [role="button"], button, select, input, label';

  onCanvasPointerdown(event: PointerEvent): void {
    const target = event.target as HTMLElement;
    if (target.closest(Workflows.PAN_BLOCK)) return; // a control: never pan
    // Dragging the empty paper pans in BOTH tools — the prototype's pan handler never consults
    // the tool. Select is a cursor affordance (the arrow says "click to select" where Pan's grab
    // says "drag the ground"); it does not disable the pan gesture. Gating pan on the tool was the
    // build's own divergence, and the "Select does nothing" the operator felt from it.
    this.panning.set(true);
    const v = this.view();
    this.panStart = { px: event.clientX, py: event.clientY, vx: v.x, vy: v.y };
    (event.currentTarget as HTMLElement).setPointerCapture?.(event.pointerId);
  }
  onCanvasPointermove(event: PointerEvent): void {
    if (!this.panning() || !this.panStart) return;
    const dx = event.clientX - this.panStart.px;
    const dy = event.clientY - this.panStart.py;
    this.view.update((v) => ({ ...v, x: this.panStart!.vx + dx, y: this.panStart!.vy + dy }));
  }
  onCanvasPointerup(): void {
    this.panning.set(false);
    this.panStart = undefined;
  }

  // ── minimap: the field jumps, the viewport rectangle drags — one gesture, one path ──
  private mapDrag?: { gx: number; gy: number };

  /** The pointer in the map's own drawing units, through the CTM — the svg's viewBox and CSS box
   * have different aspect ratios, so a rect-relative read would be off by the letterbox. */
  private mapPoint(event: PointerEvent): { x: number; y: number } | undefined {
    const svg = this.mapSvgRef?.nativeElement;
    const m = svg?.getScreenCTM();
    if (!svg || !m) return undefined;
    const p = new DOMPoint(event.clientX, event.clientY).matrixTransform(m.inverse());
    return { x: p.x, y: p.y };
  }

  private mapPanTo(ux: number, uy: number): void {
    const t = this.mapT();
    const d = this.mapDrag;
    if (!t || !d) return;
    const vx = (ux - d.gx - t.pad) / t.s + t.ox;
    const vy = (uy - d.gy - t.pad) / t.s + t.oy;
    this.view.update((v) => ({ ...v, x: -vx * v.k, y: -vy * v.k }));
  }

  onMapPointerdown(event: PointerEvent): void {
    const t = this.mapT();
    const p = this.mapPoint(event);
    if (!t || !p) return;
    const target = event.target as Element;
    const mv = this.mapView();
    if (target.classList.contains('mm-view') && mv) {
      // drag: keep the offset the rectangle was grabbed by, so it does not jump under the pointer
      this.mapDrag = { gx: p.x - mv.x, gy: p.y - mv.y };
    } else {
      // the field: centre the view on the point pressed, then keep dragging from there
      const box = this.canvasSize();
      this.mapDrag = {
        gx: ((box.width / this.view().k) * t.s) / 2,
        gy: ((box.height / this.view().k) * t.s) / 2,
      };
      this.mapPanTo(p.x, p.y);
    }
    (event.currentTarget as HTMLElement).setPointerCapture?.(event.pointerId);
    event.preventDefault(); // no text-selection sweep across the map
  }
  onMapPointermove(event: PointerEvent): void {
    if (!this.mapDrag) return;
    const p = this.mapPoint(event);
    if (p) this.mapPanTo(p.x, p.y);
  }
  onMapPointerup(): void {
    this.mapDrag = undefined;
  }

  // ── node selection + activation ──
  /** What activating a node MEANS, in one place, so the canvas and the narrow list cannot drift:
   * in the picking state the click names the fork point and openFork takes it from there
   * (including refusing a node the run never entered); otherwise it selects. */
  nodeActivate(id: string): void {
    const origin = this.pickRun();
    if (origin !== undefined) {
      this.pickRun.set(undefined);
      this.selectNode(id);
      void this.openFork(origin, id);
      return;
    }
    this.selectNode(id);
  }

  /** Select a node: one selection, both surfaces. Selecting REVEALS — the panel may have been
   * dismissed, and a selection is a request to see the thing, so it comes back. */
  selectNode(id: string): void {
    this.selectedNode.set(id);
    this.panelOpen.set(true);
    this.centerOn(id);
  }
  nodePressed(id: string): boolean {
    return this.selectedNode() === id;
  }

  /** Activate a duplicate-stack caption: select the shared node, open the inspector, and focus the
   * validator's own `duplicate_id` error for it — the same error, with its rename fix, that gates
   * publish. The chip is a shortcut INTO the one error list, never a second explanation of it. */
  openDupError(stack: DupStack): void {
    this.selectNode(stack.id);
    focusWhenRendered(`.wf-errs [data-fix="${stack.errorIndex}"]`);
  }
  nodeStateClass(n: PositionedNode): string {
    return n.run ? `is-${n.run.state}` : '';
  }

  /** Tab can move focus to a node that is off-screen. The browser would normally scroll it into
   * view — but this stage is transformed and the canvas is `overflow: clip` (never a scroll
   * container), so panning is the right instrument. */
  onNodesFocusin(event: FocusEvent): void {
    const el = (event.target as HTMLElement).closest<HTMLElement>('.wf-node');
    if (el) this.ensureVisible(el);
  }
  private ensureVisible(el: HTMLElement): void {
    const canvas = this.canvasEl();
    if (!canvas) return;
    const box = canvas.getBoundingClientRect();
    const r = el.getBoundingClientRect();
    const pad = 24;
    let dx = 0;
    let dy = 0;
    if (r.left < box.left + pad) dx = box.left + pad - r.left;
    if (r.right > box.right - pad) dx = box.right - pad - r.right;
    if (r.top < box.top + pad) dy = box.top + pad - r.top;
    if (r.bottom > box.bottom - pad) dy = box.bottom - pad - r.bottom;
    if (!dx && !dy) return;
    this.view.update((v) => ({ ...v, x: v.x + dx, y: v.y + dy }));
  }

  // ── the ⋯ menu ──
  openMenu(event: Event, id: string): void {
    event.stopPropagation();
    const canvas = this.canvasEl()?.getBoundingClientRect();
    const btn = (event.currentTarget as HTMLElement).getBoundingClientRect();
    if (!canvas) return;
    this.menu.set({ nodeId: id, x: btn.left - canvas.left - 150, y: btn.top - canvas.top + 24 });
    // Focus the first ENABLED menu item once it renders (zoneless: next frame, not this microtask).
    // Focusing the disabled Fork entry would trap the keyboard on a dead control.
    focusWhenRendered('.wf-menu button:not([disabled])');
  }
  onMoreKeydown(event: KeyboardEvent, id: string): void {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    event.stopPropagation();
    this.openMenu(event, id);
  }
  closeMenu(): void {
    this.menu.set(undefined);
  }
  onMenuKeydown(event: KeyboardEvent): void {
    if (event.key !== 'Escape') return;
    event.preventDefault();
    event.stopPropagation();
    this.closeMenu();
  }

  /** The one derived fact the node menu's three fork states read: is this graph forkable now? */
  private inRun(): boolean {
    const g = this.currentGraph();
    return this.mode() === 'run' && !!g && projectionUsable(g, this.activeProjection());
  }
  private hasRun(): boolean {
    return this.runsOfGraph().length > 0;
  }
  /** The fork menu entry's state — acts, switches, or explains. Mirrors the prototype's wfOpenMenu. */
  forkMenuState(): 'act' | 'switch' | 'disabled' {
    if (this.inRun()) return 'act';
    if (this.hasRun()) return 'switch';
    return 'disabled';
  }

  menuFork(): void {
    const id = this.menu()?.nodeId;
    this.closeMenu();
    if (id !== undefined) void this.openFork(this.runId(), id);
  }
  menuForkSwitch(): void {
    const id = this.menu()?.nodeId;
    const runs = this.runsOfGraph();
    if (!runs.includes(this.runId() ?? '')) this.runId.set(runs[0]);
    this.setMode('run');
    if (id !== undefined) this.selectedNode.set(id);
    this.closeMenu();
  }
  /** The canvas is a summary; the timeline is the record. Open it on the projected run. */
  menuEvents(): void {
    const runId = this.runId();
    this.closeMenu();
    if (runId !== undefined) this.viewService.openRun(runId);
  }
  menuCopy(): void {
    const id = this.menu()?.nodeId;
    if (id !== undefined && navigator.clipboard) void navigator.clipboard.writeText(id);
    this.closeMenu();
  }

  // ── the app-wide fork door: Inspector + Runs requests land here ──
  private async consumeIntent(intent: ForkIntent): Promise<void> {
    this.forkIntent.clear();
    await this.ensureLoaded();
    let proj = this.projections().get(intent.runId);
    if (!proj) {
      try {
        proj = await this.graphRuns.loadProjection(intent.runId);
        const got = proj;
        this.projections.update((m) => new Map(m).set(intent.runId, got));
      } catch {
        /* not a graph run, or its projection is unreadable — openFork below reports the refusal */
      }
    }
    if (proj) {
      const hash = proj.graphHash;
      if ([...this.drafts(), ...this.serverGraphs()].some((g) => g.key === hash)) {
        if (this.currentKey() !== hash) this.pick(hash);
      }
      this.runId.set(intent.runId);
      this.setMode('run');
      // The canonical URL — byte-identical to where a canvas fork leaves you.
      void this.router.navigate(['/workflows', hash.replace(/^sha256:/, '').slice(0, 12)]);
    }
    if (intent.nodeId !== undefined) {
      this.selectNode(intent.nodeId);
      void this.openFork(intent.runId, intent.nodeId);
    } else {
      // The caller names no node, so the operator picks one — never guessed for them.
      this.pickRun.set(intent.runId);
    }
  }

  forkPickCancel(): void {
    this.pickRun.set(undefined);
  }

  // ── the fork dialog: dry-run preview, acknowledge, resubmit ──
  async openFork(runId: string | undefined, nodeId: string): Promise<void> {
    if (runId === undefined) return;
    this.forkNode.set(nodeId);
    this.forkOrigin.set(runId);
    this.forkAck.set(new Set());
    this.forkRefusal.set(undefined);
    this.forked.set('');
    const node = this.currentGraph()?.nodes.find((n) => n.id === nodeId);
    this.forkTitle.set(`Fork at ${node?.name ?? nodeId}`);
    this.forkSub.set(
      `origin ${runId.slice(0, 8)} · node ${nodeId} · graph ${(this.currentGraph()?.hash ?? '').slice(0, 15)}`,
    );
    this.openDialog();
    try {
      const preview = await this.graphRuns.fork(runId, { fromNode: nodeId, dryRun: true });
      this.forkReview.set(reviewOf(preview));
    } catch (err) {
      // A structural refusal (parked origin, bad node) has nothing ack-able to render.
      this.forkReview.set({ hazards: [], unacknowledged: [], refused: true });
      this.forkTitle.set('This run cannot be forked here');
      this.forkRefusal.set(err instanceof Error ? err.message : String(err));
    }
  }

  toggleAck(seq: number, checked: boolean): void {
    this.forkAck.update((set) => {
      const next = new Set(set);
      if (checked) next.add(seq);
      else next.delete(seq);
      return next;
    });
  }

  readonly forkGoDisabled = computed(() => {
    const review = this.forkReview();
    return !review || review.refused || !forkReady(review, this.forkAck());
  });

  readonly forkCountLabel = computed(() => {
    const review = this.forkReview();
    if (!review || review.refused) return '';
    const need = review.unacknowledged.length;
    const have = review.unacknowledged.filter((s) => this.forkAck().has(s)).length;
    return need ? `${have} of ${need} write${need === 1 ? '' : 's'} acknowledged` : 'nothing to acknowledge';
  });

  async confirmFork(): Promise<void> {
    const runId = this.forkOrigin() ?? this.runId();
    const nodeId = this.forkNode();
    if (runId === undefined || nodeId === undefined) return;
    const outcome: ForkOutcome = await this.graphRuns.fork(runId, {
      fromNode: nodeId,
      acknowledgeWrites: [...this.forkAck()],
    });
    if (outcome.kind === 'forked') {
      this.closeDialog();
      this.forked.set(`forked ${outcome.run.slice(0, 8)} from ${runId.slice(0, 8)} at ${nodeId}`);
      // The new run is real; fold it into the projection cache and the picker.
      try {
        const originProj = this.projections().get(runId);
        if (originProj) this.projections.update((m) => new Map(m).set(outcome.run, originProj));
        void this.runsService.refresh();
      } catch {
        /* the confirmation stands even if the follow-up refresh fails */
      }
    } else if (outcome.kind === 'hazard') {
      // Resubmitting under-acknowledged: re-surface the outstanding writes rather than silently fail.
      this.forkReview.set(reviewOf(outcome));
    }
  }

  private openDialog(): void {
    const dlg = this.forkDlgRef?.nativeElement;
    if (dlg && !dlg.open) dlg.showModal();
  }
  closeDialog(): void {
    const dlg = this.forkDlgRef?.nativeElement;
    if (dlg?.open) dlg.close();
  }

  // ── the docked node inspector panel ──
  openPanel(): void {
    this.panelOpen.set(true);
    focusWhenRendered('.cpanel[data-panel="workflows"] .cpanel-x');
  }
  closePanel(): void {
    this.panelOpen.set(false);
    focusWhenRendered('.cpanel-tab[data-panel="workflows"]');
  }

  kindBlurb(kind: string): string {
    return KIND_BLURB[kind] ?? '';
  }
  /** A run-state class rendered as its label: `not-reached` reads as `not reached`. */
  runLabel(state: string): string {
    return state.replace(/-/g, ' ');
  }
  /** The run-mode iteration progress a fold node shows from its recorded projection: how many
   * passes have joined of those started, and — once the loop settled — which pass won. Empty
   * string when the node is not a fold, or the walk has not reached it. */
  foldRun(n: PositionedNode): string {
    const fold = n.run?.fold;
    if (n.kind !== 'fold' || !fold) return '';
    if (fold.converged) return `converged on pass ${fold.converged.winner_index + 1}`;
    const joined = fold.iterations.filter((it) => it.joined).length;
    const started = fold.iterations.length;
    return started ? `pass ${joined}/${started}` : 'starting';
  }
  errWhere(er: WfError): string {
    const g = this.currentGraph();
    if (er.node !== undefined) return `node ${er.node}`;
    const e = er.edge !== undefined ? g?.edges[er.edge] : undefined;
    return `edge ${(er.edge ?? 0) + 1} · ${e?.from ?? '?'} → ${e?.to ?? '?'}`;
  }

  // ── draft editing: every mutation snapshots history and persists through the draft store ──
  private updateDraft(mutate: (g: WfGraph) => WfGraph): void {
    const g = this.currentGraph();
    if (!g || g.state !== 'draft') return;
    this.past.update((p) => [...p, g]);
    this.future.set([]);
    this.drafts.set(saveDraft(mutate(g)));
  }

  applyFixAt(index: number): void {
    const er = this.errors()[index];
    if (er?.fix) this.updateDraft((g) => applyFix(g, er.fix!));
  }

  private editSelected(patch: (n: WfNode) => WfNode): void {
    const id = this.selectedNode();
    if (id === undefined) return;
    this.updateDraft((g) => ({ ...g, nodes: g.nodes.map((n) => (n.id === id ? patch(n) : n)) }));
  }
  editName(event: Event): void {
    const v = (event.target as HTMLInputElement).value;
    this.editSelected((n) => ({ ...n, name: v }));
  }
  editHash(event: Event): void {
    const v = (event.target as HTMLInputElement).value.trim();
    this.editSelected((n) => ({ ...n, agentHash: v }));
  }
  editEffect(event: Event): void {
    const v = (event.target as HTMLSelectElement).value;
    this.editSelected((n) => ({ ...n, effect: v }));
  }
  editConcurrency(event: Event): void {
    const v = Number((event.target as HTMLInputElement).value);
    this.editSelected((n) => ({ ...n, concurrency: v }));
  }

  // ── save (local draft) / publish (POST /v1/graphs) ──
  save(): void {
    const g = this.currentGraph();
    if (!g || g.state !== 'draft') return;
    this.drafts.set(saveDraft(g));
    this.published.set(`draft saved · ${g.name}`);
  }

  async publish(): Promise<void> {
    const g = this.currentGraph();
    if (!g || g.state !== 'draft' || this.errors().length > 0) return;
    // A start banner belongs to the graph it was made on; publishing moves the canvas to a new one.
    this.started.set('');
    try {
      const hash = await this.graphsService.submit(toServerDocument(g));
      this.drafts.set(removeDraft(g.key));
      await this.load();
      this.currentKey.set(hash);
      this.published.set('published');
    } catch (err) {
      this.published.set(`publish refused — ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  // ── start a run of the published document (POST /v1/graph-runs) ──

  /**
   * Start a run of the graph on screen and land the canvas on it: the run id is reported at once
   * (fire-and-return, exactly as an agent run is), then its projection is folded into the cache so
   * the picker holds it and Run mode inks the walk instead of leaving the operator to go find it.
   *
   * Everything the document references is resolved server-side BEFORE the run is spawned, so a
   * reference that cannot resolve comes back as a refusal with no run id. Those refusals are
   * reported verbatim and then EXPLAINED (see {@link startRefusal}), because the two that a
   * default server produces are facts about the server, not mistakes in the graph.
   */
  async startRun(): Promise<void> {
    const hash = this.currentGraph()?.hash;
    if (!hash || this.starting()) return;
    this.starting.set(true);
    this.started.set('');
    this.published.set('');
    try {
      const start = await this.graphRuns.start(hash);
      this.started.set(`run ${start.run.slice(0, 8)} started · ${start.status}`);
      try {
        const projection = await this.graphRuns.loadProjection(start.run);
        this.projections.update((m) => new Map(m).set(start.run, projection));
      } catch {
        /* the run is real even if its first projection read is not; the picker refresh below still
           finds it, and Run mode simply has nothing to ink yet */
      }
      void this.runsService.refresh();
      this.runId.set(start.run);
      this.setMode('run');
    } catch (err) {
      this.started.set(startRefusal(err));
    } finally {
      this.starting.set(false);
    }
  }


  // ── history ──
  canUndo(): boolean {
    return this.past().length > 0;
  }
  canRedo(): boolean {
    return this.future().length > 0;
  }
  undo(): void {
    const past = this.past();
    const g = this.currentGraph();
    if (!past.length || !g) return;
    const prev = past[past.length - 1];
    this.past.set(past.slice(0, -1));
    this.future.update((f) => [...f, g]);
    this.drafts.set(saveDraft(prev));
  }
  redo(): void {
    const future = this.future();
    const g = this.currentGraph();
    if (!future.length || !g) return;
    const next = future[future.length - 1];
    this.future.set(future.slice(0, -1));
    this.past.update((p) => [...p, g]);
    this.drafts.set(saveDraft(next));
  }

  // ── template helpers ──
  hazardRows(): readonly HazardRow[] {
    return this.forkReview()?.hazards ?? [];
  }
  isAcked(seq: number): boolean {
    return this.forkAck().has(seq);
  }
  /** Escaped + token-highlighted JSON for the hazard review and the panel pres — one helper
   * (`jsonHi`) across the whole app, copy-faithful at indent 1. */
  jsonHtml(value: unknown): string {
    return jsonHi(value, 1);
  }
  hashShort(hash: string): string {
    return shortHash(hash);
  }
  capabilityFork(): boolean {
    return this.capability().fork;
  }
}

/** The mono field strip a node card carries — the keys it actually reads, the way a form shows its
 * fields. Ported from the prototype's per-kind `fields` derivation; an agent has none inline. */
function nodeFields(n: WfNode): string[] {
  switch (n.kind) {
    case 'tool':
      return Object.keys((n.input as Record<string, unknown> | undefined) ?? {});
    case 'gate': {
      const schema = n.inputSchema as { properties?: Record<string, unknown> } | undefined;
      return Object.keys(schema?.properties ?? {});
    }
    case 'branch':
      return [...(n.cases ?? [])];
    case 'map':
      return [String(n.over ?? '').replace(/[${}]/g, '')];
    case 'fold':
      return [String(n.stopWhen ?? '').replace(/[${}]/g, '')];
    default:
      return [];
  }
}

/** Convert a canvas draft back to a server document for publish — `{ kind, payload }` per node,
 * carrying every field the draft genuinely holds and the minimum each kind requires. */
function toServerDocument(g: WfGraph): import('@salvor-run/client').Graph {
  const nodes = g.nodes.map((n) => {
    switch (n.kind) {
      case 'agent':
        return {
          kind: 'agent' as const,
          payload: {
            id: n.id,
            agent_hash:
              n.agentHash ?? 'sha256:0000000000000000000000000000000000000000000000000000000000000000',
          },
        };
      case 'tool':
        return { kind: 'tool' as const, payload: { id: n.id, tool: n.tool ?? 'unnamed_tool' } };
      case 'gate':
        return {
          kind: 'gate' as const,
          payload: {
            id: n.id,
            approval_schema: (n.inputSchema as import('@salvor-run/client').JsonValue) ?? { type: 'object' },
            ...(n.prompt !== undefined ? { prompt: n.prompt } : {}),
          },
        };
      case 'branch':
        return {
          kind: 'branch' as const,
          payload: {
            id: n.id,
            cases: (n.cases ?? []).map((name) => ({ name, when: { kind: 'model_decision' as const } })),
          },
        };
      case 'map':
        return {
          kind: 'map' as const,
          payload: {
            id: n.id,
            over: n.over ?? '${input}',
            concurrency: n.concurrency ?? 1,
            body: { kind: 'node' as const, value: n.body?.node ?? n.id },
          },
        };
      case 'fold':
        return {
          kind: 'fold' as const,
          payload: {
            id: n.id,
            body: { kind: 'node' as const, value: n.body?.node ?? n.id },
            max_iterations: n.maxIterations ?? 1,
            stop_when: n.stopWhen ?? 'false',
            join: { kind: 'last' as const },
          },
        };
    }
  });
  const edges = g.edges.map((e) => (e.label !== undefined ? { from: e.from, to: e.to, label: e.label } : { from: e.from, to: e.to }));
  return { schema_version: 1, nodes, edges };
}
