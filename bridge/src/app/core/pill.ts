import { Injectable, type Signal, computed, signal } from '@angular/core';

import { type ConnectionState } from './api';

/**
 * The connection pill's shared display state. The pill lives in the app shell (`#conn`) but its
 * truth comes from wherever a stream is actually open — today only the Inspector, which pushes its
 * stream's {@link ConnectionState} here (Live while the SSE stream delivers frames, Ended on
 * the terminal frame). Everywhere else the pill is an honest Snapshot with an as-of time.
 *
 * This is a DISPLAY mirror, not a second state machine: the Inspector's channel owns the real
 * transitions (see `connection-state.ts`, which has no public setter); this service only carries
 * the current label to the one element that renders it, so the shell need not know about streams.
 */
@Injectable({ providedIn: 'root' })
export class PillService {
  private readonly _state = signal<ConnectionState>({
    kind: 'idle',
    label: 'Snapshot',
    asOf: new Date().toISOString(),
  });
  private readonly _runId = signal<string | undefined>(undefined);
  private readonly _runState = signal<string | undefined>(undefined);

  readonly state: Signal<ConnectionState> = this._state.asReadonly();
  readonly runId: Signal<string | undefined> = this._runId.asReadonly();
  /** The bound run's TRUE resting state slug (folded by the Inspector — `suspended`, `completed`,
   * `budget_exceeded`, …). The transport `state` above says whether the STREAM is live or ended;
   * this says what the RUN itself is resting at, so a caught-up stream over a parked run is not
   * mislabeled as an ended run. Undefined when no run is bound (a plain Snapshot). */
  readonly runState: Signal<string | undefined> = this._runState.asReadonly();
  readonly kind = computed(() => this._state().kind);

  /** Mirror a stream's live state (called by the Inspector from its channel), together with the
   * folded resting state of the run it is streaming. */
  set(state: ConnectionState, runId?: string, runState?: string): void {
    this._state.set(state);
    this._runId.set(runId);
    this._runState.set(runState);
  }

  /** Fall back to a static snapshot (no stream open) as of `asOf` (default: now). */
  toSnapshot(asOf?: string): void {
    this._state.set({ kind: 'idle', label: 'Snapshot', asOf: asOf ?? new Date().toISOString() });
    this._runId.set(undefined);
    this._runState.set(undefined);
  }
}
