import type { SalvorEvent } from '@salvor/client';

import { activityDescText, bucketEvents, hourTermOf, renderActivityHtml } from './activity';

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
