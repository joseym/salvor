import type { SalvorEvent } from '@salvor-run/client';

import {
  GROUP,
  LABEL,
  type Group,
  type Overdue,
  durationLabel,
  groupOf,
  labelOf,
  overdueSince,
} from '../runs/run-model';
import { esc } from '../../shared/json-hi';
import { type CompletedCall, type CostTotal, costOf, int } from './pricing';
import type { RunStateJson, RunStatusJson } from './wasm-fold';

/**
 * The bridge between the wasm fold's `RunStateJson` and this app's presentation vocabulary.
 *
 * The wasm returns a PascalCase, `kind`-tagged status; this build's status classes, groups and
 * labels are the server's snake_case slugs (see `run-model.ts`, keyed on the live control plane's
 * `status.state`). This module maps the one onto the other, and re-derives the presentation facts
 * the header/band/derived-panel need (cost from the price table, a readable pending line) from the
 * same fold + the log, never from a second reimplementation of `derive_state`.
 */

/** wasm status kind → the server's snake_case state slug (the vocabulary `run-model.ts` keys on). */
export function statusStateOf(status: RunStatusJson): string {
  switch (status.kind) {
    case 'NotStarted':
      return 'not_started';
    case 'Running':
      return 'running';
    case 'AwaitingModel':
      return 'awaiting_model';
    case 'AwaitingTool':
      return 'awaiting_tool';
    case 'Suspended':
      return 'suspended';
    case 'Sleeping':
      return 'sleeping';
    case 'BudgetExceeded':
      return 'budget_exceeded';
    case 'NeedsReconciliation':
      return 'needs_reconciliation';
    case 'Completed':
      return 'completed';
    case 'Failed':
      return 'failed';
    case 'Abandoned':
      return 'abandoned';
  }
}

/** The plain, non-interactive status pill: a statement of fact, not a control. */
export function statusHtml(state: string): string {
  return `<span class="status s-${state}" data-group="${groupOf(state)}">${esc(labelOf(state))}</span>`;
}

export { GROUP, LABEL, groupOf, labelOf };

export function isWaitingState(state: string): boolean {
  return groupOf(state) === 'waiting';
}
export function isTerminalState(state: string): boolean {
  return groupOf(state) === 'terminal';
}

/**
 * The `waiting_on` discriminator when a wasm status is `Suspended`, else `undefined`: the one
 * place Inspector code unpacks the tag, so every `groupOf`/`derivedStatus` call downstream reads
 * it without repeating the `kind === 'Suspended'` check by hand. `'signal'` means an external
 * system, not a person, is expected to resume the run; absent means a human gate (what every
 * suspension folded before the discriminator existed means). Mirrors `run-model.ts#waitingOnOf`,
 * which reads the same fact off the server's raw JSON instead of the wasm's tagged union.
 */
export function waitingOnOfStatus(status: RunStatusJson): 'signal' | undefined {
  return status.kind === 'Suspended' ? status.waiting_on : undefined;
}

/** Whether a wasm status is parked on an external signal rather than a person. */
export function isSignalWait(status: RunStatusJson): boolean {
  return waitingOnOfStatus(status) === 'signal';
}

/**
 * Overdue-ness for a `Sleeping` wasm status: whether its `wake_at` has passed `now`, and for how
 * long. Purely clock-derived, and the ONLY way the Inspector can tell: the wasm fold has no clock
 * of its own (it only ever sees the recorded log; see `wasm-fold.ts`), unlike the server-fed runs
 * list, which prefers the server's own computed field when one is present ({@link overdueOf} in
 * `run-model.ts`). `undefined` for every status but `Sleeping`.
 */
export function overdueOfStatus(status: RunStatusJson, now: number = Date.now()): Overdue | undefined {
  return status.kind === 'Sleeping' ? overdueSince(status.wake_at, now) : undefined;
}

