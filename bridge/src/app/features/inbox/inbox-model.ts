import type { RunState, SalvorClient, SalvorEvent } from '@salvor/client';

/**
 * Shared types and pure math for the Inbox: the budget-raise floor computation, currency/number
 * formatting per budget dimension, the reconciliation evidence shape, and the receipt fetch that
 * turns a resume/resolve response into the "seq, event.kind, recorded_at, payload, re-derived
 * status" receipt the commit pattern promises.
 */

// ── budgets ──────────────────────────────────────────────────────────────────────────────────

/** The four budget dimensions the server can declare (`crates/salvor-replay::event::BudgetKind`,
 * serialized snake_case). A budget is not always `cost_usd` — the real seeded fixture
 * (`bridge/e2e-serve.sh`'s tiny-budget agent) declares a `steps` budget, and `tokens`/`wall_time`
 * are equally real dimensions — so this build's BudgetCard is honest about whichever dimension the
 * run actually crossed, never USD-only floor/propose math or "$…" copy. */
export type BudgetKind = 'steps' | 'tokens' | 'cost_usd' | 'wall_time';

export interface BudgetInfo {
  readonly kind: BudgetKind;
  readonly limit: number;
  readonly observed: number;
}

const KNOWN_BUDGET_KINDS: ReadonlySet<string> = new Set(['steps', 'tokens', 'cost_usd', 'wall_time']);

/** Read the budget info a `budget_exceeded` status carries on `raw` (the typed `RunStatus` has no
 * dedicated `budget`/`observed` field — see `sdks/typescript/src/types.ts`). Absent/malformed data
 * returns `undefined` rather than a fabricated zero. */
export function parseBudgetInfo(statusRaw: Readonly<Record<string, unknown>>): BudgetInfo | undefined {
  const b = statusRaw['budget'];
  if (!b || typeof b !== 'object') return undefined;
  const kind = (b as Record<string, unknown>)['kind'];
  if (typeof kind !== 'string' || !KNOWN_BUDGET_KINDS.has(kind)) return undefined;
  const limit = Number((b as Record<string, unknown>)['limit'] ?? Number.NaN);
  const observed = Number(statusRaw['observed'] ?? Number.NaN);
  if (Number.isNaN(limit) || Number.isNaN(observed)) return undefined;
  return { kind: kind as BudgetKind, limit, observed };
}

/** The resume-input extension key `validate_extension_input` accepts for each dimension
 * (`crates/salvor-cli/src/render.rs::extend_key`) — `wall_time` is the one dimension whose wire key
 * differs from its own name. */
export function extendKey(kind: BudgetKind): string {
  switch (kind) {
    case 'steps':
      return 'steps';
    case 'tokens':
      return 'tokens';
    case 'cost_usd':
      return 'cost_usd';
    case 'wall_time':
      return 'wall_time_seconds';
  }
}

const USD_2 = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });
const USD_4 = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 4,
  maximumFractionDigits: 4,
});

/** `$0.0273` under a dollar, `$1.23` at or above — four decimal places below a dollar so a
 * fractions-of-a-cent per-call cost is never rounded away to `$0.00`. */
export function usd(n: number): string {
  return (Math.abs(n) < 1 ? USD_4 : USD_2).format(n);
}

/** The precision a "round up, never down" ceiling uses per dimension: cents for a dollar figure,
 * whole units for a count (steps/tokens are integral in nature — see `Budget`'s own doc comment),
 * tenths of a second for wall time. The `-1e-9` epsilon guards against floating-point error pushing
 * a value that is already exactly on a boundary up past itself. */
function ceilForKind(kind: BudgetKind, n: number): number {
  if (kind === 'cost_usd') return Math.ceil(n * 100 - 1e-9) / 100;
  if (kind === 'wall_time') return Math.ceil(n * 10 - 1e-9) / 10;
  return Math.ceil(n - 1e-9);
}

