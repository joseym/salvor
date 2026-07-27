import { Injectable } from '@angular/core';
import type { SalvorEvent } from '@salvor-run/client';
import init, { deriveState as wasmDeriveState, eventCount as wasmEventCount } from 'salvor-replay-wasm';

/**
 * The scrubber's fold, on the REAL wasm module (`salvor-replay-wasm`) — the same pure
 * `derive_state` the runtime runs, compiled to wasm32. No TypeScript reimplementation: the whole
 * architectural payoff is same-fold-no-drift, so the Inspector folds prefixes through this and
 * never through a hand-written copy. Cost is the one thing computed in the UI (the price table is
 * a UI concern the fold does not carry); everything else — status, usage, next_seq, the dangling
 * call — is the wasm's answer verbatim.
 */

/** Effect class of a tool call. Mirrors `salvor_replay::Effect`'s wire form. */
export type Effect = 'read' | 'idempotent' | 'write';

export type BudgetKind = 'tokens' | 'cost_usd' | 'wall_time' | 'steps';

export interface Budget {
  kind: BudgetKind;
  limit: number;
}

export interface TokenTotals {
  input_tokens: number;
  output_tokens: number;
}

/** Where the run stands, as the `kind`-tagged union the wasm serialises. */
export type RunStatusJson =
  | { kind: 'NotStarted' }
  | { kind: 'Running' }
  | { kind: 'AwaitingModel' }
  | { kind: 'AwaitingTool' }
  | { kind: 'Suspended'; reason: string; input_schema: unknown }
  | { kind: 'BudgetExceeded'; budget: Budget; observed: number }
  | { kind: 'NeedsReconciliation' }
  | { kind: 'Completed'; output: unknown }
  | { kind: 'Failed'; error: string }
  | {
      kind: 'Abandoned';
      reason?: string;
      unresolved_write?: { seq: number; tool: string };
    };

export type PendingCallJson =
  | { kind: 'Model'; seq: number; request_hash: string }
  | {
      kind: 'Tool';
      seq: number;
      tool: string;
      input: unknown;
      effect: Effect;
      idempotency_key?: string;
    };

/**
 * The parsed result of `deriveState`. Pinned to `salvor-replay-wasm/types` `RunStateJson`; the
 * package's `surface_pin` Rust tests lock the serialised bytes, so if this drifts those fail.
 */
export interface RunStateJson {
  status: RunStatusJson;
  next_seq: number;
  usage: TokenTotals;
  pending_call?: PendingCallJson;
}

/**
 * Reconstruct the store's wire log JSON from the SDK's decoded events, so the wasm parser sees the
 * exact envelope shape it expects (`{ run_id, seq, schema_version, recorded_at, event }`). The SDK
 * hoists `kind`/`payload` out of `event` and camel-cases the envelope; this puts them back.
 */
export function toWireLog(events: readonly SalvorEvent[]): string {
  return JSON.stringify(
    events.map((e) => ({
      run_id: e.runId,
      seq: e.seq,
      schema_version: e.schemaVersion,
      recorded_at: e.recordedAt,
      event: { kind: e.kind, payload: e.payload },
    })),
  );
}

@Injectable({ providedIn: 'root' })
export class FoldService {
  private initPromise?: Promise<void>;
  private inited = false;

  /** Resolve once the wasm module is instantiated. Idempotent; safe to await on every entry. */
  ready(): Promise<void> {
    if (this.inited) return Promise.resolve();
    if (!this.initPromise) {
      this.initPromise = init().then(() => {
        this.inited = true;
      });
    }
    return this.initPromise;
  }

  /** Fold `logJson` (a wire log string from {@link toWireLog}) through its first `prefixLen`
   *  events and return the derived state. Requires {@link ready} to have resolved. */
  deriveState(logJson: string, prefixLen: number): RunStateJson {
    if (!this.inited) throw new Error('FoldService.deriveState called before ready()');
    return JSON.parse(wasmDeriveState(logJson, prefixLen)) as RunStateJson;
  }

  /** The number of events in a wire log — for enumerating scrub positions. */
  eventCount(logJson: string): number {
    if (!this.inited) throw new Error('FoldService.eventCount called before ready()');
    return wasmEventCount(logJson);
  }
}
