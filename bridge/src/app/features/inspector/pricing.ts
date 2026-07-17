/**
 * The price table and per-call pricing.
 *
 * In the Inspector we HAVE the run's log — every `ModelCallCompleted` carries its model id and
 * token usage — so cost is REAL here (unlike the Runs list, where the model ids are not returned
 * and cost degrades to an em-dash). The table is a maintained snapshot, not a live feed: a model
 * absent from it does not contribute a zero, it makes the whole figure INCOMPLETE, and the UI
 * degrades to tokens-only. That honesty is the point of the {@link CostTotal.complete} flag.
 */

/** USD per 1M tokens, keyed by model id. */
export const PRICES: Readonly<Record<string, { readonly in: number; readonly out: number }>> = {
  'claude-sonnet-4-5': { in: 3.0, out: 15.0 },
  'claude-haiku-4-5': { in: 1.0, out: 5.0 },
};

export function priceOf(model: string): { in: number; out: number } | null {
  return PRICES[model] ?? null;
}

/**
 * Money, formatted by the platform rather than by string concatenation. Two-tier precision: a
 * per-call cost is fractions of a cent, so under a dollar it needs four places or it reads as
 * $0.00; at a dollar or more, cents are what anyone reconciles against. Magnitude, not
 * sign, picks the tier. Two cached formatters — constructing an `Intl.NumberFormat` per cell is
 * the expensive part, and these render in tables.
 */
const USD_4 = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 4,
  maximumFractionDigits: 4,
});
const USD_2 = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

export function usd(n: number): string {
  return (Math.abs(n) < 1 ? USD_4 : USD_2).format(n);
}

/** `1234` → `1,234` — grouped integers, the platform's way. */
export function int(n: number): string {
  return n.toLocaleString('en-US');
}

/** The cost of a single completed model call, in dollars, or `null` if its model is unpriced. */
export function callCost(model: string, inputTokens: number, outputTokens: number): number | null {
  const price = priceOf(model);
  if (!price) return null;
  return (inputTokens * price.in + outputTokens * price.out) / 1e6;
}

/** A folded cost total, honest about unpriced models. */
export interface CostTotal {
  /** True only when EVERY completed call in the prefix was priced. */
  readonly complete: boolean;
  /** The dollar total when complete; `null` when at least one model was unpriced. */
  readonly usd: number | null;
  /** The distinct unpriced model ids that made the figure incomplete. */
  readonly unpriced: readonly string[];
}

/** One completed model call, the only shape {@link costOf} needs from a log event. */
export interface CompletedCall {
  readonly model: string;
  readonly inputTokens: number;
  readonly outputTokens: number;
}

/**
 * Fold a sequence of completed model calls into a {@link CostTotal}. A model absent from the price
 * table does NOT add zero — it flips `complete` to false and is recorded in `unpriced`, so the
 * figure is reported as incomplete rather than silently wrong.
 */
export function costOf(calls: readonly CompletedCall[]): CostTotal {
  let usdTotal = 0;
  const unpriced = new Set<string>();
  for (const c of calls) {
    const price = priceOf(c.model);
    if (price) usdTotal += (c.inputTokens * price.in + c.outputTokens * price.out) / 1e6;
    else unpriced.add(c.model);
  }
  return unpriced.size
    ? { complete: false, usd: null, unpriced: [...unpriced] }
    : { complete: true, usd: usdTotal, unpriced: [] };
}