/** The honest floor: what the run has ALREADY spent (or its declared limit, if somehow larger),
 * rounded up. Anything below it buys nothing — the run would cross the ceiling again immediately. */
export function budgetFloor(info: BudgetInfo): number {
  return ceilForKind(info.kind, Math.max(info.observed, info.limit));
}

/** THE DASHBOARD'S OWN SUGGESTION, not the run's proposal — a `BudgetExceeded` event carries no
 * proposed extension, only the limit and observed spend, so this figure must never be dressed up as
 * one. Twice the declared ceiling, floored at the honest minimum. */
export function budgetPropose(info: BudgetInfo): number {
  return Math.max(budgetFloor(info), ceilForKind(info.kind, info.limit * 2));
}

/** `$0.0273`, `12,000`, or `45s` depending on the dimension — never a bare unlabeled number for a
 * non-currency budget, and never a dollar sign on one that was never dollars. */
export function formatBudgetValue(kind: BudgetKind, n: number): string {
  if (kind === 'cost_usd') return usd(n);
  if (kind === 'wall_time') return `${n}s`;
  return n.toLocaleString('en-US');
}

/** A human word for the dimension, for the reason sentence ("its declared `steps` ceiling"). */
export function budgetKindLabel(kind: BudgetKind): string {
  return kind === 'wall_time' ? 'wall time' : kind;
}

// ── reconciliation evidence ─────────────────────────────────────────────────────────────────

/**
 * The dangling write intent, as evidence for the reconcile card.
 *
 * The obvious source for this is `RunState.pending` from a plain
 * `GET /v1/runs/{id}` — except that object omits `recorded_at` (`crates/salvor-server/src/json.rs`'s
 * `pending()` never includes it). The other place it DOES appear is a `409 needs_reconciliation`
 * refusal's `details.intent` (`API.md`) — deliberately provoking one (attempting `resume` with no
 * input, which always refuses synchronously and records nothing) was this card's first design, and
 * it is exactly what `salvor resume` itself does at the CLI to show this evidence. It was reverted:
 * a browser logs ANY non-2xx `fetch` response to the console as `Failed to load resource: the server
 * responded with status 409`, unconditionally, regardless of whether application code catches the
 * resulting rejection — and the test suite fails on ANY console error, observed failing exactly
 * this way against a live `needs_reconciliation` run. So `recorded_at` is read the other honest way
 * instead: `RunState.pending.seq` names the exact log position of the intent, and the event stream
 * opened `?from_seq=<that seq>` replays that envelope first — its own `recorded_at` is the answer,
 * off a plain 200 read, never an error response. See {@link fetchAppendedEvent}, reused for this.
 */
export interface ReconcileIntent {
  readonly kind: string;
  readonly seq: number;
  readonly tool: string;
  readonly input: unknown;
  readonly effect: string;
  readonly idempotencyKey: string | null;
  readonly recordedAt: string;
}

/** Build a {@link ReconcileIntent} from a run's `pending` call (a `GET /v1/runs/{id}` field) plus
 * the envelope recorded at that same seq (read via {@link fetchAppendedEvent}, a plain 200). Returns
 * undefined when `pending` is not a tool-kind call — the only shape `needs_reconciliation` derives
 * from (see `crates/salvor-replay/src/state.rs`'s write rule). */
export function reconcileIntentFrom(
  pending: { kind: string; seq: number; tool?: string; effect?: string; input?: unknown; raw: Readonly<Record<string, unknown>> },
  recordedAt: string | undefined,
): ReconcileIntent | undefined {
  if (pending.kind !== 'tool' || typeof pending.tool !== 'string') return undefined;
  const idem = pending.raw['idempotency_key'];
  return {
    kind: pending.kind,
    seq: pending.seq,
    tool: pending.tool,
    input: pending.input,
    effect: pending.effect ?? 'write',
    idempotencyKey: typeof idem === 'string' ? idem : null,
    recordedAt: recordedAt ?? '',
  };
}

// ── the commit receipt ──────────────────────────────────────────────────────────────────────

