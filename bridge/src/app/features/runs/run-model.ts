import type { RunStatus, RunSummary } from '@salvor-run/client';

/**
 * The Runs view-model: the fold's three-way GROUP split and per-state labels, plus a thin
 * adapter from the SDK's {@link RunSummary} to the flat {@link RunRow} the table, filter,
 * health strip and detail panel all read.
 *
 * The live control plane serialises status `state` in SNAKE_CASE (`awaiting_model`,
 * `budget_exceeded`, `needs_reconciliation`, `not_started`; see `crates/salvor-server/src/json.rs`),
 * not hyphenated slugs, so the maps below are keyed on the server's snake_case strings.
 * `completed`/`failed`/`running`/`suspended` are single words, so only the multi-word states move.
 */
export type Group = 'progress' | 'waiting' | 'terminal';

/**
 * state → group, keyed on the server's snake_case `status.state`, plus one
 * DERIVED state the server never sends: `stalled` (see {@link derivedStatus}).
 * A stalled run is waiting on a PERSON (someone must restart its driver or
 * resolve/abandon it), so it groups with `waiting`, not `progress`: it is not
 * motion, and the health strip must not count it as such.
 *
 * `suspended` below is the HUMAN-GATE default: a run parked awaiting input from a person, which is
 * what every suspension recorded before the server's `kind` discriminator existed means, and what
 * an absent discriminator still means today. A suspension parked on an external SIGNAL instead
 * (`kind: "signal"` on the wire, `waiting_on: "signal"` on the wasm fold) reads as `progress`
 * instead, the same call `sleeping` makes below and for the same reason: nobody OWES it action, so
 * it must never sit in the Inbox. This table alone cannot make that call (it is keyed on `state`
 * alone, and both waits share the one `suspended` state), so {@link groupOf} overrides its answer
 * for that one case; see its own doc comment for how.
 */
export const GROUP: Readonly<Record<string, Group>> = {
  running: 'progress',
  awaiting_model: 'progress',
  awaiting_tool: 'progress',
  not_started: 'progress',
  /**
   * PROGRESS, not waiting: mirrors the CLI's own call exactly (`status_group` in
   * `crates/salvor-cli-core/src/render.rs`). `waiting` means a PERSON is the only thing that will
   * move this run; a `sleeping` run is parked on a durable timer and continues on its OWN once its
   * `wake_at` instant arrives, whether anyone reads the list or not. Grouping it with the approval
   * queue would put a run nobody can act on into the one group that exists to be a to-do list, so
   * it must never sit in the Inbox.
   */
  sleeping: 'progress',
  suspended: 'waiting',
  budget_exceeded: 'waiting',
  needs_reconciliation: 'waiting',
  stalled: 'waiting',
  completed: 'terminal',
  failed: 'terminal',
  // Operator-abandoned: a terminal resting state, never attention. It groups
  // with completed/failed so the health strip counts it as terminal and the
  // status filter reaches it through the same enumerable path (see json.rs).
  abandoned: 'terminal',
};

/** state → human label. */
export const LABEL: Readonly<Record<string, string>> = {
  not_started: 'not started',
  running: 'running',
  awaiting_model: 'awaiting model',
  awaiting_tool: 'awaiting tool',
  sleeping: 'sleeping',
  suspended: 'suspended',
  budget_exceeded: 'budget exceeded',
  needs_reconciliation: 'needs reconciliation',
  stalled: 'stalled',
  completed: 'completed',
  failed: 'failed',
  abandoned: 'abandoned',
};

/**
 * How long a `running` run may report `driver: "none"` before the dashboard
 * calls it STALLED. This graces the momentary gap between one driver task
 * ending and the next re-driving it, and the just-opened case, so a run is
 * never flagged the instant it appears driverless; only once it has genuinely
 * gone quiet. The server's own lease TTL and task tracking already make
 * `driver: "none"` mean "no driver right now"; this is the client-side quiet
 * period on top, the "+stale" half of the rule.
 */
export const STALL_GRACE_MS = 10_000;

