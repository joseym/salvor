import { Injectable, inject } from '@angular/core';
import type { SalvorEvent } from '@salvor-run/client';

import { SALVOR_CLIENT } from '../../core/api';
import { agentOf, completedCallsOf, costOfPrefix } from '../inspector/state-model';
import { type CostTotal } from '../inspector/pricing';
import { FoldService, type RunStateJson, toWireLog } from '../inspector/wasm-fold';

/**
 * Spend pays the fold cost the run list refuses to pay: for a dollar figure it needs the per-call
 * model id, and the only place that lives is the log. This reads a run's full recorded log (its
 * whole event stream, seq 0 through however many events `GET /v1/runs` had already counted when
 * the ledger was fetched) and folds it through the SAME wasm `derive_state` the Inspector uses
 * ({@link FoldService}) — never a second reimplementation of the fold — plus the shared price
 * table ({@link costOfPrefix}) for the dollar total.
 */

/** One run's folded figures, honest about what could and could not be read. */
export interface FoldedRun {
  readonly id: string;
  /** Every event this run's log actually recorded, up to the prefix this fold could read. */
  readonly events: readonly SalvorEvent[];
  /** False when fewer events came back than `GET /v1/runs` had counted — the log could not be
   *  read in full (a store error, a run that vanished between the list fetch and the read). The
   *  figures below are then a partial fold, never silently presented as the whole run. */
  readonly readable: boolean;
  readonly agent: string | null;
  readonly usage: { readonly inputTokens: number; readonly outputTokens: number };
  readonly cost: CostTotal;
  /** How many `ModelCallCompleted` events this run's log recorded. A run that made ZERO model
   *  calls (a stalled run that died before any call completed — RunStarted, maybe a dangling
   *  ModelCallRequested, then abandoned) has nothing to price at all: {@link CostTotal.complete}
   *  reads `true` for it VACUOUSLY (no calls means no unpriced model to report), which is honest
   *  as a per-run figure ($0 spent so far) but must not be read as "this run's model IS priced" —
   *  see {@link allUnpriced}, which excludes zero-call runs from that determination for exactly
   *  this reason. */
  readonly calls: number;
  /** The wasm-derived state, when the fold itself succeeded on the events read. */
  readonly state: RunStateJson | undefined;
}

/**
 * THE PAGE-WIDE UNPRICED LEDE'S CONDITION, in one place (defect B). Every run that made AT LEAST
 * ONE model call is unpriced, and at least one such run exists.
 *
 * A run with ZERO model calls (a stalled run that died before any call completed) is EXCLUDED from
 * this check on both sides: it must not count as "priced" (its {@link CostTotal.complete} reads
 * `true` vacuously — there is no unpriced model to report when there were no calls at all — so
 * without this exclusion a single such run would wrongly suppress a real all-unpriced banner), and
 * it must not count as "unpriced" either (there is no unpriced FACT to state about a run that never
 * called a model). Only `readable` runs are considered — an unreadable log has no honest verdict.
 */
export function allUnpriced(runs: readonly FoldedRun[]): boolean {
  const priced = runs.filter((f) => f.readable && f.calls > 0);
  return priced.length > 0 && priced.every((f) => !f.cost.complete);
}

/** Precedence for what a run's ceiling was: the log's own `BudgetExceeded` (authoritative — it is
 *  the limit that was actually in force), or genuinely unknown. This build has no `GET /v1/agents`
 *  registry wired (the same honest gap the Inspector's run-stats hero already declares), so unlike
 *  the design that shipped a registry lookup, a ceiling that was never crossed has no second place
 *  to be read from here. */
export interface Ceiling {
  readonly usd: number | null;
  readonly src: 'crossed' | 'unknown';
}

export function ceilingFor(state: RunStateJson | undefined): Ceiling {
  if (state?.status.kind === 'BudgetExceeded') return { usd: state.status.budget.limit, src: 'crossed' };
  return { usd: null, src: 'unknown' };
}

@Injectable({ providedIn: 'root' })
export class CostFoldService {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly fold = inject(FoldService);

  /** Fold one run: read its log up to `expectedEvents` (the count `GET /v1/runs` already knows,
   *  so a still-driving run's stream is read to a bounded prefix rather than tailed forever), then
   *  derive its state through the shared wasm fold and its cost through the shared price table. */
  async foldRun(runId: string, expectedEvents: number): Promise<FoldedRun> {
    const events = await this.readLog(runId, expectedEvents);
    const readable = events.length >= expectedEvents;

    await this.fold.ready();
    let state: RunStateJson | undefined;
    try {
      state = events.length > 0 ? this.fold.deriveState(toWireLog(events), events.length) : undefined;
    } catch {
      state = undefined; // a log the wasm fold itself refused — honest absence, not a fabricated zero
    }

    return {
      id: runId,
      events,
      readable,
      agent: agentOf(events),
      usage: state
        ? { inputTokens: state.usage.input_tokens, outputTokens: state.usage.output_tokens }
        : { inputTokens: 0, outputTokens: 0 },
      cost: costOfPrefix(events, events.length),
      calls: completedCallsOf(events).length,
      state,
    };
  }

  /** Drain the SDK's event stream for `runId` from seq 0, stopping as soon as `expected` events
   *  have arrived (or the stream itself rests). A run still in progress never rests on its own —
   *  the server tails it live — so this bounds the read to the prefix the list already counted,
   *  which is what makes it a snapshot fold rather than an indefinite hang. Every one of those
   *  events is already durably recorded, so the bound is always reachable. */
  private async readLog(runId: string, expected: number): Promise<SalvorEvent[]> {
    if (expected <= 0) return [];
    const events: SalvorEvent[] = [];
    const stream = this.client.streamEvents(runId, { fromSeq: 0 });
    const iterator = stream[Symbol.asyncIterator]();
    try {
      while (events.length < expected) {
        const result = await iterator.next();
        if (result.done) break;
        events.push(result.value);
      }
    } catch {
      // an unreadable log: whatever prefix was actually read stands, honestly partial
    } finally {
      if (iterator.return) {
        try {
          await iterator.return();
        } catch {
          /* best-effort unwind, matching RunEventsChannel's disconnect() */
        }
      }
    }
    return events;
  }
}