/** What a commit actually wrote: seq, event.kind, recorded_at, payload — plus the re-derived status
 * a fresh `GET /v1/runs/{id}` reports right after. Nothing here is edited or guessed; every field is
 * read back off the log the write just extended. */
export interface ReceiptVM {
  readonly seq: number | undefined;
  readonly kind: string;
  readonly recordedAt: string | undefined;
  readonly payload: unknown;
  readonly statusState: string;
  readonly endpoint: string;
  /** True only when the appended event itself could not be confirmed within the fetch budget below
   * (see {@link fetchAppendedEvent}) — an honest admission, never a silently stale seq/recorded_at. */
  readonly unconfirmed: boolean;
}

/**
 * Read the ONE event a commit just appended, straight off the log: open the event stream from the
 * sequence position the run was at before the commit, take the first event it yields, and release
 * the connection immediately (mirrors `RunEventsChannel.disconnect()`'s own `iterator.return()`
 * pattern — a one-shot read has no business holding a live SSE connection open).
 *
 * A resume/resolve response (`ResumeResult`/`RunState`) carries no seq/kind/recorded_at/payload of
 * its own (see `API.md`'s response shapes) — this is the one honest way to get them from the real
 * control plane.
 */
export async function fetchAppendedEvent(
  client: SalvorClient,
  runId: string,
  fromSeq: number,
): Promise<SalvorEvent | undefined> {
  const stream = client.streamEvents(runId, { fromSeq });
  const iterator = stream[Symbol.asyncIterator]();
  try {
    const result = await iterator.next();
    return result.done ? undefined : result.value;
  } finally {
    await iterator.return?.().catch(() => undefined);
  }
}

const APPENDED_EVENT_TIMEOUT_MS = 5000;

/** {@link fetchAppendedEvent}, bounded: the append is already durable by the time resume/resolve's
 * own response arrives (both record their one event synchronously before responding), so this
 * should resolve near-instantly — the timeout exists only so a network hiccup on the follow-up read
 * cannot hang the receipt forever. A timeout surfaces as `undefined`, never a fabricated value. */
async function fetchAppendedEventBounded(
  client: SalvorClient,
  runId: string,
  fromSeq: number,
): Promise<SalvorEvent | undefined> {
  return Promise.race([
    fetchAppendedEvent(client, runId, fromSeq),
    new Promise<undefined>((resolve) => setTimeout(() => resolve(undefined), APPENDED_EVENT_TIMEOUT_MS)),
  ]);
}

/** Build the receipt for a resume/resolve commit: fetch the appended event, then re-derive status
 * from a fresh `GET /v1/runs/{id}` (independent of whatever the write response reported). */
export async function buildReceipt(
  client: SalvorClient,
  runId: string,
  beforeEventCount: number,
  expectedKind: string,
  fallbackPayload: unknown,
  endpoint: string,
): Promise<ReceiptVM> {
  const [event, state] = await Promise.all([
    fetchAppendedEventBounded(client, runId, beforeEventCount),
    client.getRun(runId) as Promise<RunState>,
  ]);
  return {
    seq: event?.seq,
    kind: event?.kind ?? expectedKind,
    recordedAt: event?.recordedAt,
    payload: event?.payload ?? fallbackPayload,
    statusState: state.status.state,
    endpoint,
    unconfirmed: event === undefined,
  };
}

/** `run-abc12345…` → `abc12345`, the app-wide 8-char id truncation. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}

// ── gate decision evidence ─────────────────────────────────────────────────────────────────────
// A suspension card asks true/false; these read the run's RECENT SUBSTANCE off its own log so the
// decision is made against evidence, not a bare form. Everything here is recorded fact, bounded.

/**
 * Collect a run's recorded log by streaming from `fromSeq`, stopping once `expected` events are in
 * hand or a short budget elapses. A suspended run's event stream stays OPEN after its backfill
 * (it is live), so a bounded collect is the honest one-shot read — the same release discipline
 * {@link fetchAppendedEvent} uses, never a live connection left holding.
 */