/**
 * THE STALLED DERIVATION, in one place both the ledger row, the Inbox, and the Inspector read.
 *
 * A run is `stalled` when ALL THREE hold (IN PROGRESS, driverless, and stale):
 *   1. its folded status is in the IN-PROGRESS family: `groupOf(state) === 'progress'`
 *      (`running`, `awaiting_model`, `awaiting_tool`, `not_started`: every non-terminal resting
 *      state where a driver SHOULD be attached to move it forward). This is deliberately wider
 *      than literal `running`: a real client-driven run that dies mid model-call folds to
 *      `awaiting_model`, not `running`, and it is exactly as stalled. The HUMAN-WAITING family
 *      (`suspended`/`budget_exceeded`/`needs_reconciliation`) is excluded on purpose: those are
 *      their own honest waits on a PERSON, not on a driver, so a driverless one of THOSE is
 *      normal, not stalled, and so is every terminal state,
 *   2. the server's liveness evidence says `driver: "none"`: no task drives it
 *      and no client lease is current. STRICT: an ABSENT driver field (an older
 *      server, or a terminal run) is not evidence of a stall, so it is never
 *      derived as one: honesty over a guess, and
 *   3. its last event is older than {@link STALL_GRACE_MS}.
 *
 * `sleeping` is carved out BEFORE any of the three checks run, even though it groups with
 * `progress` (see {@link GROUP}): a run parked on a durable timer holds no driver BY DESIGN, for
 * as long as its nap lasts, so `driver: "none"` on a sleeping run is the honest resting state, not
 * evidence of anything gone quiet. Without this carve-out every sleeping run older than
 * {@link STALL_GRACE_MS} would misread as stalled the instant its own design (no driver while
 * asleep) is mistaken for a driver that dropped.
 *
 * A `suspended` run parked on an external SIGNAL (`waitingOn === 'signal'`) gets the identical
 * carve-out, for the identical reason: once {@link groupOf} answers `progress` for it, the driver
 * check below would otherwise fire, and nobody holds a driver for a run parked on a webhook either.
 *
 * Returns the effective status: `stalled` when the rule fires, otherwise the
 * server's own `state` unchanged. This is the client's verdict over the
 * server's evidence, the same division of labor `status` itself has.
 */
export function derivedStatus(
  state: string,
  driver: string | undefined,
  last: string | undefined,
  now: number = Date.now(),
  waitingOn?: 'signal',
): string {
  if (state === 'sleeping') return state;
  if (state === 'suspended' && waitingOn === 'signal') return state;
  if (groupOf(state, waitingOn) !== 'progress' || driver !== 'none') return state;
  const lastMs = last ? Date.parse(last) : Number.NaN;
  const ageMs = Number.isNaN(lastMs) ? Number.POSITIVE_INFINITY : now - lastMs;
  return ageMs >= STALL_GRACE_MS ? 'stalled' : state;
}

/**
 * The group a state belongs to, defaulting unknown states to `progress` (never silently dropped).
 *
 * `waitingOn` is the one override {@link GROUP} cannot itself express, because the table is keyed
 * on `state` alone and a human gate and a signal wait share the one `suspended` state: pass it
 * `'signal'` (the discriminator the server's `kind` / the wasm fold's `waiting_on` carries, absent
 * for a human gate) and a `suspended` state reads `progress` instead of the table's `waiting`
 * default, the same as `sleeping`. THE LEAST INVASIVE EXTENSION: an optional trailing parameter
 * that defaults to `undefined`, so every existing single-argument call site (the Runs ledger's
 * badge counts, sort, row classes, and the filter vocabulary among them) keeps reading a gate's
 * `state` exactly as before, unchanged, without being taught about the discriminator at all.
 */
export function groupOf(state: string, waitingOn?: 'signal'): Group {
  if (state === 'suspended' && waitingOn === 'signal') return 'progress';
  return GROUP[state] ?? 'progress';
}
export function labelOf(state: string): string {
  return LABEL[state] ?? state.replace(/_/g, ' ');
}
export function isWaiting(state: string, waitingOn?: 'signal'): boolean {
  return groupOf(state, waitingOn) === 'waiting';
}

/**
 * The `waiting_on` discriminator read straight off a `RunStatus`'s raw JSON (`status.raw.kind`;
 * see `crates/salvor-server/src/json.rs`): the SDK's typed {@link RunStatus} does not surface it
 * as its own field, only on `raw`, so this is the one place that unpacks it. `'signal'` means an
 * external system, not a person, will resume the run; `undefined` for every other state and for a
 * `suspended` run recorded before the discriminator existed, which is a human gate.
 */
export function waitingOnOf(status: RunStatus): 'signal' | undefined {
  return status.state === 'suspended' && status.raw['kind'] === 'signal' ? 'signal' : undefined;
}

/** Token usage, flattened. */
export interface RunUsage {
  readonly inputTokens: number;
  readonly outputTokens: number;
}

/**
 * One row of the ledger: a projection of `GET /v1/runs`, flat and view-ready. Every value here
 * is something the list endpoint actually carries; nothing is folded client-side. `usage`,
 * `stepCount`, `agentDefHash` and `labels` are present when the server could fold the run's log
 * and genuinely absent (undefined, never a fabricated zero or an invented `{}`) when it could not.
 * `labels` follows the rule one step further: also absent when the run recorded none at all.
 */
export interface RunRow {
  readonly id: string;
  /** The EFFECTIVE status: the server's folded `state`, or the client-derived
   * `stalled` when {@link derivedStatus} fires. Every downstream consumer (the
   * pill, the group split, the sort, the status filter) reads this one field, so
   * the stalled verdict is made once and never drifts between surfaces. */
  readonly status: string;
  readonly eventCount: number;
  readonly first?: string;
  readonly last?: string;
  /** The server's liveness evidence, verbatim: `attached`/`none`, or absent for
   * a terminal run (and an older server). Kept so a surface can show WHY a run
   * is stalled ("no driver attached") straight from the evidence. */
  readonly driver?: 'attached' | 'none';
  readonly usage?: RunUsage;
  readonly stepCount?: number;
  readonly agentDefHash?: string;
  readonly labels?: Readonly<Record<string, string>>;
  /** The `waiting_on` discriminator (see {@link waitingOnOf}), carried onto the row so a
   * `suspended` row's group/waiting classification can be re-derived correctly wherever the row
   * travels, without every reader re-reading `raw` by hand. Absent for every state but a signal
   * wait's `suspended`. */
  readonly waitingOn?: 'signal';
}

