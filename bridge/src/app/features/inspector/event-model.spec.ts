import { ESTRIP_GAP_BUDGET_PCT, estripGapPct, estripTickBox, zoneOf } from './event-model';

describe('zoneOf — the scrub zone a timeline row falls into', () => {
  it('marks every seq at or past the prefix as beyond', () => {
    expect(zoneOf(5, 5)).toBe('beyond');
    expect(zoneOf(6, 5)).toBe('beyond');
    expect(zoneOf(0, 0)).toBe('beyond'); // nothing folded yet — seq 0 has not happened
  });

  it('marks seq n-1 — the event under the playhead — as boundary, never seq n', () => {
    // the bug this replaces: the old treatment accented seq === n (the first EXCLUDED event)
    expect(zoneOf(4, 5)).toBe('boundary');
    expect(zoneOf(5, 5)).not.toBe('boundary');
  });

  it('there is no boundary at n = 0 — nothing is folded, so nothing is under the playhead', () => {
    expect(zoneOf(0, 0)).not.toBe('boundary');
    expect(zoneOf(-1, 0)).not.toBe('boundary');
  });

  it('marks everything strictly before the boundary as folded', () => {
    expect(zoneOf(0, 5)).toBe('folded');
    expect(zoneOf(3, 5)).toBe('folded');
  });

  it('a single-event prefix (n=1) makes seq 0 the boundary, not folded or beyond', () => {
    expect(zoneOf(0, 1)).toBe('boundary');
  });

  it('classifies a whole 26-event log consistently, one boundary, folded before it, beyond after', () => {
    const n = 12;
    const zones = Array.from({ length: 26 }, (_, seq) => zoneOf(seq, n));
    expect(zones.filter((z) => z === 'boundary')).toHaveLength(1);
    expect(zones[n - 1]).toBe('boundary');
    expect(zones.slice(0, n - 1).every((z) => z === 'folded')).toBe(true);
    expect(zones.slice(n).every((z) => z === 'beyond')).toBe(true);
  });
});

describe('the event strip tick geometry never lays a tick outside its container', () => {
  // The real defect: on a 206-event run the old CSS gave every `.etick` a hard `min-width: 3px`
  // floor plus a fixed `2px` gap, with no ceiling on how many ticks could exist — 206×3 + 205×2 ≈
  // 1028px demanded against a strip only ~316px wide (the Scrubber's fixed 340px side column,
  // minus its padding), so most ticks rendered off the right edge of the container and of the
  // viewport (a mid-tick measured at x=1444 on a 1280px viewport). `estripTickBox` models the
  // fix's flex arithmetic (equal `flex: 1 1 0%` ticks, no min-width floor, a density-scaled gap)
  // as pure numbers, swept across every event count 1..500 the way f63a7f0 swept the Spend
  // histogram's `.hhit` partition across nBuckets 1..400.
  const EPS = 1e-9;

  it('every tick box sits fully inside [0, 100] percent of the strip, for n = 1..500', () => {
    for (let n = 1; n <= 500; n++) {
      for (let i = 0; i < n; i++) {
        const { leftPct, widthPct } = estripTickBox(i, n);
        expect(widthPct, `n=${n} tick ${i}: width must not be negative`).toBeGreaterThanOrEqual(-EPS);
        expect(leftPct, `n=${n} tick ${i}: left edge must not be negative`).toBeGreaterThanOrEqual(-EPS);
        expect(
          leftPct + widthPct,
          `n=${n} tick ${i}: right edge must not exceed the strip's own width`,
        ).toBeLessThanOrEqual(100 + EPS);
      }
    }
  });

  it('adjacent ticks never overlap, for n = 1..500', () => {
    for (let n = 1; n <= 500; n++) {
      for (let i = 0; i < n - 1; i++) {
        const cur = estripTickBox(i, n);
        const next = estripTickBox(i + 1, n);
        expect(
          cur.leftPct + cur.widthPct,
          `n=${n} tick ${i}: must not overlap tick ${i + 1}`,
        ).toBeLessThanOrEqual(next.leftPct + EPS);
      }
    }
  });

  it('the gap budget never exceeds ESTRIP_GAP_BUDGET_PCT of the axis, for n = 1..500', () => {
    for (let n = 1; n <= 500; n++) {
      const totalGap = estripGapPct(n) * Math.max(0, n - 1);
      expect(totalGap, `n=${n}: total gap must stay within budget`).toBeLessThanOrEqual(
        ESTRIP_GAP_BUDGET_PCT + EPS,
      );
    }
  });

  it('a sparse strip (10 ticks) keeps a comfortably wide hit target, well past the app’s usual 3px/~340px-strip floor', () => {
    const { widthPct } = estripTickBox(0, 10);
    // ~340px is the Scrubber panel's own documented width (inspector.css .inspector grid column);
    // this asserts the PERCENT is generous, not a literal pixel measurement (no DOM in this test).
    expect(widthPct).toBeGreaterThan(9); // ~30px+ at a typical strip width — plenty for a mouse
  });

  it('a dense strip (206 ticks, the proven defect count) compresses but still tiles exactly, never overflowing', () => {
    const n = 206;
    let sumWidth = 0;
    for (let i = 0; i < n; i++) {
      const { leftPct, widthPct } = estripTickBox(i, n);
      expect(leftPct + widthPct).toBeLessThanOrEqual(100 + EPS);
      sumWidth += widthPct;
    }
    const totalGap = estripGapPct(n) * (n - 1);
    expect(sumWidth + totalGap).toBeCloseTo(100, 6); // the axis is fully, exactly accounted for
  });
});
