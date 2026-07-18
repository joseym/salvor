import { Injectable, type Signal, signal } from '@angular/core';

/**
 * A fork the operator asked for from OUTSIDE the canvas — the Inspector's scrubber offer or the
 * Runs detail panel. Both go through this ONE door so the canvas is the only place that decides
 * what a fork request means (which graph, which run, whether a node still has to be picked) and
 * `openFork` stays the only owner of every refusal. A second copy of the parked-origin rule is
 * precisely how two doors begin to disagree about what is safe to do twice.
 *
 * `nodeId` is set only when the caller genuinely knows the fork point (the event under the
 * Inspector's playhead names a node). A Runs list row names no node, so it omits the field and
 * the canvas enters its node-picking state rather than guessing one.
 *
 * The `nonce` makes every request a fresh object, so asking to fork the same run twice in a row
 * still re-fires the consumer's effect.
 */
export interface ForkIntent {
  readonly runId: string;
  readonly nodeId?: string;
  readonly nonce: number;
}

@Injectable({ providedIn: 'root' })
export class ForkIntentService {
  private readonly _intent = signal<ForkIntent | undefined>(undefined);

  readonly intent: Signal<ForkIntent | undefined> = this._intent.asReadonly();

  request(runId: string, nodeId?: string): void {
    this._intent.set(nodeId !== undefined ? { runId, nodeId, nonce: Date.now() } : { runId, nonce: Date.now() });
  }

  /** The consumer (the canvas) clears the intent once it has acted on it. */
  clear(): void {
    this._intent.set(undefined);
  }
}
