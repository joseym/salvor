import { Component, computed, effect, inject, signal } from '@angular/core';
import type { RunState, RunSummary } from '@salvor/client';

import { RunsService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { focusWhenRendered } from '../../core/focus';
import { labelOf } from '../runs/run-model';
import { BudgetCard } from './budget-card';
import { shortId } from './inbox-model';
import { jsonHi } from '../../shared/json-hi';
import { ReconcileCard } from './reconcile-card';
import { SuspensionCard } from './suspension-card';

/**
 * The Inbox: every run at `suspended`, `budget_exceeded`, or `needs_reconciliation`, with the one
 * action that unblocks it. Reads the same {@link RunsService} projection the Runs ledger and the
 * shell's badge read, so the count can never disagree across views.
 *
 * The real control plane's wire statuses are snake_case (`budget_exceeded`, `needs_reconciliation`,
 * same shape as `run-model.ts`'s own) — the lede above states the real ones, not hyphenated slugs.
 *
 * Each card kind (suspension / budget / reconcile) is its own small component so its local form
 * state — a per-run signal, not a `Map` keyed by run id in this parent — lives naturally in its own
 * component instance, one per `@for` iteration.
 *
 * CARD PERMANENCE: `parked` does NOT simply re-filter `runsService.runs()` live, and the card kind
 * it renders is not re-derived from the row's live status either. A committed card must be
 * deliberately REPLACED IN PLACE by its receipt, never re-rendered away — a naive live re-filter
 * breaks that: the moment a commit succeeds, `onCommitted()` must refresh `RunsService` (the Runs
 * ledger, health strip, and the shell's badge all need the fresh status), and that refresh's
 * response no longer carries this run as `waiting` — so a plain `.filter(isWaiting)` recomputes the
 * very instant the commit lands.
 * Worse, even keeping the ROW around is not enough on its own: a `@switch` keyed on the row's live
 * `status.state` stops matching any card kind the moment that status changes (to `running`, then
 * `completed`, as the freshly-resumed run keeps driving), so the card component itself still gets
 * torn down by Angular. `shownCards` fixes both: once a run is first seen waiting, its run id AND
 * the card kind it was waiting as are remembered together, and the remembered kind — never the
 * row's current status — decides which component renders. The row data itself stays live (so the
 * card's `[row]` input reflects a fresh `eventCount` etc.), but which template branch renders is
 * pinned. That is exactly how a card can carry its own local `receipt` signal through a parent-level
 * data refresh without losing it, live-tested against a real resume/resolve round trip that kept
 * driving the run to completion within the same page view.
 */
export type CardKind = 'suspension' | 'budget' | 'reconcile';

function cardKindOf(state: string): CardKind | undefined {
  switch (state) {
    case 'suspended':
      return 'suspension';
    case 'budget_exceeded':
      return 'budget';
    case 'needs_reconciliation':
      return 'reconcile';
    default:
      return undefined;
  }
}

export interface InboxCardVM {
  readonly row: RunSummary;
  readonly kind: CardKind;
}

@Component({
  selector: 'bridge-inbox',
  imports: [SuspensionCard, BudgetCard, ReconcileCard],
  templateUrl: './inbox.html',
})
export class Inbox {
  private readonly runsService = inject(RunsService);

  readonly loading = this.runsService.loading;
  /** STICKY on purpose — `lastLoadedAt` alone, never `!loading()`. A post-commit refresh flips
   * `loading` true for a moment, and gating the cards region on it swaps the whole `@if` branch to
   * the loading note, DESTROYING every card component mid-flight — including the receipt a person
   * just watched get written: the announce fired, then the card re-mounted blank.
   * Once the first load has landed, the region never un-renders. */
  readonly listLoaded = computed(() => this.runsService.lastLoadedAt() !== undefined);

  /** Every run id ever seen waiting, with the card kind it was waiting AS — grown, never shrunk
   * and never re-derived, by the effect below. This is the "committed cards stay in place" memory;
   * see the class doc comment. */
  private readonly shownCards = signal<ReadonlyMap<string, CardKind>>(new Map());

  readonly parked = computed<InboxCardVM[]>(() => {
    const byId = new Map(this.runsService.runs().map((r) => [r.run, r] as const));
    const cards: InboxCardVM[] = [];
    for (const [id, kind] of this.shownCards()) {
      const row = byId.get(id);
      if (row) cards.push({ row, kind });
    }
    return cards;
  });

  /** Announced to `role="status" aria-live="polite"` — every commit, and nothing else, so the
   * region does not chatter on every unrelated re-render. */
  readonly liveMessage = signal('');

  constructor() {
    void this.load();
    effect(() => {
      const rows = this.runsService.runs();
      const additions = new Map<string, CardKind>();
      for (const r of rows) {
        const kind = cardKindOf(r.status.state);
        if (kind && !this.shownCards().has(r.run)) additions.set(r.run, kind);
      }
      if (additions.size === 0) return;
      this.shownCards.update((prev) => new Map([...prev, ...additions]));
    });
  }

  private async load(): Promise<void> {
    try {
      await this.runsService.refresh();
    } catch {
      /* error surfaced via runsService.error, same swallow-and-surface pattern as Runs' own load() */
    }
  }

  onAnnounce(message: string): void {
    this.liveMessage.set(message);
  }

  /** A commit re-fetches the list, so the Runs ledger, the health strip, and the shell's Inbox
   * badge all reflect the append — nothing here mutates a count by hand. */
  onCommitted(): void {
    void this.load();
  }

  // ── the Evidence panel: READ, never act ─────────────────────────────────────────────────────
  private readonly client = inject(SALVOR_CLIENT);

  /** Cold-load COLLAPSED: the cards are the decision surface and the evidence panel duplicates
   * the selected card, so it earns its width only on request. */
  readonly panelOpen = signal(false);
  readonly evidenceSel = signal<string | undefined>(undefined);
  readonly evidenceState = signal<RunState | undefined>(undefined);
  readonly evidenceError = signal<string | undefined>(undefined);

  readonly panelTitle = computed(() => {
    const id = this.evidenceSel();
    return id ? shortId(id) : 'Evidence';
  });
  readonly panelSub = computed(() => {
    const state = this.evidenceState();
    if (!this.evidenceSel()) return 'Nothing selected';
    return state ? labelOf(state.status.state) : '';
  });

  /** Toggle a card's Evidence selection. READ, never act — the panel renders a fresh
   * `GET /v1/runs/{id}` and deliberately does NOT re-render the cards, so a half-filled form
   * survives a change in what you are reading. */
  onEvidence(runId: string): void {
    if (this.evidenceSel() === runId) {
      this.evidenceSel.set(undefined);
      this.evidenceState.set(undefined);
      this.panelOpen.set(true);
      return;
    }
    this.evidenceSel.set(runId);
    this.evidenceState.set(undefined);
    this.evidenceError.set(undefined);
    this.panelOpen.set(true);
    void this.loadEvidence(runId);
  }

  private async loadEvidence(runId: string): Promise<void> {
    try {
      const state = await this.client.getRun(runId);
      if (this.evidenceSel() === runId) this.evidenceState.set(state);
    } catch (ex) {
      if (this.evidenceSel() === runId) this.evidenceError.set(errorMessage(ex));
    }
  }

  // The dock hand-off: opening focuses the panel's dismiss control and closing focuses the
  // restore tab, so keyboard focus is never stranded on a control that just disappeared.
  closePanel(): void {
    this.panelOpen.set(false);
    focusWhenRendered('.cpanel-tab[data-panel="inbox"]');
  }
  openPanel(): void {
    this.panelOpen.set(true);
    focusWhenRendered('.cpanel[data-panel="inbox"] .cpanel-x');
  }

  // panel field helpers — every value read straight off the fresh RunState's own raw JSON
  evidenceStatusLabel(state: RunState): string {
    return labelOf(state.status.state);
  }
  evidencePendingHtml(state: RunState): string {
    return jsonHi(state.raw['pending'], 1);
  }
  evidenceSchemaHtml(state: RunState): string {
    return jsonHi(state.status.inputSchema, 1);
  }
  evidenceBudgetHtml(state: RunState): string {
    return jsonHi(
      { budget: state.status.raw['budget'], observed: state.status.raw['observed'] },
      1,
    );
  }
  evidenceReason(state: RunState): string {
    return state.status.reason ?? '';
  }
  evidenceEndpoint(state: RunState): string {
    const ns = shortId(state.run);
    return state.status.state === 'needs_reconciliation'
      ? `POST /v1/runs/${ns}…/resolve`
      : `POST /v1/runs/${ns}…/resume`;
  }
}
