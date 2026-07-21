import { Component, computed, inject, input, output, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import type { AbandonResult, RunSummary } from '@salvor/client';

import { SALVOR_CLIENT, errorMessage } from '../../core/api';
import { focusWhenRendered } from '../../core/focus';
import { shortId } from './inbox-model';

/**
 * AbandonAction: the one shared "retire this run" affordance, embedded by every Inbox card.
 *
 * Abandonment is the "we do not care about this run anymore" path — for a husk that is dead forever,
 * or a run whose noise is no longer worth carrying. It is a serious, deliberate action, so the
 * trigger never fires the call directly: it opens a confirm affordance (an optional reason plus an
 * "I understand" checkbox) and only the confirmed submit calls
 * `POST /v1/runs/{id}/abandon`. The response is the receipt — the appended seq and the re-derived
 * status — rendered in place, never a status this component set by hand.
 *
 * VARIANT decides visual weight, never behaviour:
 *   - `primary` (the Stalled card): this is the card's real action, so the trigger is a filled
 *     serious button — a stall has no resume/resolve/raise of its own.
 *   - `secondary` (the reconcile / budget / suspension cards): abandonment is subordinate to that
 *     card's own primary unblock, so the trigger is a quiet, ghost-weight serious button that sits
 *     visually below the primary action.
 *
 * RECONCILE honesty: when `reconcile` is set (the needs-reconciliation card), the confirm copy
 * states plainly that the dangling write STAYS UNRESOLVED and is recorded as such — abandoning a
 * needs-reconciliation run is allowed, but it never claims the write question was answered. The
 * server records the outstanding intent as `unresolved_write` on the terminal event; when the
 * receipt carries it back, the honesty line names it.
 */
@Component({
  selector: 'bridge-abandon-action',
  imports: [FormsModule],
  templateUrl: './abandon-action.html',
})
export class AbandonAction {
  private readonly client = inject(SALVOR_CLIENT);

  readonly row = input.required<RunSummary>();
  /** Filled serious button (`primary`, the Stalled card's real action) vs a quiet subordinate one
   * (`secondary`, below another card's own primary unblock). Default subordinate. */
  readonly variant = input<'primary' | 'secondary'>('secondary');
  /** The needs-reconciliation card: drive the write-stays-unresolved confirm copy. */
  readonly reconcile = input(false);

  readonly announce = output<string>();
  readonly committed = output<void>();

  readonly ns = computed(() => shortId(this.row().run));
  readonly endpoint = computed(() => `POST /v1/runs/${this.ns()}…/abandon`);

  /** The confirm affordance is closed until the operator opens it — the trigger never abandons on a
   * single click. */
  readonly open = signal(false);
  readonly reason = signal('');
  readonly confirmed = signal(false);
  readonly submitting = signal(false);
  readonly error = signal<string | undefined>(undefined);
  readonly receipt = signal<AbandonResult | undefined>(undefined);

  readonly canSubmit = computed(() => this.confirmed() && !this.submitting());

  /** The unresolved-write honesty, when the receipt carried one back — the write the abandonment
   * recorded as still unsettled rather than claiming settled. */
  readonly unresolved = computed(() => this.receipt()?.status.unresolvedWrite);

  openConfirm(): void {
    this.open.set(true);
    // Same hand-off shape as the app's other disclosures (see inbox.ts's cpanel open/close pair):
    // opening moves focus INTO the revealed slot, onto its first real field — the reason input,
    // never the checkbox first, since a reason is the lower-commitment thing to consider.
    focusWhenRendered('#abandon-reason-' + this.ns());
  }

  cancel(): void {
    this.open.set(false);
    this.confirmed.set(false);
    this.error.set(undefined);
    // ...and closing returns focus to the control that opened it, so the operator is never
    // stranded after the slot they were in collapses back to zero height.
    focusWhenRendered('[data-abandon="' + this.row().run + '"]');
  }

  async submit(): Promise<void> {
    if (!this.canSubmit()) return;
    this.error.set(undefined);
    this.submitting.set(true);
    const runId = this.row().run;
    const reason = this.reason().trim();
    try {
      const result = await this.client.abandon(runId, reason || undefined);
      this.receipt.set(result);
      this.open.set(false);
      const seq = result.appendedSeq ?? '—';
      this.announce.emit(
        `Abandoned run ${this.ns()}. Appended RunAbandoned at sequence ${seq}. Status now ${result.status.state}.`,
      );
      this.committed.emit();
    } catch (ex) {
      this.error.set(errorMessage(ex));
    } finally {
      this.submitting.set(false);
    }
  }
}
