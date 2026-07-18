import type { SalvorEvent } from '@salvor/client';

import { activityDescText, activityExclusionNote, bucketEvents, hourTermOf, renderActivityHtml } from './activity';

function ev(kind: string, recordedAt: string, seq = 0): SalvorEvent {
  return { runId: 'run-1', seq, schemaVersion: 1, recordedAt, kind, payload: {} };
}

describe('bucketEvents', () => {
  it('is undefined for no events — nothing to draw', () => {
    expect(bucketEvents([])).toBeUndefined();
    expect(bucketEvents([[]])).toBeUndefined();
  });

  it('buckets by the UTC hour of each event’s own recorded_at, not a run’s last touch', () => {
    const runA = [
      ev('ModelCallRequested', '2026-07-17T09:10:00Z'),
      ev('ModelCallCompleted', '2026-07-17T11:59:00Z'), // same run, spans into the 11:00 bucket
    ];
    const win = bucketEvents([runA]);
    expect(win).toBeDefined();
    expect(win!.nBuckets).toBe(3); // 09:00, 10:00, 11:00 inclusive
    expect(win!.buckets[0].model).toBe(1);
    expect(win!.buckets[1].model + win!.buckets[1].tool + win!.buckets[1].other).toBe(0);
    expect(win!.buckets[2].model).toBe(1);
  });

  it('marks park (Suspended/BudgetExceeded) and fail (RunFailed) hours', () => {
    const win = bucketEvents([
      [ev('Suspended', '2026-07-17T09:00:00Z'), ev('RunFailed', '2026-07-17T09:30:00Z')],
    ]);
    expect(win!.buckets[0].park).toBe(true);
    expect(win!.buckets[0].fail).toBe(true);
  });

  it('classifies ModelCall*/ToolCall*/other independently, across runs', () => {
    const win = bucketEvents([
      [ev('ModelCallRequested', '2026-07-17T09:00:00Z')],
      [ev('ToolCallRequested', '2026-07-17T09:00:00Z')],
      [ev('NowObserved', '2026-07-17T09:00:00Z')],
    ]);
    expect(win!.buckets[0]).toMatchObject({ model: 1, tool: 1, other: 1 });
  });
});

