import type { SalvorEvent } from '@salvor/client';

import { esc } from '../../shared/json-hi';
import { hourKey } from '../runs/run-model';

/**
 * The activity histogram's bucket builder and SVG renderer — pure functions over the events every
 * folded run actually recorded, mirroring `event-model.ts`'s split (compute here, the component
 * injects the markup and wires delegated events).
 *
 * One bar per UTC hour, counting events by their OWN `recorded_at` — never by a run's last touch.
 * A run that worked for three hours spans three bars, which is the only thing a chart shaped like
 * time can honestly mean.
 *
 * DISCLOSE AND EXCLUDE: a `recorded_at` cannot be trusted just because it parses. A production
 * store held events stamped `1970-01-01T00:00:00Z` — a client placeholder the server took on
 * faith before it started stamping `recorded_at` itself (see `client_runs.rs`'s server-clock
 * fix). A single such stray beside real 2026 events stretches the window to 56 years and collapses
 * every real bar onto the right edge; clamping it silently would draw a window that never
 * happened. Instead, an implausible stamp is EXCLUDED from the window/bucket computation and its
 * count is disclosed ({@link ActivityWindow.excluded}) — the event is real spend and still counts
 * everywhere else in Spend, only its place on this timeline is unknowable.
 */

const HOUR_MS = 3600_000;

/** Salvor cannot have recorded a real event before it existed. Any `recorded_at` older than this
 * is not a real timestamp — in practice, the epoch-zero client placeholder above — so it is
 * excluded from the window rather than trusted to place a bar. */
const PLAUSIBLE_FLOOR_MS = Date.parse('2020-01-01T00:00:00Z');

export interface HourBucket {
  model: number;
  tool: number;
  other: number;
  park: boolean;
  fail: boolean;
}

export interface ActivityWindow {
  readonly lo: number;
  readonly hi: number;
  readonly nBuckets: number;
  readonly buckets: readonly HourBucket[];
  /** Events whose `recorded_at` fell before {@link PLAUSIBLE_FLOOR_MS} — excluded from `lo`/`hi`/
   * `buckets` above, counted here rather than dropped silently. Zero when every stamp was
   * plausible. `nBuckets` is 0 (an honest empty window, not a broken axis) when NO stamp was
   * plausible; `excluded` alone still reports how many events that was. */
  readonly excluded: number;
}

/** Bucket every event from every folded run's log by the UTC hour its own `recorded_at` falls in.
 * Events with an implausible `recorded_at` (see {@link PLAUSIBLE_FLOOR_MS}) never enter the
 * window's extent or its buckets — only plausible stamps place a bar — but they are counted in
 * the returned window's `excluded` so nothing is dropped without being said. */
export function bucketEvents(allEvents: readonly (readonly SalvorEvent[])[]): ActivityWindow | undefined {
  const stamps: { t: number; kind: string }[] = [];
  for (const events of allEvents) {
    for (const e of events) {
      const t = Date.parse(e.recordedAt);
      if (!Number.isNaN(t)) stamps.push({ t, kind: e.kind });
    }
  }
  if (stamps.length === 0) return undefined;

  let excluded = 0;
  const plausible: { t: number; kind: string }[] = [];
  for (const s of stamps) {
    if (s.t < PLAUSIBLE_FLOOR_MS) excluded++;
    else plausible.push(s);
  }
  if (plausible.length === 0) {
    // every recorded_at was implausible — an honest empty window, not a broken axis
    return { lo: 0, hi: 0, nBuckets: 0, buckets: [], excluded };
  }

  // Iterate for the extent rather than `Math.min(...plausible.map(...))`: a run with a hundred
  // thousand events would spread that many arguments into `Math.min`/`Math.max` and overflow
  // the call stack. A loop is bounded by nothing but the array's own length.
  let minT = Infinity;
  let maxT = -Infinity;
  for (const s of plausible) {
    if (s.t < minT) minT = s.t;
    if (s.t > maxT) maxT = s.t;
  }
  const lo = Math.floor(minT / HOUR_MS) * HOUR_MS;
  const hi = Math.floor(maxT / HOUR_MS) * HOUR_MS;
  const nBuckets = Math.round((hi - lo) / HOUR_MS) + 1;
  const buckets: HourBucket[] = Array.from({ length: nBuckets }, () => ({
    model: 0,
    tool: 0,
    other: 0,
    park: false,
    fail: false,
  }));
  for (const s of plausible) {
    const i = Math.round((Math.floor(s.t / HOUR_MS) * HOUR_MS - lo) / HOUR_MS);
    const b = buckets[i];
    if (!b) continue;
    if (s.kind.startsWith('ModelCall')) b.model++;
    else if (s.kind.startsWith('ToolCall')) b.tool++;
    else b.other++;
    if (s.kind === 'Suspended' || s.kind === 'BudgetExceeded') b.park = true;
    if (s.kind === 'RunFailed') b.fail = true;
  }
  return { lo, hi, nBuckets, buckets, excluded };
}