export async function collectLog(
  client: SalvorClient,
  runId: string,
  fromSeq: number,
  expected: number,
  budgetMs = 2500,
): Promise<SalvorEvent[]> {
  const stream = client.streamEvents(runId, { fromSeq });
  const iterator = stream[Symbol.asyncIterator]();
  const out: SalvorEvent[] = [];
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<'timeout'>((resolve) => {
    timer = setTimeout(() => resolve('timeout'), budgetMs);
  });
  try {
    while (out.length < expected) {
      const res = await Promise.race([iterator.next(), timeout]);
      if (res === 'timeout') break;
      if (res.done) break;
      out.push(res.value);
    }
  } finally {
    clearTimeout(timer);
    await iterator.return?.().catch(() => undefined);
  }
  return out;
}

/** The last assistant text block the model produced, with the seq it was recorded at — the "what
 * the run last said" excerpt. Reads `response.content[]` (the Anthropic message shape the log
 * records), joining any text blocks; ignores tool-use blocks. Undefined when the tail has none. */
export function lastAssistantText(
  events: readonly SalvorEvent[],
): { readonly text: string; readonly seq: number } | undefined {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.kind !== 'ModelCallCompleted') continue;
    const response = e.payload['response'] as Record<string, unknown> | undefined;
    const content = response?.['content'];
    if (!Array.isArray(content)) continue;
    const text = content
      .filter(
        (b): b is Record<string, unknown> =>
          !!b && typeof b === 'object' && (b as Record<string, unknown>)['type'] === 'text',
      )
      .map((b) => String(b['text'] ?? ''))
      .join('\n')
      .trim();
    if (text) return { text, seq: e.seq };
  }
  return undefined;
}

/** The last tool result recorded, with the tool name (read from its paired ToolCallRequested by
 * `payload.seq`, recorded structure not a guess) and the seq — "what the run last did". */
export function lastToolResult(
  events: readonly SalvorEvent[],
): { readonly tool: string; readonly output: unknown; readonly seq: number } | undefined {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.kind !== 'ToolCallCompleted') continue;
    const pairedSeq = e.payload['seq'];
    const paired = pairedSeq !== undefined ? events.find((x) => x.seq === pairedSeq) : undefined;
    const tool = String(paired?.payload['tool'] ?? e.payload['tool'] ?? 'tool');
    return { tool, output: e.payload['output'], seq: e.seq };
  }
  return undefined;
}

/** One node a resume would continue into. */
export interface NextNode {
  readonly id: string;
  readonly name: string;
  readonly kind: string;
}

/** The nodes an edge leaves `currentNode` for, named — "resuming continues into: Follow up (agent)".
 * Read straight off the stored graph document's own nodes and edges, so it is recorded topology,
 * never a guess. Empty when the gate is terminal (nothing downstream). */
export function nextNodesAfter(
  document: { nodes?: unknown; edges?: unknown } | undefined,
  currentNode: string,
): NextNode[] {
  const nodes = (document?.nodes as Array<Record<string, unknown>>) ?? [];
  const edges = (document?.edges as Array<Record<string, unknown>>) ?? [];
  const byId = new Map<string, { name: string; kind: string }>();
  for (const n of nodes) {
    const payload = (n['payload'] as Record<string, unknown>) ?? {};
    const id = String(payload['id'] ?? '');
    byId.set(id, { name: String(payload['name'] ?? id), kind: String(n['kind'] ?? '') });
  }
  const seen = new Set<string>();
  const out: NextNode[] = [];
  for (const e of edges) {
    if (String(e['from'] ?? '') !== currentNode) continue;
    const to = String(e['to'] ?? '');
    if (!to || seen.has(to)) continue;
    seen.add(to);
    const meta = byId.get(to) ?? { name: to, kind: '' };
    out.push({ id: to, name: meta.name, kind: meta.kind });
  }
  return out;
}
