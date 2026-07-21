import {
  ChangeDetectorRef,
  Component,
  OnInit,
  computed,
  inject,
  input,
  output,
  signal,
} from '@angular/core';
import type { RunSummary } from '@salvor/client';

import { RunDetailService, SALVOR_CLIENT, errorMessage } from '../../core/api';
import { ViewService } from '../../core/view';
import { AbandonAction } from './abandon-action';
import {
  type ReceiptVM,
  type ReconcileIntent,
  buildReceipt,
  fetchAppendedEvent,
  reconcileIntentFrom,
  shortId,
} from './inbox-model';
import { jsonHi } from '../../shared/json-hi';
import { RunRef } from './run-ref';

/** The reconcile card's own branch state. A class that sets its own `display` beats the UA's
 * `[hidden]` rule, so visibility is driven from THIS state machine, and only this, never inferred
 * from which controls happen to be checked at read time. Undefined is "no choice made yet" — both
 * branches unrendered. */
export type ReconcileOutcome = 'reached' | 'not_reached' | undefined;

/**
 * ReconcileCard: `status.state === 'needs_reconciliation'`. The evidence (tool, effect,
 * idempotency_key, input, recorded_at) is built from two plain 200 reads, never an error response —
 * see {@link reconcileIntentFrom}'s doc comment for why: a browser logs any non-2xx `fetch` to the
 * console unconditionally, and this suite fails a test on any console output, so the first design
 * (provoke the `409 needs_reconciliation` refusal and read its evidence, exactly as `salvor resume`
 * does at the CLI) could not survive contact with a real run. `GET /v1/runs/{id}`'s `pending` gives
 * everything but `recorded_at`; the event stream opened at `pending.seq` gives that, off the
 * envelope itself.
 *
 * The two honest paths are EXCLUSIVE: `outcome` decides which single branch renders (both hidden
 * with no choice made), driven by real clicks in the suite trial, never by `page.evaluate` setting
 * hidden directly. Submit stays disabled until the "reached" branch is chosen AND the immutable-
 * history checkbox is confirmed — the "not_reached" path has no submit at all, by design (resolve
 * cannot record an absence).
 */
@Component({
  selector: 'bridge-reconcile-card',
  imports: [RunRef, AbandonAction],
  templateUrl: './reconcile-card.html',
})
export class ReconcileCard implements OnInit {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly runDetail = inject(RunDetailService);
  private readonly viewService = inject(ViewService);
  private readonly cdr = inject(ChangeDetectorRef);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  readonly announce = output<string>();
  readonly committed = output<void>();
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();
  /** Forwarded from the embedded (secondary) abandon-action's receipt "Done" — an abandon here
   * retires the run without resolving it, so the parent folds this whole card away, the same
   * treatment the Stalled card's abandon receipt gets (see stalled-card.ts's EXIT note). The card's
   * OWN resolve receipt is untouched — it keeps its existing, permanent `.committed` treatment. */
  readonly retire = output<void>();

  readonly ns = computed(() => shortId(this.row().run));
  readonly endpoint = computed(() => `POST /v1/runs/${this.ns()}…/resolve`);

  readonly intent = signal<ReconcileIntent | undefined>(undefined);
  readonly loadError = signal<string | undefined>(undefined);

  readonly outcome = signal<ReconcileOutcome>(undefined);
  readonly outputText = signal('');
  readonly confirmed = signal(false);
  readonly submitError = signal<string | undefined>(undefined);
  readonly submitting = signal(false);
  readonly receipt = signal<ReceiptVM | undefined>(undefined);

  readonly canSubmit = computed(
    () => this.outcome() === 'reached' && this.confirmed() && !this.submitting(),
  );

  ngOnInit(): void {
    void this.loadEvidence();
  }

  private async loadEvidence(): Promise<void> {
    try {
      const runId = this.row().run;
      const state = await this.client.getRun(runId);
      const pending = state.pending;
      if (!pending || pending.kind !== 'tool') {
        this.loadError.set(
          'This run no longer needs reconciliation — it may have been resolved elsewhere.',
        );
        return;
      }
      const event = await fetchAppendedEvent(this.client, runId, pending.seq);
      const built = reconcileIntentFrom(pending, event?.recordedAt);
      if (built) this.intent.set(built);
      else this.loadError.set('The recorded pending call carried no readable intent.');
    } catch (ex) {
      this.loadError.set(errorMessage(ex));
    }
  }

  /**
   * Choosing a branch must be reflected in the DOM before this method returns to the caller —
   * observed flaky (~1 in 5) without the explicit tick: this app is zoneless, so a plain
   * `signal.set()` inside a template event handler schedules Angular's own change-detection
   * notify, which can still land a frame after the click when nothing else forces a flush, and a
   * test reading `getComputedStyle` immediately after a real click (no auto-retry, by design — the
   * whole point is reading COMPUTED state, not polling for it) can sample the gap.
   * `ChangeDetectorRef.detectChanges()` forces the flush synchronously, scoped to this component's
   * own view (unlike `ApplicationRef.tick()`, which walks every attached view app-wide and is both
   * unnecessary here and unsafe in a test harness that keeps multiple fixtures alive at once) — so
   * `[hidden]` on both branches is already correct by the time this handler's caller (the click)
   * returns.
   */
  chooseOutcome(value: ReconcileOutcome): void {
    this.outcome.set(value);
    this.cdr.detectChanges();
  }

  setConfirmed(value: boolean): void {
    this.confirmed.set(value);
    this.cdr.detectChanges();
  }

  jsonHiCompact(value: unknown): string {
    return jsonHi(value);
  }

  /** The recorded write as a copyable invocation — the tool plus its exact recorded input, so the
   * operator who must perform the call by hand (the honest off-ramp of the "did not reach" branch)
   * can lift it verbatim rather than retyping the JSON. */
  invocation(i: ReconcileIntent): { tool: string; input: unknown } {
    return { tool: i.tool, input: i.input };
  }
  readonly callCopied = signal(false);
  async copyInvocation(i: ReconcileIntent): Promise<void> {
    try {
      await navigator.clipboard.writeText(JSON.stringify(this.invocation(i), null, 2));
      this.callCopied.set(true);
      setTimeout(() => this.callCopied.set(false), 1500);
    } catch {
      /* clipboard blocked — no fallback theater, matching the rest of the app's copy() */
    }
  }

  async submit(): Promise<void> {
    const i = this.intent();
    if (!i || !this.canSubmit()) return;
    let output: unknown;
    try {
      const text = this.outputText().trim();
      if (!text) throw new Error('Paste the output the provider returned, or record the call as not performed.');
      output = JSON.parse(text);
    } catch (ex) {
      const msg = errorMessage(ex);
      this.submitError.set(msg.startsWith('Paste') ? msg : `Not valid JSON — ${msg}`);
      return;
    }
    this.submitError.set(undefined);
    this.submitting.set(true);
    const runId = this.row().run;
    const beforeCount = this.row().eventCount;
    const payload = { tool: i.tool, output };
    try {
      await this.runDetail.resolve(runId, output);
      const r = await buildReceipt(
        this.client,
        runId,
        beforeCount,
        'ToolCallCompleted',
        payload,
        this.endpoint(),
      );
      this.receipt.set(r);
      this.announce.emit(
        `Recorded the outcome for run ${this.ns()}. Appended at sequence ${r.seq ?? '—'}. Status now ${r.statusState}.`,
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
}
