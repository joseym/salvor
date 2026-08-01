import { Component, computed, inject, input, output, signal } from '@angular/core';
import type { RunSummary } from '@salvor-run/client';

import { ViewService } from '../../core/view';
import { age, labelOf } from '../runs/run-model';
import { AbandonAction } from './abandon-action';
import { shortId } from './inbox-model';
import { RunRef } from './run-ref';

/**
 * StalledCard: a run that folds to some state in the IN-PROGRESS family (`running`,
 * `awaiting_model`, `awaiting_tool`, `not_started`) yet the server reports `driver: "none"` for,
 * and whose last event has gone stale: a DERIVED state (see `run-model.ts#derivedStatus`), never
 * a server status. It is going nowhere: resolved but never re-driven, its driver crashed, or its
 * client abandoned it. The headline and evidence below name the run's ACTUAL folded state (e.g.
 * "awaiting model" for a run that died mid model-call), never a hardcoded "running": the
 * in-progress family is wider than that one state.
 *
 * Unlike every other Inbox card, a stalled card carries NO in-app action. The other cards each own
 * the one call that unblocks their run (resume, resolve, raise-the-limit); a stall is unblocked by
 * restarting the HOST PROCESS that drives the run, which lives outside this browser entirely. So
 * this card is honest about that: it states the evidence, gives the guidance ("restart the driver,
 * or resolve/abandon"), and links to the run; it never renders a fake fix button the dashboard
 * could not honor.
 *
 * It is also LIVE, not sticky: the parent derives the stalled set fresh from each list load, so if a
 * driver reattaches and the run resumes moving, its card simply stops being derived and disappears:
 * the opposite of the commit cards, which persist in place to carry their receipt.
 *
 * ONE in-app action, added deliberately: ABANDON. Everything above about the driver being an
 * external host process still holds; this card still renders no fake resume/restart button, because
 * there is no call this dashboard can make to un-stall a run. But abandonment is different: it is not
 * a fix, it is a RETIREMENT: "we do not care about this run anymore", and it IS a call the
 * dashboard can honestly make (`POST /v1/runs/{id}/abandon`, an operator action over the store,
 * needing no driver). So the stalled card carries the abandon affordance as its PRIMARY action,
 * alongside the same honest guidance about the external driver.
 *
 * THE HUSK'S EXIT (owner ask, 2026-07-21): a stall now DOES leave a receipt, and "live, not sticky"
 * used to mean it vanished the instant abandoning it re-derived the list: the receipt would land,
 * then get yanked out from under the operator the moment the post-commit refresh reclassified the
 * run as terminal (no longer `stalled`). Two things fix that:
 *   - `abandoned` (below) latches the moment the embedded abandon-action's receipt lands
 *     (`receipted`) and swaps this card's own body to a compact, receipt-only view: the stale
 *     "No driver attached"/evidence/reason copy would otherwise keep asserting a resting state the
 *     run no longer has.
 *   - the parent (inbox.ts) PINS this run id the same moment, so the commit-triggered refresh's
 *     re-derivation cannot drop the card out from under the still-showing receipt; it only unpins
 *     (and the card only actually leaves the Inbox) once the operator dismisses the receipt
 *     (`retire`, forwarded from the abandon-action's "Done") and the fold-away exit finishes.
 */
@Component({
  selector: 'bridge-stalled-card',
  imports: [RunRef, AbandonAction],
  templateUrl: './stalled-card.html',
})
export class StalledCard {
  private readonly viewService = inject(ViewService);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();
  /** Forwarded from the embedded abandon action, so the parent announces the retire and refreshes
   * the list (dropping this now-terminal run from the stalled set once it is dismissed). */
  readonly announce = output<string>();
  readonly committed = output<void>();
  /** Forwarded the instant the abandon receipt lands: the parent's cue to pin this run id against
   * its own commit-triggered refresh (see the class doc's EXIT note). */
  readonly receipted = output<void>();
  /** Forwarded from the abandon receipt's "Done": the parent folds this whole card away and, once
   * the exit finishes, unpins it. */
  readonly retire = output<void>();

  readonly ns = computed(() => shortId(this.row().run));
  /** "10m", "2h": the relative age of the last recorded event, the "gone quiet" evidence. */
  readonly lastAge = computed(() => age(this.row().lastRecordedAt));
  /** The run's ACTUAL folded resting state, human-labelled ("awaiting model", "running", …): the
   * headline and evidence name this, never a hardcoded "running" (see the class doc comment). */
  readonly restingLabel = computed(() => labelOf(this.row().status.state));

  /** Latches once the embedded abandon-action's receipt lands: swaps this card's body from the
   * live "no driver attached" evidence to a compact receipt-only view (see the class doc's EXIT
   * note). Never unset: once abandoned, always abandoned, for the remainder of this card's life. */
  readonly abandoned = signal(false);

  onReceipted(): void {
    this.abandoned.set(true);
    this.receipted.emit();
  }

  openTimeline(): void {
    this.viewService.openRun(this.row().run);
  }
}
