import {
  ChangeDetectionStrategy,
  Component,
  ElementRef,
  ViewChild,
  computed,
  inject,
  signal,
} from '@angular/core';

import {
  CapabilityProbeService,
  GraphRunService,
  GraphsService,
  RunsService,
  type GraphProjection,
  type ForkOutcome,
} from '../../core/api';
import { focusWhenRendered } from '../../core/focus';
import { agentIdentity, toRunRow } from '../runs/run-model';
import { loadDrafts, removeDraft, saveDraft } from './wf-draft';
import {
  type WfView,
  layeredLayout,
  wfFit,
  wfReset,
  wfZoom,
  zoomPercent,
} from './wf-geometry';
import { type HazardReview, type HazardRow, forkReady, reviewOf } from './wf-hazard';
import {
  type WfGraph,
  type WfNode,
  type WfPickOption,
  fromServerGraph,
  pickOptions,
} from './wf-model';
import {
  type WfNodeProjection,
  edgeWalked,
  projectNodeStates,
  projectionUsable,
} from './wf-projection';

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
  readonly run?: WfNodeProjection;
}

interface PositionedEdge {
  readonly from: string;
  readonly to: string;
  readonly label?: string;
  readonly d: string;
  readonly walked: boolean;
}

/**
 * THE WORKFLOWS CANVAS — the graph authoring + fork surface, ported from the prototype's
 * `salvor-bridge.html` canvas. Builder and Run modes; a picker over server graphs AND in-browser
 * drafts; pan/zoom with the {@link wfFit} legibility floor; a minimap; per-node ⋯ menus; and the
 * app's ONE modal, the fork hazard review, driven off the REAL fork API.
 *
 * HELD THIS SLICE. The suite un-holds Workflows only when a `.nav-link[data-view="workflows"]`
 * OR a `#wf-nodes` exists (the e2e suite's workflowsHeld probe). So this
 * component is rendered ONLY behind a dev-only `?wf=1` query flag (see `app.ts#wfDevFlag`), and the
 * nav link is not added — the structural detectors see the held state by default on both targets.
 * S8d removes the flag, adds the nav link, and flips `workflowsHeld` to false.
 */
@Component({
  selector: 'bridge-workflows',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './workflows.html',
})
export class Workflows {
  private readonly graphsService = inject(GraphsService);
  private readonly graphRuns = inject(GraphRunService);
  private readonly runsService = inject(RunsService);
  private readonly capabilityProbe = inject(CapabilityProbeService);

  @ViewChild('wfCanvas') private canvasRef?: ElementRef<HTMLElement>;
  @ViewChild('forkDlg') private forkDlgRef?: ElementRef<HTMLDialogElement>;

  // ── graph catalog ──
  private readonly serverGraphs = signal<WfGraph[]>([]);
  private readonly drafts = signal<WfGraph[]>([]);
  readonly currentKey = signal<string>('');
  /** Cached run_id → projection for every graph run (fetched once on entry, a handful of runs). */
  private readonly projections = signal<ReadonlyMap<string, GraphProjection>>(new Map());
  private readonly capability = this.capabilityProbe.capabilities;

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

  // ── fork dialog state ──
  readonly forkReview = signal<HazardReview | undefined>(undefined);
  readonly forkAck = signal<ReadonlySet<number>>(new Set());
  readonly forkTitle = signal<string>('Fork this run');
  readonly forkSub = signal<string>('');
  readonly forkNode = signal<string | undefined>(undefined);
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
  readonly zoomLabel = computed(() => zoomPercent(this.view().k));

  private readonly layout = computed(() => {
    const g = this.currentGraph();
    return g ? layeredLayout(g) : {};
  });

