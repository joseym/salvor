import { type CompletedCall, callCost, costOf, int, priceOf, usd } from './pricing';

describe('priceOf / callCost', () => {
  it('prices a known model per 1M tokens', () => {
    // sonnet: 1842 in * $3 + 96 out * $15, per 1M
    expect(callCost('claude-sonnet-4-5', 1842, 96)).toBeCloseTo((1842 * 3 + 96 * 15) / 1e6, 10);
    expect(priceOf('claude-haiku-4-5')).toEqual({ in: 1.0, out: 5.0 });
  });

  it('returns null for a model absent from the table', () => {
    expect(priceOf('some-unlisted-model')).toBeNull();
    expect(callCost('some-unlisted-model', 100, 100)).toBeNull();
  });
});

describe('usd formatting', () => {
  it('uses four places under a dollar and two at or above, grouped', () => {
    expect(usd(0.9368)).toBe('$0.9368');
    expect(usd(1234.5)).toBe('$1,234.50');
  });
  it('picks the tier by magnitude, not sign', () => {
    expect(usd(-5)).toBe('-$5.00');
  });
  it('int groups thousands', () => {
    expect(int(3061)).toBe('3,061');
  });
});

describe('costOf — honest about unpriced models', () => {
  it('sums a fully-priced prefix and reports complete', () => {
    const calls: CompletedCall[] = [
      { model: 'claude-sonnet-4-5', inputTokens: 1842, outputTokens: 96 },
      { model: 'claude-sonnet-4-5', inputTokens: 2410, outputTokens: 173 },
    ];
    const total = costOf(calls);
    expect(total.complete).toBe(true);
    expect(total.unpriced).toEqual([]);
    expect(total.usd).toBeCloseTo(
      (1842 * 3 + 96 * 15 + 2410 * 3 + 173 * 15) / 1e6,
      10,
    );
  });

  it('does NOT add a zero for an unpriced model — the whole figure goes incomplete', () => {
    const calls: CompletedCall[] = [
      { model: 'claude-sonnet-4-5', inputTokens: 1000, outputTokens: 100 },
      { model: 'mystery-model', inputTokens: 5000, outputTokens: 900 },
    ];
    const total = costOf(calls);
    expect(total.complete).toBe(false);
    expect(total.usd).toBeNull();
    expect(total.unpriced).toEqual(['mystery-model']);
  });

  it('an empty prefix is complete at zero', () => {
    expect(costOf([])).toEqual({ complete: true, usd: 0, unpriced: [] });
  });
});