/**
 * The sleeping band's inner sentence, as HTML: calm before the deadline ("Sleeping. Wakes at
 * ..."), an attention-toned statement of fact once `wake_at` has passed with nothing having woken
 * the run ("Overdue. Was due at ..., ...ago, and nothing has woken it."), plus the one thing an
 * operator can do about it: wake it by hand. `wakeClock` is the caller's already-formatted,
 * already-escaped wake time (see `event-model.ts#clock`): kept a plain string parameter, not a raw
 * ISO instant, so this stays a pure function with no clock-formatting or escaping of its own for
 * `state-model.spec.ts` to exercise directly.
 */
export function sleepingBandHtml(wakeClock: string, overdue: Overdue): string {
  if (overdue.overdue) {
    const ago = overdue.overdueSeconds !== undefined ? `${durationLabel(overdue.overdueSeconds)} ago` : 'and counting';
    return `<b>Overdue.</b> Was due at <span class="mono">${wakeClock}</span>, ${ago}, and nothing has woken it. Wake it by hand with <span class="mono">salvor wake</span>.`;
  }
  return `<b>Sleeping.</b> Wakes at <span class="mono">${wakeClock}</span>; nothing to do until then. The run continues on its own once the instant passes.`;
}

/**
 * The group a wasm status belongs to: `groupOf` (run-model.ts) extended to see the status's own
 * `waiting_on` discriminator, so a signal wait folds to `progress` here exactly as it does for the
 * server-driven Runs ledger, without the caller unpacking the tag itself.
 */
export function groupOfStatus(status: RunStatusJson): Group {
  return groupOf(statusStateOf(status), waitingOnOfStatus(status));
}

/** Extract the completed model calls from a log prefix, for {@link costOf}. */
export function completedCallsOf(events: readonly SalvorEvent[]): CompletedCall[] {
  const calls: CompletedCall[] = [];
  for (const e of events) {
    if (e.kind !== 'ModelCallCompleted') continue;
    const usage = (e.payload['usage'] ?? {}) as Record<string, unknown>;
    calls.push({
      model: String(e.payload['model'] ?? ''),
      inputTokens: Number(usage['input_tokens'] ?? 0),
      outputTokens: Number(usage['output_tokens'] ?? 0),
    });
  }
  return calls;
}

/** The folded cost of the first `n` events: REAL here, because the Inspector has the log. */
export function costOfPrefix(events: readonly SalvorEvent[], n: number): CostTotal {
  return costOf(completedCallsOf(events.slice(0, n)));
}

/** How many model turns the prefix has taken: a step is a `ModelCallRequested`. */
export function stepsOf(events: readonly SalvorEvent[], n: number): number {
  return events.slice(0, n).filter((e) => e.kind === 'ModelCallRequested').length;
}

/**
 * The derived-panel `pending` line. The wasm's `pending_call` names the dangling intent; for a
 * model call we look its model id up from the log at that seq (the wasm carries only the request
 * hash), so the line names the model rather than just "model".
 */
export function pendingLabel(state: RunStateJson, events: readonly SalvorEvent[]): string {
  const pc = state.pending_call;
  if (!pc) return '-';
  if (pc.kind === 'Model') {
    const at = events.find((e) => e.seq === pc.seq);
    const model = at ? String(at.payload['model'] ?? '') : '';
    return model ? `model · ${model}` : 'model';
  }
  return `tool · ${pc.tool} (${pc.effect})`;
}

/** The agent hash a run records on its `RunStarted`, or null. Real: read from the log. */
export function agentOf(events: readonly SalvorEvent[]): string | null {
  const first = events[0];
  if (first && first.kind === 'RunStarted') {
    return (first.payload['agent_def_hash'] as string | undefined) ?? null;
  }
  return null;
}

export function isHash(v: string): boolean {
  return /^sha256:/.test(v);
}

/** `1234` → `1,234`, re-exported so the component reads one numbers helper. */
export { int };
