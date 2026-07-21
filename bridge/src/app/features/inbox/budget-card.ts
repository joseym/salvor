import { Component, computed, effect, inject, input, output, signal } from '@angular/core';
import type { RunSummary } from '@salvor/client';

import { RunDetailService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { ViewService } from '../../core/view';
import { AbandonAction } from './abandon-action';
import {
  type BudgetInfo,
  type ReceiptVM,
  budgetFloor,
  budgetKindLabel,
  budgetPropose,
  buildReceipt,
  extendKey,
  formatBudgetValue,
  parseBudgetInfo,
  shortId,
} from './inbox-model';
import { jsonHi } from '../../shared/json-hi';
import { RunRef } from './run-ref';

/**
 * BudgetCard: `status.state === 'budget_exceeded'`. The floor is the honest minimum — what the run
 * has already spent, rounded up — and the proposed figure is explicitly THIS DASHBOARD'S suggestion
 * (twice the declared ceiling), never dressed up as something the run itself asked for: a
 * `BudgetExceeded` event carries only the limit and the observed spend, no proposal.
 *
 * THE HUSK'S EXIT, secondary-receipt pin (bug found in 9fe407b, fixed here): the embedded (secondary)
 * abandon-action lives inside the `@else if (budget(); as b)` branch of this card's template — and
 * `budget()` is a `computed` re-deriving `parseBudgetInfo` off this run's OWN status on every
 * `row()` change. Abandoning fires `committed` alongside its receipt, which the parent answers with a
 * list refresh; that refresh's fresh status no longer parses as `budget_exceeded`, so `budget()` goes
 * undefined and the template falls through to the "no readable budget object" branch — tearing down
 * the abandon-action, and the still-unread receipt riding inside it, out from under the operator.
 * `abandonPinned`/`lastBudget` fix this the same way `inbox.ts`'s own pin protects a stalled card:
 * `lastBudget` latches the last successfully-parsed budget info (the `effect` below), and once the
 * abandon-action's `receipted` fires, `budget()` falls back to that latch instead of going undefined,
 * so the SAME template branch (and so the same abandon-action instance, with its receipt intact)
 * keeps rendering until the operator's "Done" (`retire`) starts the fold — mirrored, never
 * reimplemented, from the exact mechanism `inbox.ts` already uses for the stalled card's own receipt.
 */
@Component({
  selector: 'bridge-budget-card',
  imports: [RunRef, AbandonAction],
  templateUrl: './budget-card.html',
})
export class BudgetCard {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly runDetail = inject(RunDetailService);
  private readonly viewService = inject(ViewService);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  readonly announce = output<string>();
  readonly committed = output<void>();
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();
  /** Forwarded from the embedded (secondary) abandon-action's receipt "Done" — an abandon here
   * retires the run, so the parent folds this whole card away, the same treatment the Stalled
   * card's abandon receipt gets (see stalled-card.ts's EXIT note). The card's OWN raise-and-resume
   * receipt is untouched — it keeps its existing, permanent `.committed` treatment. */
  readonly retire = output<void>();

  readonly ns = computed(() => shortId(this.row().run));

  /** Latches the last successfully-parsed budget info off `row()` — the "still readable" fallback
   * `budget()` reaches for once `abandonPinned` is set (see the class doc's EXIT note). Never reset:
   * once this card has shown a real budget object, it always has one to fall back on. */
  private readonly lastBudget = signal<BudgetInfo | undefined>(undefined);
  /** Set the instant the embedded (secondary) abandon-action's receipt lands — the parent's own
   * `onAbandonReceipted` equivalent, scoped to this card, so its own `committed`-triggered refresh
   * cannot yank the abandon-action's receipt out from under the operator (see the class doc). */
  private readonly abandonPinned = signal(false);

  constructor() {
    effect(() => {
      const parsed = parseBudgetInfo(this.row().status.raw);
      if (parsed) this.lastBudget.set(parsed);
    });
  }

  /** Real budget info while the run still carries one; once pinned (an abandon receipt has landed),
   * falls back to the last real one rather than going undefined and tearing the card's template
   * branch — and the abandon-action's own receipt riding inside it — down. */
  readonly budget = computed<BudgetInfo | undefined>(() => {
    const parsed = parseBudgetInfo(this.row().status.raw);
    return parsed ?? (this.abandonPinned() ? this.lastBudget() : undefined);
  });
  readonly floor = computed(() => {
    const b = this.budget();
    return b ? budgetFloor(b) : 0;
  });
  readonly propose = computed(() => {
    const b = this.budget();
    return b ? budgetPropose(b) : 0;
  });
  readonly kindLabel = computed(() => (this.budget() ? budgetKindLabel(this.budget()!.kind) : ''));
  readonly extendKeyName = computed(() => (this.budget() ? extendKey(this.budget()!.kind) : ''));
  readonly endpoint = computed(() => `POST /v1/runs/${this.ns()}…/resume`);

  private readonly edited = signal<string | undefined>(undefined);
  readonly newLimit = computed(() => this.edited() ?? String(this.propose()));
  readonly submitError = signal<string | undefined>(undefined);
  readonly submitting = signal(false);
  readonly receipt = signal<ReceiptVM | undefined>(undefined);

  setLimit(value: string): void {
    this.edited.set(value);
  }

  formatted(n: number): string {
    const b = this.budget();
    return b ? formatBudgetValue(b.kind, n) : String(n);
  }

  jsonHiCompact(value: unknown): string {
    return jsonHi(value);
  }

  async submit(): Promise<void> {
    const b = this.budget();
    if (!b || this.submitting()) return;
    const v = Number(this.newLimit());
    if (!(Number.isFinite(v) && v > b.observed)) {
      this.submitError.set(
        `The new ceiling must exceed the ${this.formatted(b.observed)} already spent, or the run ` +
          `would cross it again immediately.`,
      );
      return;
    }
    this.submitError.set(undefined);
    this.submitting.set(true);
    const runId = this.row().run;
    const beforeCount = this.row().eventCount;
    const input = { extend: { [extendKey(b.kind)]: v } };
    try {
      await this.runDetail.resume(runId, input);
      const r = await buildReceipt(this.client, runId, beforeCount, 'Resumed', { input }, this.endpoint());
      this.receipt.set(r);
      this.announce.emit(
        `Raised the limit and resumed run ${this.ns()}. Appended at sequence ${r.seq ?? '—'}. Status now ${r.statusState}.`,
      );
      this.committed.emit();
    } catch (ex) {
      this.submitError.set(errorMessage(ex));
    } finally {
      this.submitting.set(false);
    }
  }

  openTimeline(): void {
    this.viewService.openRun(this.row().run);
  }

  /** The embedded (secondary) abandon-action's `receipted` — the instant its receipt lands, before
   * the `committed` it always fires alongside triggers the parent's list refresh. Pins `budget()`
   * against that refresh's reclassification (see the class doc's EXIT note). */
  onAbandonReceipted(): void {
    this.abandonPinned.set(true);
  }
}
