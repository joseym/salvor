import { Component, computed, inject, input, output } from '@angular/core';
import type { RunSummary } from '@salvor/client';

import { ViewService } from '../../core/view';
import { age } from '../runs/run-model';
import { shortId } from './inbox-model';
import { RunRef } from './run-ref';

/**
 * StalledCard: a run that folds to `running` yet the server reports `driver: "none"` for, and whose
 * last event has gone stale — a DERIVED state (see `run-model.ts#derivedStatus`), never a server
 * status. It is going nowhere: resolved but never re-driven, its driver crashed, or its client
 * abandoned it.
 *
 * Unlike every other Inbox card, a stalled card carries NO in-app action. The other cards each own
 * the one call that unblocks their run (resume, resolve, raise-the-limit); a stall is unblocked by
 * restarting the HOST PROCESS that drives the run, which lives outside this browser entirely. So
 * this card is honest about that: it states the evidence, gives the guidance ("restart the driver,
 * or resolve/abandon"), and links to the run — it never renders a fake fix button the dashboard
 * could not honor.
 *
 * It is also LIVE, not sticky: the parent derives the stalled set fresh from each list load, so if a
 * driver reattaches and the run resumes moving, its card simply stops being derived and disappears —
 * the opposite of the commit cards, which persist in place to carry their receipt. A stall has no
 * receipt to protect, so there is nothing to pin.
 */
@Component({
  selector: 'bridge-stalled-card',
  imports: [RunRef],
  templateUrl: './stalled-card.html',
})
export class StalledCard {
  private readonly viewService = inject(ViewService);

  readonly row = input.required<RunSummary>();
  /** Whether this card's Evidence is the one open in the parent's panel (drives aria-pressed). */
  readonly evidencePressed = input(false);
  /** READ, never act: asks the parent to show this run's recorded evidence in the side panel. */
  readonly evidence = output<void>();

  readonly ns = computed(() => shortId(this.row().run));
  /** "10m", "2h" — the relative age of the last recorded event, the "gone quiet" evidence. */
  readonly lastAge = computed(() => age(this.row().lastRecordedAt));

  openTimeline(): void {
    this.viewService.openRun(this.row().run);
  }
}