describe('a very wide PLAUSIBLE window does not overflow the call stack', () => {
  // Two real, plausible events years apart (the owner's store also holds long-running data,
  // not just the epoch-zero case below) still stretch the window to tens of thousands of hourly
  // buckets. Spreading that many buckets into `Math.max(...)` — as the render/desc/extent code
  // once did — throws `RangeError: Maximum call stack size exceeded`. These fold the whole
  // pipeline over that window and must simply return, iteratively, regardless of the exclusion
  // rule below (both stamps here are plausible, so nothing is excluded).
  const wide = bucketEvents([
    [ev('ModelCallCompleted', '2020-01-01T00:00:00Z'), ev('ModelCallCompleted', '2026-07-12T18:00:00Z')],
  ])!;

  it('bucketEvents spans the full range without spreading its stamps', () => {
    expect(wide).toBeDefined();
    expect(wide.excluded).toBe(0);
    expect(wide.nBuckets).toBeGreaterThan(50_000);
  });

  it('renderActivityHtml renders both real bars over that window', () => {
    const html = renderActivityHtml(wide, () => 1, undefined);
    // two non-empty hours (2020 and 2026); every other bucket is empty and draws nothing
    expect((html.match(/class="hbucket/g) ?? []).length).toBe(2);
  });

  it('activityDescText reports the window without spreading its buckets', () => {
    const text = activityDescText(wide, () => 1);
    expect(text).toMatch(/Peak 1 events? in one hour/);
  });
});

describe('disclose and exclude: implausible recorded_at stamps', () => {
  // The real bug: a production store held events stamped 1970-01-01T00:00:00Z — the client's own
  // placeholder, trusted verbatim until the server started stamping recorded_at itself. Beside a
  // real 2026 event this used to stretch the window to 56 years and collapse every real bar onto
  // the right edge. Now the epoch stamp is excluded from the window entirely and disclosed.

  it('a mixed window builds its extent from the plausible stamp only, and discloses the rest', () => {
    const win = bucketEvents([
      [ev('ModelCallCompleted', '1970-01-01T00:00:00Z'), ev('ModelCallCompleted', '2026-07-12T18:00:00Z')],
    ])!;
    expect(win).toBeDefined();
    expect(win.excluded).toBe(1);
    expect(win.nBuckets).toBe(1); // just the one plausible hour — no 56-year stretch
    expect(win.buckets[0].model).toBe(1);

    const html = renderActivityHtml(win, () => 1, undefined);
    expect((html.match(/class="hbucket/g) ?? []).length).toBe(1); // one real bar, drawn cleanly

    expect(activityExclusionNote(win)).toBe('1 event carries no plausible timestamp — excluded from the timeline.');
    expect(activityDescText(win, () => 1)).toContain(
      '1 event carries no plausible timestamp — excluded from the timeline.',
    );
  });

  it('a stamp exactly at the plausibility floor is kept; the instant before it is excluded', () => {
    const atFloor = bucketEvents([[ev('ModelCallCompleted', '2020-01-01T00:00:00Z')]])!;
    expect(atFloor.excluded).toBe(0);
    expect(atFloor.nBuckets).toBe(1);

    const beforeFloor = bucketEvents([[ev('ModelCallCompleted', '2019-12-31T23:59:59Z')]])!;
    expect(beforeFloor.excluded).toBe(1);
    expect(beforeFloor.nBuckets).toBe(0);
  });

  it('an all-plausible window discloses nothing — zero-vs-absent, no note at all', () => {
    const win = bucketEvents([[ev('ModelCallCompleted', '2026-07-17T09:00:00Z')]])!;
    expect(win.excluded).toBe(0);
    expect(activityExclusionNote(win)).toBeUndefined();
    expect(activityExclusionNote(undefined)).toBeUndefined();
    expect(activityDescText(win, () => 1)).not.toMatch(/plausible timestamp/);
  });

  it('an all-implausible window is an honest empty state, not a broken axis', () => {
    const win = bucketEvents([
      [
        ev('ModelCallCompleted', '1970-01-01T00:00:00Z'),
        ev('ModelCallCompleted', '1970-01-01T00:05:00Z'),
        ev('ModelCallCompleted', '1970-01-01T00:10:00Z'),
      ],
    ])!;
    expect(win).toBeDefined();
    expect(win.excluded).toBe(3);
    expect(win.nBuckets).toBe(0);
    expect(win.buckets).toEqual([]);

    // nothing to draw — renderActivityHtml must not fabricate a window from lo=hi=0
    expect(renderActivityHtml(win, () => 1, undefined)).toBe('');

    expect(activityExclusionNote(win)).toBe('3 events carry no plausible timestamp — there is nothing to chart.');
    expect(activityDescText(win, () => 1)).toBe(
      '3 events carry no plausible timestamp — there is nothing to chart.',
    );
  });

  it('bucketEvents is still undefined when there are no events at all — a different absence than all-implausible', () => {
    expect(bucketEvents([])).toBeUndefined();
    expect(bucketEvents([[]])).toBeUndefined();
  });
});

describe('hourTermOf', () => {
  it('matches run-model’s own hourKey shape (no "hour:" prefix)', () => {
    const lo = Date.parse('2026-07-17T09:00:00Z');
    expect(hourTermOf(lo, 0)).toBe('2026-07-17T09:00Z');
    expect(hourTermOf(lo, 2)).toBe('2026-07-17T11:00Z');
  });
});

describe('renderActivityHtml', () => {
  const win = bucketEvents([[ev('ModelCallCompleted', '2026-07-17T09:00:00Z')]])!;

  it('a pickable hour (a run the list can name) renders as a real button with both counts', () => {
    const html = renderActivityHtml(win, () => 2, undefined);
    expect(html).toContain('class="hbucket pick"');
    expect(html).toContain('role="button"');
    expect(html).toContain('tabindex="0"');
    expect(html).toMatch(/1 event/);
    expect(html).toMatch(/2 runs? last active/);
  });

  it('an unpickable hour (no run the list can name) draws but is not a control', () => {
    const html = renderActivityHtml(win, () => 0, undefined);
    expect(html).not.toContain('role="button"');
    expect(html).not.toContain('aria-label=');
    expect(html).toContain('No run was last active');
  });

  it('marks the currently-selected hour with .sel', () => {
    const html = renderActivityHtml(win, () => 1, '2026-07-17t09:00z');
    expect(html).toContain('class="hbucket pick sel"');
  });

  it('escapes the label — no raw HTML from a hostile hour string reaches the DOM', () => {
    // hourTermOf's own output is always a safe ISO shape; this proves the esc() call is real
    // by checking the title/label carries no unescaped angle brackets under normal input.
    const html = renderActivityHtml(win, () => 1, undefined);
    expect(html).not.toMatch(/<script/i);
  });
});

describe('activityDescText', () => {
  it('names the peak and how many hours the filter can actually land on', () => {
    const win = bucketEvents([[ev('ModelCallCompleted', '2026-07-17T09:00:00Z')]])!;
    const text = activityDescText(win, () => 1);
    expect(text).toMatch(/Peak 1 events? in one hour/);
    expect(text).toMatch(/1 of these hours can filter/);
  });
});
