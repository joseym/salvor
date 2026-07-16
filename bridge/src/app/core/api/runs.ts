import { Injectable, type Signal, inject, signal } from '@angular/core';
import type { RunSummary } from '@salvor/client';

import { SALVOR_CLIENT } from './client';
import { errorMessage } from './errors';

/**
 * The Runs ledger's list surface: `GET /v1/runs`.
 *
 * `RunSummary` (from `@salvor/client`) already carries the fields `e3182c5` added —
 * `usage`, `stepCount`, `agentDefHash` — additively and honestly absent (never a
 * fabricated zero) when a run's log could not fold; this service does no re-shaping, it
 * just surfaces the SDK's typed decode as a signal. This is a REST snapshot, not a
 * stream: there is no `GET /v1/runs` SSE surface, so freshness here is always "as of
 * `lastLoadedAt`" — a future Runs-list consumer states that honestly rather than
 * borrowing the per-run connection pill's "Live" label for a listing nothing pushes to.
 */
@Injectable({ providedIn: 'root' })
export class RunsService {
  private readonly client = inject(SALVOR_CLIENT);

  private readonly _runs = signal<RunSummary[]>([]);
  private readonly _loading = signal(false);
  private readonly _error = signal<string | undefined>(undefined);
  private readonly _lastLoadedAt = signal<string | undefined>(undefined);

  readonly runs: Signal<RunSummary[]> = this._runs.asReadonly();
  readonly loading: Signal<boolean> = this._loading.asReadonly();
  readonly error: Signal<string | undefined> = this._error.asReadonly();
  readonly lastLoadedAt: Signal<string | undefined> = this._lastLoadedAt.asReadonly();

  /** Re-fetch the full run list. Throws on failure after recording `error`. */
  async refresh(): Promise<RunSummary[]> {
    this._loading.set(true);
    this._error.set(undefined);
    try {
      const runs = await this.client.listRuns();
      this._runs.set(runs);
      this._lastLoadedAt.set(new Date().toISOString());
      return runs;
    } catch (err) {
      this._error.set(errorMessage(err));
      throw err;
    } finally {
      this._loading.set(false);
    }
  }
}
