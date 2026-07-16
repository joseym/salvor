import type { RunSummary } from '@salvor/client';

/**
 * The Runs view-model: the fold's three-way GROUP split and per-state labels, plus a thin
 * adapter from the SDK's {@link RunSummary} to the flat {@link RunRow} the table, filter,
 * health strip and detail panel all read.
 *
 * DIVERGENCE FROM THE PROTOTYPE (filed for the drift ledger): the OD prototype's fixtures use
 * HYPHENATED status slugs (`awaiting-model`, `budget-exceeded`, `needs-reconciliation`,
 * `not-started`). The live control plane serialises status `state` in SNAKE_CASE
 * (`awaiting_model`, `budget_exceeded`, `needs_reconciliation`, `not_started`; see
 * `crates/salvor-server/src/json.rs`). This build consumes the REAL API, so the maps below are
 * keyed on the server's snake_case strings. `completed`/`failed`/`running`/`suspended` are
 * identical in both, so only the multi-word states move.
 */
export type Group = 'progress' | 'waiting' | 'terminal';

/** state → group, keyed on the server's snake_case `status.state`. */
export const GROUP: Readonly<Record<string, Group>> = {
  running: 'progress',
  awaiting_model: 'progress',
  awaiting_tool: 'progress',
  not_started: 'progress',
  suspended: 'waiting',
  budget_exceeded: 'waiting',
  needs_reconciliation: 'waiting',
  completed: 'terminal',
  failed: 'terminal',
};

/** state → human label. */
export const LABEL: Readonly<Record<string, string>> = {
  not_started: 'not started',
  running: 'running',
  awaiting_model: 'awaiting model',
  awaiting_tool: 'awaiting tool',
  suspended: 'suspended',
  budget_exceeded: 'budget exceeded',
  needs_reconciliation: 'needs reconciliation',
  completed: 'completed',
  failed: 'failed',
};

/** The group a state belongs to, defaulting unknown states to `progress` (never silently dropped). */
export function groupOf(state: string): Group {
  return GROUP[state] ?? 'progress';
}
export function labelOf(state: string): string {
  return LABEL[state] ?? state.replace(/_/g, ' ');
}
export function isWaiting(state: string): boolean {
  return groupOf(state) === 'waiting';
}

/** Token usage, flattened. */
export interface RunUsage {
  readonly inputTokens: number;
  readonly outputTokens: number;
}

/**
 * One row of the ledger — a projection of `GET /v1/runs`, flat and view-ready. Every value here
 * is something the list endpoint actually carries; nothing is folded client-side. `usage`,
 * `stepCount` and `agentDefHash` are present when the server could fold the run's log and
 * genuinely absent (undefined, never a fabricated zero) when it could not.
 */
export interface RunRow {
  readonly id: string;
  readonly status: string;
  readonly eventCount: number;
  readonly first?: string;
  readonly last?: string;
  readonly usage?: RunUsage;
  readonly stepCount?: number;
  readonly agentDefHash?: string;
}

export function toRunRow(s: RunSummary): RunRow {
  return {
    id: s.run,
    status: s.status.state,
    eventCount: s.eventCount,
    first: s.firstRecordedAt,
    last: s.lastRecordedAt,
    usage: s.usage ? { inputTokens: s.usage.inputTokens, outputTokens: s.usage.outputTokens } : undefined,
    stepCount: s.stepCount,
    agentDefHash: s.agentDefHash,
  };
}

/** `2026-07-12T08:41:12Z` → `2026-07-12T08:00Z` (the UTC hour a run was LAST active). */
export function hourKey(iso: string | undefined): string {
  return iso ? iso.slice(0, 13) + ':00Z' : '';
}

/** A short relative age from an ISO timestamp against `now` (default: real wall clock). */
export function age(iso: string | undefined, now: number = Date.now()): string {
  if (!iso) return '—';
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return '—';
  const s = Math.max(0, Math.round((now - then) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}
