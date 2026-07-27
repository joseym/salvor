import { Component, computed, effect, inject, signal } from '@angular/core';
import type { RunState, RunSummary } from '@salvor-run/client';

import { RunsService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { FirstReceiptsService } from '../../core/first-receipts';
import { focusWhenRendered } from '../../core/focus';
import { ViewService } from '../../core/view';
import { derivedStatus, labelOf } from '../runs/run-model';
import { BudgetCard } from './budget-card';
import { shortId } from './inbox-model';
import { jsonHi } from '../../shared/json-hi';
import { ReconcileCard } from './reconcile-card';
import { StalledCard } from './stalled-card';
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
  imports: [SuspensionCard, BudgetCard, ReconcileCard, StalledCard],
  templateUrl: './inbox.html',
})
export class Inbox {
  private readonly runsService = inject(RunsService);
  private readonly viewService = inject(ViewService);
  private readonly firstReceipts = inject(FirstReceiptsService);

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

  /**
   * The STALLED cards, LIVE from each list load, PLUS any run id currently pinned by
   * {@link abandonPinned} — a stall used to have no commit and so nothing to protect (see
   * {@link StalledCard}'s history); it now does, once its abandon-action's receipt lands. Without
   * the pinned union, the moment abandoning it re-derives the list, {@link derivedStatus} stops
   * returning `stalled` for the now-terminal run and the card carrying the still-unread receipt
   * would vanish out from under the operator. A driver reattaching (the ORIGINAL reason this stayed
   * live rather than sticky) still drops the card the instant it happens, exactly as before — that
   * path never pins anything.
   */
  readonly stalled = computed<RunSummary[]>(() => {
    const now = Date.now();
    const rows = this.runsService.runs();
    const byId = new Map(rows.map((r) => [r.run, r] as const));
    const live = rows.filter((r) => derivedStatus(r.status.state, r.driver, r.lastRecordedAt, now) === 'stalled');
    const seen = new Set(live.map((r) => r.run));
    for (const id of this.abandonPinned()) {
      if (seen.has(id)) continue;
      const row = byId.get(id);
      if (row) live.push(row);
    }
    return live;
  });

  /** Whether the Inbox has anything to show — a commit card or a stalled card. Drives the empty
   * "All clear" state, which must not claim calm while a run sits driverless. */
  readonly hasCards = computed(() => this.parked().length > 0 || this.stalled().length > 0);

  /** Announced to `role="status" aria-live="polite"` — every commit, and nothing else, so the
   * region does not chatter on every unrelated re-render. */
  readonly liveMessage = signal('');

  /**
   * THE HUSK'S EXIT (owner ask, 2026-07-21). Two signals drive an abandon receipt's own lifecycle,
   * on top of (and orthogonal to) `shownCards`'s permanence rule:
   *
   *   - `abandonPinned`: run ids whose abandon receipt has landed but not yet been dismissed. Only
   *     a STALLED run needs this to survive at all (see `stalled`'s doc comment above) — a parked
   *     card is already permanent via `shownCards` — but the set is shared by both families so one
   *     mechanism drives the fold-away wrapper class for every card kind.
   *   - `abandonLeaving`: run ids currently PLAYING the fold-away exit (the CSS class is applied in
   *     inbox.html); removed from view only once the transition genuinely finishes
   *     (`onAbandonExitDone`, fired by the wrapper's own `transitionend` — never a guessed timer,
   *     the same discipline the reveal side of this mechanism already follows).
   */
  private readonly abandonPinned = signal<ReadonlySet<string>>(new Set());
  private readonly abandonLeaving = signal<ReadonlySet<string>>(new Set());

  /** True while `runId`'s card is playing its fold-away exit — drives `.is-leaving` on its wrapper. */
  isAbandonLeaving(runId: string): boolean {
    return this.abandonLeaving().has(runId);
  }

  /** The abandon-action's `receipted`: pin this run id so a STALLED card's live re-derivation (about
   * to be triggered by the `committed` this always fires alongside) cannot yank it out from under
   * the receipt. Harmless no-op significance for a parked card — already permanent — but pinning it
   * too keeps one mechanism for both families. */
  onAbandonReceipted(runId: string): void {
    this.abandonPinned.update((s) => new Set(s).add(runId));
  }

  /** The abandon receipt's own "Done": start the fold-away. Actual removal waits for
   * `onAbandonExitDone`, so the exit is seen, never guessed at with a timer. */
  onAbandonRetire(runId: string): void {
    this.abandonLeaving.update((s) => new Set(s).add(runId));
  }

  /** The wrapper's `transitionend` has fired — the fold-away has genuinely finished. Drop the run id
   * from every set that was keeping its card around: `shownCards` (for a parked card; a no-op if it
   * was never there — a stalled run alone), and both of the sets above. `stalled`/`parked` then
   * exclude it on their own on the next read — no separate "remove the card" call needed.
   *
   * STAGED EXIT (inbox.css's `.acard-exit`): the wrapper fires more than one `transitionend` now —
   * a quick, undelayed content fade first, then the actual `grid-template-rows` height collapse,
   * delayed to start once the fade finishes. Only the height one is the card's REAL exit; reacting
   * to whichever fires first would drop the DOM out from under a collapse still mid-flight, so this
   * is keyed on `event.propertyName` rather than on "a transitionend happened". */
  onAbandonExitDone(runId: string, event: Event): void {
    if ((event as TransitionEvent).propertyName !== 'grid-template-rows') return; // the fade stage's own transitionend — not the real exit
    if (!this.abandonLeaving().has(runId)) return; // stray/duplicate transitionend — already handled
    this.abandonLeaving.update((s) => {
      const next = new Set(s);
      next.delete(runId);
      return next;
    });
    this.abandonPinned.update((s) => {
      const next = new Set(s);
      next.delete(runId);
      return next;
    });
    this.shownCards.update((m) => {
      if (!m.has(runId)) return m;
      const next = new Map(m);
      next.delete(runId);
      return next;
    });
  }

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

    // A signpost from the Runs side panel: land focus on the named run's action card. Each card
    // carries the DOM id `card-<first8>` (see the card templates); focusWhenRendered waits out the
    // zoneless render + view switch, then focuses it.
    effect(() => {
      const focus = this.viewService.inboxFocus();
      if (!focus) return;
      focusWhenRendered('#card-' + shortId(focus.runId));
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
    // First Receipts step 3 reads the REAL committed action — the card's own announce (set on
    // onAnnounce, which every card emits just before `committed`) carries the verb and the run.
    this.firstReceipts.noteInboxAction(this.liveMessage());
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