/** The `hour:` term a click on bucket `i` would apply — same vocabulary Runs' own filter reads. */
export function hourTermOf(lo: number, i: number): string {
  return hourKey(new Date(lo + i * HOUR_MS).toISOString());
}

const W = 460;
const H = 128;
const BASE = 96;
const MAX_H = 78;
const PAD_L = 22;

/**
 * The tallest stacked bar's event count, floored at 1 (the histogram's y-axis top). Iterated on
 * purpose: a window whose events span a very wide time range has one hourly bucket PER HOUR of the
 * range — hundreds of thousands of them for a stray 1970 timestamp beside a 2026 one — and
 * `Math.max(...buckets.map(...))` would spread that whole array into `Math.max`, overflowing the
 * call stack. A loop has no such ceiling and returns the identical number for any window.
 */
function peakTotal(buckets: readonly HourBucket[]): number {
  let max = 1;
  for (const b of buckets) {
    const total = b.model + b.tool + b.other;
    if (total > max) max = total;
  }
  return max;
}

/**
 * Render the histogram's `<svg>` inner markup: a stacked bar per non-empty hour (`.hbucket`, a
 * real `role="button"` when the run list can name any run for it, drawn-but-inert otherwise), the
 * hour ticks, the axis and its end labels. `lastActiveCount(hourTerm)` answers, per hour, how many
 * runs the Runs filter would actually land on — the bar's own count and that count are frequently
 * different numbers, which is exactly what the label discloses.
 */
