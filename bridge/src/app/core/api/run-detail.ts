import { Injectable, type Signal, inject, signal } from '@angular/core';
import type { ResumeResult, RunState } from '@salvor/client';

import { SALVOR_CLIENT } from './client';
import { errorMessage } from './errors';

/**
 * One run's derived-state surface: `GET /v1/runs/{id}`, plus the two write verbs that
 * change it, `resume` and `resolve`. A single instance tracks the currently-loaded run
 * (the app shows one Inspector detail panel at a time, per the prototype); loading a
 * different id replaces it.
 *
 * `resume` and `resolve` both re-derive `run` from the server's response so every reader
 * of {@link run} sees the consequence of the write without a manual re-fetch — consequence
 * re-rendering after resume/resolve starts
 * here; the Inbox UI reads it.
 */
@Injectable({ providedIn: 'root' })
export class RunDetailService {
  private readonly client = inject(SALVOR_CLIENT);

  private readonly _run = signal<RunState | undefined>(undefined);
  private readonly _loading = signal(false);
  private readonly _error = signal<string | undefined>(undefined);

  readonly run: Signal<RunState | undefined> = this._run.asReadonly();
  readonly loading: Signal<boolean> = this._loading.asReadonly();
  readonly error: Signal<string | undefined> = this._error.asReadonly();

  /** Fetch one run's derived state and make it the current {@link run}. */
  async load(runId: string): Promise<RunState> {
    this._loading.set(true);
    this._error.set(undefined);
    try {
      const state = await this.client.getRun(runId);
      this._run.set(state);
      return state;
    } catch (err) {
      this._error.set(errorMessage(err));
      throw err;
    } finally {
      this._loading.set(false);
    }
  }

  /**
   * Continue a parked or crashed run. A `NeedsReconciliationError` (from
   * `@salvor/client`) propagates uncaught — the caller is expected to route it to the
   * reconciliation path (`resolve`), per the SDK's own documented contract; this method
   * does not swallow it.
   */
  async resume(runId: string, input?: unknown): Promise<ResumeResult> {
    const result = await this.client.resume(runId, input);
    await this.load(runId);
    return result;
  }

  /** Record a dangling write's completion by hand, unsticking the run. */
  async resolve(runId: string, output: unknown): Promise<RunState> {
    const state = await this.client.resolve(runId, output);
    this._run.set(state);
    return state;
  }
}