  /** The run's projection, when Run mode is on a run of the shown graph. */
  private readonly activeProjection = computed<GraphProjection | undefined>(() => {
    const id = this.runId();
    return id ? this.projections().get(id) : undefined;
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
    return g.nodes.map((n) => ({
      ...n,
      x: layout[n.id]?.x ?? 0,
      y: layout[n.id]?.y ?? 0,
      run: states[n.id],
    }));
  });

  readonly edges = computed<PositionedEdge[]>(() => {
    const g = this.currentGraph();
    if (!g) return [];
    const layout = this.layout();
    const states = this.nodeStates();
    const inRun = this.mode() === 'run' && Object.keys(states).length > 0;
    const branchSources = new Set(g.nodes.filter((n) => n.kind === 'branch').map((n) => n.id));
    return g.edges.map((e) => {
      const a = layout[e.from] ?? { x: 0, y: 0 };
      const b = layout[e.to] ?? { x: 0, y: 0 };
      const x1 = a.x + 208;
      const y1 = a.y + 52;
      const x2 = b.x;
      const y2 = b.y + 52;
      const mid = (x1 + x2) / 2;
      const d = `M ${x1} ${y1} C ${mid} ${y1}, ${mid} ${y2}, ${x2} ${y2}`;
      const walked = inRun && edgeWalked(e.from, e.to, e.label, branchSources.has(e.from), states);
      return e.label !== undefined ? { from: e.from, to: e.to, label: e.label, d, walked } : { from: e.from, to: e.to, d, walked };
    });
  });

  /** The graph runs the run picker offers (agentIdentity 'graph' rows), newest first. */
  readonly graphRunOptions = computed(() =>
    this.runsService
      .runs()
      .map(toRunRow)
      .filter((r) => agentIdentity(r).kind === 'graph')
      .map((r) => ({ id: r.id, label: r.id.slice(0, 8), status: r.status })),
  );

  /** The runs of the CURRENTLY shown graph — a graph run whose projection names this graph. */
  private readonly runsOfGraph = computed<string[]>(() => {
    const g = this.currentGraph();
    if (!g?.hash) return [];
    const out: string[] = [];
    for (const [id, proj] of this.projections()) if (proj.graphHash === g.hash) out.push(id);
    return out;
  });

  readonly stageTransform = computed(() => {
    const v = this.view();
    return `translate(${v.x}px, ${v.y}px) scale(${v.k})`;
  });

  readonly canvasBox = computed<{ width: number; height: number }>(() => ({
    width: 3000,
    height: 2000,
  }));

  readonly minimapHidden = computed(() => {
    // The minimap is furniture only when the graph overruns the viewport. A rough test: the scaled
    // content is wider or taller than a nominal viewport at the current zoom. Recomputed on zoom.
    const g = this.currentGraph();
    if (!g) return true;
    const layout = this.layout();
    const xs = g.nodes.map((n) => layout[n.id]?.x ?? 0);
    const ys = g.nodes.map((n) => layout[n.id]?.y ?? 0);
    const w = (Math.max(...xs) + 208 - Math.min(...xs)) * this.view().k;
    const h = (Math.max(...ys) + 104 - Math.min(...ys)) * this.view().k;
    const box = this.canvasEl()?.getBoundingClientRect();
    if (!box) return true;
    return w <= box.width && h <= box.height;
  });

  constructor() {
    void this.load();
  }