export function renderActivityHtml(
  win: ActivityWindow,
  lastActiveCount: (hourTerm: string) => number,
  selectedHourTerm: string | undefined,
): string {
  const { lo, nBuckets, buckets } = win;
  if (nBuckets === 0) return ''; // no plausible stamp to place a bar at — an empty chart, not a broken axis
  const max = peakTotal(buckets);
  const step = (W - PAD_L - 6) / nBuckets;
  const bw = Math.max(3, step - 2);

  const bars = buckets
    .map((b, i) => {
      const total = b.model + b.tool + b.other;
      if (!total) return ''; // an empty hour has nothing to show and nothing to filter to
      const x = PAD_L + i * step;
      const h = Math.max(2, (total / max) * MAX_H);
      const hm = (b.model / total) * h;
      const ht = (b.tool / total) * h;
      const ho = (b.other / total) * h;
      const mark = b.fail
        ? `<text class="h-fail" x="${x + bw / 2}" y="${BASE - h - 5}" text-anchor="middle" font-size="11">✕</text>`
        : b.park
          ? `<text class="h-park" x="${x + bw / 2}" y="${BASE - h - 5}" text-anchor="middle" font-size="11">▲</text>`
          : '';
      // The hit rect is a PARTITION of the axis, not a padded copy of the visible bar: exactly
      // `step` wide, edge to edge with its neighbors, zero overlap and zero gap. The visible bar
      // (`bw`, above) is clamped to a 3px floor so a thin bucket still paints something, but a
      // click target built the same way overlaps its neighbors once buckets get denser than that
      // floor — a dense real window (hundreds of hourly buckets, step under 3px) had `bw+2`-wide
      // hit rects overlapping by nearly half a bucket, so a center-click could resolve to the
      // WRONG neighbor (`elementFromPoint` picks whichever overlapping rect is later in paint
      // order). Tiling the hit geometry on `step` alone removes the overlap at any density while
      // leaving the bar's own width untouched.
      const hr = hourTermOf(lo, i);
      const n = lastActiveCount(hr);
      const label =
        `${hr.slice(11, 16)}Z — ${total} event${total === 1 ? '' : 's'}` +
        (b.fail ? ', a run failed this hour' : b.park ? ', a run parked this hour' : '') +
        `. ${
          n === 0
            ? 'No run was last active in this hour, so there is nothing to filter to'
            : `${n} run${n === 1 ? '' : 's'} last active in this hour — filter the run list to ${n === 1 ? 'it' : 'them'}`
        }.`;
      const cls = [
        'hbucket',
        n ? 'pick' : '',
        b.fail ? 'bad' : b.park ? 'hot' : '',
        selectedHourTerm && hr.toLowerCase().includes(selectedHourTerm) ? 'sel' : '',
      ]
        .filter(Boolean)
        .join(' ');
      return `<g class="${cls}" ${n ? 'role="button" tabindex="0"' : ''} data-hour="${esc(hr)}" ${n ? `aria-label="${esc(label)}"` : ''}>
        <title>${esc(label)}</title>
        <rect class="hhit" x="${x}" y="0" width="${step}" height="${BASE + 9}"></rect>
        <rect class="h-model" x="${x}" y="${BASE - h}" width="${bw}" height="${hm}"></rect>
        <rect class="h-tool" x="${x}" y="${BASE - h + hm}" width="${bw}" height="${ht}"></rect>
        <rect class="empty-bar" x="${x}" y="${BASE - h + hm + ht}" width="${bw}" height="${ho}"></rect>
        <rect class="hfoot" x="${x + bw / 4}" y="${BASE + 3}" width="${bw / 2}" height="3"></rect>
        ${mark}
      </g>`;
    })
    .join('');

  const ticks = buckets
    .map((_, i) => {
      if (i % 6) return '';
      const d = new Date(lo + i * HOUR_MS);
      return `<text x="${PAD_L + i * step + bw / 2}" y="${BASE + 13}" text-anchor="middle">${String(d.getUTCHours()).padStart(2, '0')}:00</text>`;
    })
    .join('');

  return `
    <text x="0" y="14">${max}</text>
    <text x="0" y="${BASE - 2}">0</text>
    ${bars}${ticks}
    <line x1="${PAD_L}" y1="${BASE}" x2="${W}" y2="${BASE}" stroke="var(--rule-firm)" stroke-width="1"></line>
    <text x="${PAD_L}" y="${BASE + 26}">${new Date(win.lo).toISOString().slice(5, 10)}</text>
    <text x="${W}" y="${BASE + 26}" text-anchor="end">${new Date(win.hi).toISOString().slice(5, 10)}</text>`;
}

/** The disclosure sentence for events {@link bucketEvents} excluded as implausible — undefined
 * (not an empty string) when nothing was excluded, so the view can render no note at all rather
 * than an empty one: zero-vs-absent. When every stamp was implausible (`nBuckets === 0`) the
 * chart itself has nothing to draw, so the sentence says that rather than naming a timeline the
 * excluded events were merely left off of. */
export function activityExclusionNote(win: ActivityWindow | undefined): string | undefined {
  if (!win || win.excluded === 0) return undefined;
  const noun = `${win.excluded} event${win.excluded === 1 ? '' : 's'}`;
  const verb = win.excluded === 1 ? 'carries' : 'carry';
  return win.nBuckets === 0
    ? `${noun} ${verb} no plausible timestamp — there is nothing to chart.`
    : `${noun} ${verb} no plausible timestamp — excluded from the timeline.`;
}

/** The `#activity-desc` `.sr` text: the chart's content, said in words for anyone not reading bars. */
export function activityDescText(win: ActivityWindow, lastActiveCount: (hourTerm: string) => number): string {
  if (win.nBuckets === 0) return activityExclusionNote(win) ?? '';
  const { nBuckets, buckets, lo } = win;
  const max = peakTotal(buckets);
  let pickable = 0;
  for (let i = 0; i < nBuckets; i++) {
    const b = buckets[i];
    if (b.model + b.tool + b.other && lastActiveCount(hourTermOf(lo, i))) pickable++;
  }
  const base =
    `Events per hour across the ${nBuckets}-hour window, stacked by kind. Peak ${max} events in one hour. ` +
    `${pickable} of these hours can filter the run list to the runs last active in them; the rest hold events ` +
    `from runs that have since moved on, which the list response cannot place here.`;
  const note = activityExclusionNote(win);
  return note ? `${base} ${note}` : base;
}