export function toRunRow(s: RunSummary, now: number = Date.now()): RunRow {
  const driver = s.driver === 'attached' || s.driver === 'none' ? s.driver : undefined;
  const waitingOn = waitingOnOf(s.status);
  return {
    id: s.run,
    status: derivedStatus(s.status.state, driver, s.lastRecordedAt, now, waitingOn),
    eventCount: s.eventCount,
    first: s.firstRecordedAt,
    last: s.lastRecordedAt,
    driver,
    usage: s.usage ? { inputTokens: s.usage.inputTokens, outputTokens: s.usage.outputTokens } : undefined,
    stepCount: s.stepCount,
    agentDefHash: s.agentDefHash,
    labels: s.labels,
    waitingOn,
  };
}

/**
 * A hash-shaped `agent_def_hash` (a salvor-native run, content-addressed by its definition) vs a
 * caller-supplied readable label (a client-driven run, e.g. AARG's `aarg_jd_parser_v1`, is its
 * own label; it was never a hash to begin with). Mirrors the prototype's `isHash` exactly.
 */
export function isHash(value: string): boolean {
  return /^sha256:/.test(value);
}

/** Which of the honest renderings {@link agentIdentity} chose. */
export type AgentIdentityKind = 'none' | 'label' | 'hash' | 'name' | 'graph';

/**
 * A run whose log the server READ and folded: `usage`/`step_count` are present exactly when the
 * fold succeeded (the list omits every folded field together when `read_log` fails: see
 * `crates/salvor-server/src/runs.rs`, the zero-vs-absent rule). So the presence of either is the
 * honest "the log folded" signal, and its absence is the honest "the log could not be read" signal.
 */
function foldedLog(r: RunRow): boolean {
  return r.stepCount !== undefined || r.usage !== undefined;
}

/**
 * The agent column's one source of truth. The run's log records only
 * `agent_def_hash`. The NAME here, when present, is a separate, honestly-sourced lookup (a
 * registry resolution via {@link AgentRegistryService}), never read off the log. The renderings,
 * in the same priority the design's `agentIdentity()` used:
 *
 *   1. no `agent_def_hash`, but the log DID fold → `"graph"` ("graph run"). A folded run with no
 *      recorded `agent_def_hash` is a graph run by construction: its log's head is
 *      `GraphRunStarted`, not the `RunStarted` every agent run records first (server runs.rs), so
 *      it never had a single agent hash to carry. `GET /v1/runs` carries nothing else graph-shaped
 *      for a graph run (the graph_hash lives behind `GET /v1/runs/{id}/graph`), so "graph run" is
 *      the honest, distinct vocabulary entry here, not a fabricated hash.
 *   2. no `agent_def_hash` AND the log did not fold → `"none"` (a hyphen; the endpoint declined
 *      to answer, so genuinely nothing is recorded to show).
 *   3. not hash-shaped → `"label"` (a caller-supplied readable name, shown as-is)
 *   4. hash-shaped, and `names` resolved it → `"name"` (registry-resolved; the hash still rides
 *      along on `.hash` for a title)
 *   5. hash-shaped, unresolved (unregistered, or not yet looked up) → `"hash"` (the hash itself,
 *      truncated + copyable at render time; an absent name is not an error, see
 *      {@link AgentRegistryService})
 *
 * `names` defaults to empty so this stays a pure function callers can use before any lookup has
 * resolved (or in a context, like the filter vocabulary, with no registry in scope).
 */
export interface AgentIdentity {
  readonly text: string;
  readonly kind: AgentIdentityKind;
  readonly hash?: string;
}
export function agentIdentity(
  r: RunRow,
  names: ReadonlyMap<string, string> = EMPTY_NAMES,
): AgentIdentity {
  const h = r.agentDefHash;
  if (!h) return foldedLog(r) ? { text: 'graph run', kind: 'graph' } : { text: '-', kind: 'none' };
  if (!isHash(h)) return { text: h, kind: 'label', hash: h };
  const name = names.get(h);
  if (name) return { text: name, kind: 'name', hash: h };
  return { text: h, kind: 'hash', hash: h };
}
const EMPTY_NAMES: ReadonlyMap<string, string> = new Map();

/** `2026-07-12T08:41:12Z` → `2026-07-12T08:00Z` (the UTC hour a run was LAST active). */
export function hourKey(iso: string | undefined): string {
  return iso ? iso.slice(0, 13) + ':00Z' : '';
}

/** A short relative age from an ISO timestamp against `now` (default: real wall clock). */
export function age(iso: string | undefined, now: number = Date.now()): string {
  if (!iso) return '-';
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return '-';
  const s = Math.max(0, Math.round((now - then) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}