  private async load(): Promise<void> {
    this.drafts.set(loadDrafts());
    // Probe fork capability for the canvas's own fork entry (the Inspector offer stays pinned off).
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
    // Graph-run projections (a handful), so Run mode and the node menu know which runs are real.
    try {
      const runs = await this.runsService.refresh();
      const graphRuns = runs
        .map(toRunRow)
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
    queueMicrotask(() => this.fit());
  }

  private canvasEl(): HTMLElement | undefined {
    return this.canvasRef?.nativeElement;
  }

  // ── picker ──
  pick(value: string): void {
    this.currentKey.set(value);
    this.selectedNode.set(undefined);
    this.menu.set(undefined);
    // A run only projects onto the graph it ran; leaving Run mode on a graph switch keeps the
    // canvas honest rather than inking a mismatched run.
    if (this.mode() === 'run' && this.runsOfGraph().length === 0) this.setMode('build');
    queueMicrotask(() => this.fit());
  }

  onPick(event: Event): void {
    this.pick((event.target as HTMLSelectElement).value);
  }

  // ── mode + tool ──
  setMode(mode: WfMode): void {
    this.mode.set(mode);
    this.menu.set(undefined);
    if (mode === 'run' && !this.runId()) {
      const first = this.runsOfGraph()[0] ?? this.graphRunOptions()[0]?.id;
      if (first) this.runId.set(first);
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

  // ── pan (PAN_BLOCK: a pointerdown on a control never starts a canvas pan) ──
  private panStart?: { px: number; py: number; vx: number; vy: number };
  // A pan starts ONLY on the empty canvas paper. Every node and every floating control is blocked
  // here so a pointerdown on it selects/acts (and is never captured by the canvas) — the exact
  // defect 02-canvas-chrome-gate.spec.js exists to catch. A pan drags the background, nothing else.
  private static readonly PAN_BLOCK = '.wf-node, .wf-chips, .wf-bar, .wf-map, .wf-menu, .wf-note, [data-more], button, select, input, label';

  onCanvasPointerdown(event: PointerEvent): void {
    const target = event.target as HTMLElement;
    if (target.closest(Workflows.PAN_BLOCK)) return; // a control: never pan
    if (this.tool() !== 'pan') return;
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

  // ── minimap: click to jump the view to that graph point ──
  onMinimapClick(event: MouseEvent): void {
    const g = this.currentGraph();
    const svg = event.currentTarget as SVGElement;
    const box = this.canvasEl()?.getBoundingClientRect();
    if (!g || !box) return;
    const r = svg.getBoundingClientRect();
    const layout = this.layout();
    const xs = g.nodes.map((n) => layout[n.id]?.x ?? 0);
    const ys = g.nodes.map((n) => layout[n.id]?.y ?? 0);
    const gw = Math.max(...xs) + 208 - Math.min(...xs);
    const gh = Math.max(...ys) + 104 - Math.min(...ys);
    const fx = (event.clientX - r.left) / r.width;
    const fy = (event.clientY - r.top) / r.height;
    const gx = Math.min(...xs) + fx * gw;
    const gy = Math.min(...ys) + fy * gh;
    this.view.update((v) => ({ ...v, x: box.width / 2 - gx * v.k, y: box.height / 2 - gy * v.k }));
  }

  // ── node selection + ⋯ menu ──
  selectNode(id: string): void {
    this.selectedNode.set(this.selectedNode() === id ? undefined : id);
  }
  nodePressed(id: string): boolean {
    return this.selectedNode() === id;
  }
  nodeStateClass(n: PositionedNode): string {
    return n.run ? `is-${n.run.state}` : '';
  }

  openMenu(event: MouseEvent, id: string): void {
    event.stopPropagation();
    const canvas = this.canvasEl()?.getBoundingClientRect();
    const btn = (event.currentTarget as HTMLElement).getBoundingClientRect();
    if (!canvas) return;
    this.menu.set({ nodeId: id, x: btn.left - canvas.left - 150, y: btn.top - canvas.top + 24 });
    // Focus the first ENABLED menu item once it renders (zoneless: next frame, not this microtask).
    // Focusing the disabled Fork entry would trap the keyboard on a dead control — 03-fork asserts it.
    focusWhenRendered('.wf-menu button:not([disabled])');
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
    return this.mode() === 'run' && projectionUsable(this.currentGraph()!, this.activeProjection());
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
    if (id) void this.openFork(this.runId(), id);
  }
  menuForkSwitch(): void {
    const id = this.menu()?.nodeId;
    const runs = this.runsOfGraph();
    if (!runs.includes(this.runId() ?? '')) this.runId.set(runs[0]);
    this.setMode('run');
    if (id) this.selectedNode.set(id);
    this.closeMenu();
  }
  menuCopy(): void {
    const id = this.menu()?.nodeId;
    if (id && navigator.clipboard) void navigator.clipboard.writeText(id);
    this.closeMenu();
  }

  // ── the fork dialog: dry-run preview, acknowledge, resubmit ──
  async openFork(runId: string | undefined, nodeId: string): Promise<void> {
    if (!runId) return;
    this.forkNode.set(nodeId);
    this.forkAck.set(new Set());
    this.forkRefusal.set(undefined);
    this.forked.set('');
    const node = this.currentGraph()?.nodes.find((n) => n.id === nodeId);
    this.forkTitle.set(`Fork at ${node?.name ?? nodeId}`);
    this.forkSub.set(`origin ${runId.slice(0, 8)} · node ${nodeId} · graph ${(this.currentGraph()?.hash ?? '').slice(0, 15)}`);
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
    const runId = this.runId();
    const nodeId = this.forkNode();
    if (!runId || !nodeId) return;
    const outcome: ForkOutcome = await this.graphRuns.fork(runId, {
      fromNode: nodeId,
      acknowledgeWrites: [...this.forkAck()],
    });
    if (outcome.kind === 'forked') {
      this.closeDialog();
      this.forked.set(`forked ${outcome.run.slice(0, 8)} from ${runId.slice(0, 8)} at ${nodeId}`);
      // The new run is real; fold it into the projection cache and the picker.
      try {
        this.projections.update((m) => new Map(m).set(outcome.run, this.projections().get(runId)!));
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

  // ── save (local draft) / publish (POST /v1/graphs) ──
  save(): void {
    const g = this.currentGraph();
    if (!g || g.state !== 'draft') return;
    this.drafts.set(saveDraft(g));
    this.published.set(`draft saved · ${g.name}`);
  }

  async publish(): Promise<void> {
    const g = this.currentGraph();
    if (!g || g.state !== 'draft') return;
    try {
      const hash = await this.graphsService.submit(toServerDocument(g));
      this.drafts.set(removeDraft(g.key));
      await this.load();
      this.currentKey.set(hash);
      this.published.set(`published ${hash.slice(0, 15)}`);
    } catch (err) {
      this.published.set(`publish refused — ${err instanceof Error ? err.message : String(err)}`);
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
  inputJson(input: unknown): string {
    try {
      return JSON.stringify(input, null, 2);
    } catch {
      return String(input);
    }
  }
  capabilityFork(): boolean {
    return this.capability().fork;
  }
}

/** Convert a canvas draft back to a server document for publish. A draft node carries id/kind/name;
 * the server document is `{ kind, payload }` — the minimum each kind requires. */
function toServerDocument(g: WfGraph): import('@salvor/client').Graph {
  const nodes = g.nodes.map((n) => {
    switch (n.kind) {
      case 'agent':
        return { kind: 'agent' as const, payload: { id: n.id, agent_hash: 'sha256:0000000000000000000000000000000000000000000000000000000000000000' } };
      case 'tool':
        return { kind: 'tool' as const, payload: { id: n.id, tool: 'unnamed_tool' } };
      case 'gate':
        return { kind: 'gate' as const, payload: { id: n.id, approval_schema: { type: 'object' } } };
      case 'branch':
        return { kind: 'branch' as const, payload: { id: n.id, cases: [] } };
      case 'map':
        return { kind: 'map' as const, payload: { id: n.id, over: '${input}', concurrency: 1, body: { kind: 'node' as const, value: n.id } } };
    }
  });
  const edges = g.edges.map((e) => (e.label !== undefined ? { from: e.from, to: e.to, label: e.label } : { from: e.from, to: e.to }));
  return { schema_version: 1, nodes, edges };
}
