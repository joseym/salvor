import type { SalvorEvent } from '@salvor/client';

import { esc, pretty } from '../../shared/json-hi';
import { callCost, int, usd } from './pricing';
import { costOfPrefix } from './state-model';

/**
 * The timeline renderer (`rowOf`/`groupsOf`/`renderTimeline`/`renderStrip`). Pure functions that
 * take the run's decoded log and return HTML strings — the Inspector component injects them and
 * wires event delegation. Kept as string-building rather than Angular template so the DOM stays
 * byte-faithful (the `.levent` grid, the kchip dots, the effect badges, the highlighted `<pre>`
 * panes) instead of drifting through template diffing.
 */

/** kind → { dot: kchip dot class, cat: filter category }. Node-scoped kinds included so a graph
 *  run's log (one log, node-scoped envelopes) renders without reading `.cat` off undefined. */
export const KINDS: Readonly<Record<string, { dot: string; cat: string }>> = {
  NodeEntered: { dot: 'k-life', cat: 'lifecycle' },
  NodeExited: { dot: 'k-life', cat: 'lifecycle' },
  BranchTaken: { dot: 'k-blue', cat: 'context' },
  MapIteration: { dot: 'k-green', cat: 'tool' },
  ForkedFrom: { dot: 'k-blue', cat: 'lifecycle' },
  ForkAcknowledged: { dot: 'k-amber', cat: 'lifecycle' },
  RunStarted: { dot: 'k-life', cat: 'lifecycle' },
  RunCompleted: { dot: 'k-life', cat: 'lifecycle' },
  RunFailed: { dot: 'k-red', cat: 'lifecycle' },
  BudgetExceeded: { dot: 'k-amber', cat: 'lifecycle' },
  Suspended: { dot: 'k-amber', cat: 'lifecycle' },
  Resumed: { dot: 'k-life', cat: 'lifecycle' },
  ModelCallRequested: { dot: 'k-model', cat: 'model' },
  ModelCallCompleted: { dot: 'k-model', cat: 'model' },
  ToolCallRequested: { dot: 'k-tool', cat: 'tool' },
  ToolCallCompleted: { dot: 'k-tool', cat: 'tool' },
  NowObserved: { dot: 'k-observe', cat: 'context' },
  RandomObserved: { dot: 'k-observe', cat: 'context' },
};

function kindMeta(kind: string): { dot: string; cat: string } {
  return KINDS[kind] ?? { dot: 'k-observe', cat: 'context' };
}

/** The dot class for an event — a write tool call gets the hollow amber ring. */
export function dotOf(e: SalvorEvent): string {
  return e.kind === 'ToolCallRequested' && e.payload['effect'] === 'write'
    ? 'k-write'
    : kindMeta(e.kind).dot;
}

/** UTC HH:MM:SSZ — every time in this app is evidence (the `recorded_at` the log carries), pinned
 *  to UTC and stamped Z so two operators never quote different times for the same event. */
const UTC_HMS = new Intl.DateTimeFormat('en-GB', {
  timeZone: 'UTC',
  hour12: false,
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
});
export function clock(iso: string): string {
  return UTC_HMS.format(new Date(iso)) + 'Z';
}

function pane(label: string, v: unknown): string {
  return `<div class="pane"><span class="pane-label">${label}</span><pre>${pretty(v)}</pre></div>`;
}

const CARET = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5"/></svg>`;
const FOLD = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><path d="M2.5 3v10"/><path d="M5 8h9M11 4.5 14.5 8 11 11.5"/></svg>`;

/** The running cost label: the folded dollar total, or "tokens only" when it's incomplete. */
function costLabel(c: { complete: boolean; usd: number | null }): string {
  return c.complete ? usd(c.usd ?? 0) : 'tokens only';
}

/**
 * A fork's inherited prefix marker, if any — the first `ForkedFrom` in the log. Real runs from
 * this build's server carry none (there is no fork runtime yet), so this returns null and nothing
 * renders as inherited; the code path exists for when v0.4 writes real `ForkedFrom` markers.
 */
export function forkOf(events: readonly SalvorEvent[]): SalvorEvent | null {
  return events.find((e) => e.kind === 'ForkedFrom') ?? null;
}

/**
 * The scrub zone a timeline row falls into, given the fold prefix length `n` (the scrubber's
 * `prefix`, i.e. `derive_state(&log[0..n])`): `folded` (already inside the derived state),
 * `boundary` (seq n−1 — the event under the playhead, the LAST folded one), or `beyond` (seq ≥
 * n — in the log but not yet folded). Pure, so `renderScrubber`'s DOM class toggle and its unit
 * tests share one source of truth for the boundary math. This replaces the old bug of accenting
 * seq === n (the first EXCLUDED event) instead of the one actually under the playhead.
 */
export type ScrubZone = 'folded' | 'boundary' | 'beyond';
export function zoneOf(seq: number, n: number): ScrubZone {
  if (seq >= n) return 'beyond';
  if (n > 0 && seq === n - 1) return 'boundary';
  return 'folded';
}

/** One log row's full `.levent` HTML, over a decoded {@link SalvorEvent}. */
export function rowOf(e: SalvorEvent, events: readonly SalvorEvent[], arrivedSeq: number | null): string {
  const p = e.payload;
  const kind = e.kind;
  const id = `ev-${e.seq}`;
  let detail = '';
  let body = '';
  let cls = '';

  switch (kind) {
    case 'RunStarted': {
      const a = String(p['agent_def_hash'] ?? '');
      detail = `agent ${esc(a)}${p['record_prompts'] ? ' · prompts recorded' : ''}`;
      body = pane('input', p['input']);
      break;
    }
    case 'NowObserved':
      detail = `now = <b>${esc(String(p['now']))}</b>`;
      body = pane('payload', p);
      break;
    case 'RandomObserved': {
      const bits = String(p['bits']);
      let derivedFloat = '';
      try {
        derivedFloat = (Number(BigInt(bits) >> 11n) / 2 ** 53).toFixed(7);
      } catch {
        derivedFloat = '—';
      }
      detail = `bits = <b>${esc(bits)}</b> <span style="color:var(--faint)">· u64</span>`;
      body =
        pane('payload', p) +
        `<p class="ev-honest">The recorded value is the raw u64. Any float a caller uses
           (here <span class="mono">${derivedFloat}</span>, derived as
           <span class="mono">bits &gt;&gt; 11 / 2^53</span>) is computed caller-side and never enters the log —
           which is why replay reproduces it exactly.</p>`;
      break;
    }
    case 'ModelCallRequested':
      detail = `<b>${esc(String(p['model']))}</b> · ${esc(String(p['request_hash'] ?? ''))}`;
      body = p['request_body']
        ? pane('request body (recorded)', p['request_body'])
        : `<p class="ev-honest">Prompt not recorded (recording is opt-in). The intent carries
             <span class="mono">request_hash</span> only; this run did not set <span class="mono">record_prompts</span>.</p>`;
      break;
    case 'ModelCallCompleted': {
      const model = String(p['model']);
      const usage = (p['usage'] ?? {}) as Record<string, unknown>;
      const inTok = Number(usage['input_tokens'] ?? 0);
      const outTok = Number(usage['output_tokens'] ?? 0);
      const upto = costOfPrefix(events, e.seq + 1);
      const c = callCost(model, inTok, outTok);
      const call = c !== null ? usd(c) : `<span class="tokens-only">unpriced model</span>`;
      detail = `<b class="fig">${int(inTok)}</b> in · <b class="fig">${int(outTok)}</b> out ·
                ${call} · run total <b>${costLabel(upto)}</b>`;
      body = pane('response + usage', p);
      if (c === null)
        body += `<p class="ev-honest"><span class="mono">${esc(model)}</span> is not in the price table,
        so every dollar figure for this run degrades to tokens-only rather than omitting this call's spend.</p>`;
      break;
    }
    case 'ToolCallRequested': {
      const effect = String(p['effect']);
      const key = p['idempotency_key']
        ? ` · <span class="mono">${esc(String(p['idempotency_key']))}</span>`
        : effect === 'write'
          ? ` · <span class="mono tokens-only">idempotency_key: null</span>`
          : '';
      detail = `<b>${esc(String(p['tool']))}</b> <span class="badge e-${effect}">${esc(effect)}</span>${key}`;
      body = pane('input', p['input']);
      if (effect === 'write') cls = 'attn';
      break;
    }
    case 'ToolCallCompleted': {
      const output = p['output'] as Record<string, unknown> | undefined;
      detail =
        output && output['ok'] === false
          ? `<b>${esc(String(p['tool']))}</b> returned a failure — recorded, not thrown`
          : `<b>${esc(String(p['tool']))}</b> returned`;
      body = pane('output', p['output']);
      break;
    }
    case 'Suspended':
      detail = 'awaiting human input';
      cls = 'attn';
      body = `<p class="ev-note">${esc(String(p['reason']))}</p>${pane('input_schema', p['input_schema'])}`;
      break;
    case 'Resumed':
      detail = 'resumed with recorded input';
      body = `<p class="ev-honest">The event carries the input only — no actor. There is no user model
                (single tenant), so there is no actor to record.</p>${pane('input', p['input'])}`;
      break;
    case 'BudgetExceeded':
      detail = `<b>${esc(String(p['limit']))}</b> · limit ${usd(Number(p['limit_usd']))} · observed <b>${usd(Number(p['observed_usd']))}</b>`;
      cls = 'attn';
      body = `<p class="ev-note">This is the only event that puts a declared ceiling in the log.
                Every other run's ceiling lives in the agent definition.</p>${pane('payload', p)}`;
      break;
    case 'RunCompleted':
      detail = `terminal · <b>completed</b>`;
      body = pane('output', p['output']);
      break;
    case 'RunFailed': {
      const error = (p['error'] ?? {}) as Record<string, unknown>;
      detail = `terminal · <b>failed</b> · ${esc(String(error['kind'] ?? ''))}`;
      cls = 'fail';
      body = `<p class="ev-note">${esc(String(error['message'] ?? ''))}</p>${pane('error', p['error'])}`;
      break;
    }
    default:
      detail = esc(kind);
      body = pane('payload', p);
  }

  const isNew = arrivedSeq !== null && arrivedSeq === e.seq;
  const fk = forkOf(events);
  const inh = fk !== null && e.seq < fk.seq;
  const node = (e.payload['node'] as string | undefined) ?? '';
  return `<div class="levent ${cls}${isNew ? ' arrive' : ''}${inh ? ' inherited' : ''}" data-seq="${e.seq}"
    data-cat="${kindMeta(kind).cat}" data-node="${esc(node)}"${inh ? ' data-inherited="1"' : ''}>
    <div class="lrow lgrid">
      <div class="bar"></div>
      <div class="seq">${e.seq}</div>
      <div><span class="kchip ${dotOf(e)}">${esc(kind)}</span></div>
      <div class="ldetail">${detail}${
        inh && fk
          ? `<span class="inh-tag" title="Copied from run ${esc(String(fk.payload['origin_run_id'] ?? '').slice(0, 8))} and replayed — this call did not run a second time.">inherited</span>`
          : ''
      }</div>
      <div class="ltime">${clock(e.recordedAt)}</div>
      <div class="lacts">
        <button class="iact expand" type="button" aria-expanded="false" aria-controls="${id}"
                title="Show this event's payload">
          <span class="sr">Show payload for seq ${e.seq}</span>${CARET}
        </button>
        <button class="iact fold" type="button" data-scrub="${e.seq}"
                title="Fold the run's state through seq ${e.seq}">
          <span class="sr">Fold state through seq ${e.seq}</span>${FOLD}
        </button>
      </div>
    </div>
    <div class="lbody" id="${id}">${body}</div>
  </div>`;
}

interface Turn {
  title: string;
  of: string;
  events: SalvorEvent[];
  tot: string;
}

/** Group the log by model turn, with each turn's recorded-usage subtotal. A new turn opens at
 *  each `ModelCallRequested`; a terminal/budget event opens "End". */
export function groupsOf(events: readonly SalvorEvent[]): Turn[] {
  const gs: Turn[] = [];
  let cur: Turn = { title: 'Start', of: '', events: [], tot: '' };
  let turn = 0;
  for (const e of events) {
    const k = e.kind;
    if (k === 'ModelCallRequested') {
      if (cur.events.length) gs.push(cur);
      cur = { title: `Turn ${++turn}`, of: String(e.payload['model'] ?? ''), events: [], tot: '' };
    } else if (k === 'RunCompleted' || k === 'RunFailed' || k === 'BudgetExceeded') {
      if (cur.events.length) gs.push(cur);
      cur = { title: 'End', of: '', events: [], tot: '' };
    }
    cur.events.push(e);
  }
  if (cur.events.length) gs.push(cur);

  for (const g of gs) {
    let i = 0;
    let o = 0;
    let c = 0;
    let unpriced = false;
    for (const e of g.events) {
      if (e.kind !== 'ModelCallCompleted') continue;
      const usage = (e.payload['usage'] ?? {}) as Record<string, unknown>;
      const inTok = Number(usage['input_tokens'] ?? 0);
      const outTok = Number(usage['output_tokens'] ?? 0);
      i += inTok;
      o += outTok;
      const cost = callCost(String(e.payload['model']), inTok, outTok);
      if (cost !== null) c += cost;
      else unpriced = true;
    }
    g.tot =
      i || o
        ? `${int(i)} in · ${int(o)} out · ${unpriced ? '<span class="tokens-only">tokens only</span>' : usd(c)}`
        : `${g.events.length} event${g.events.length > 1 ? 's' : ''}`;
  }
  return gs;
}

/** The full timeline HTML — turns with heads and their rows. */
export function renderTimelineHtml(events: readonly SalvorEvent[], arrivedSeq: number | null): string {
  return groupsOf(events)
    .map(
      (g) => `
    <section class="turn">
      <div class="turn-head">
        <h4>${esc(g.title)}</h4>
        ${g.of ? `<span class="of">${esc(g.of)}</span>` : ''}
        <span class="tot">${g.tot}</span>
      </div>
      ${g.events.map((e) => rowOf(e, events, arrivedSeq)).join('')}
    </section>`,
    )
    .join('');
}

/** The event strip's ticks — one button per event, sized/hued by kind. */
export function renderStripHtml(events: readonly SalvorEvent[], arrivedSeq: number | null): string {
  return events
    .map(
      (e) => `
    <button class="etick t-${dotOf(e).slice(2)}${arrivedSeq !== null && arrivedSeq === e.seq ? ' arrive' : ''}"
            type="button" data-scrub="${e.seq}" title="seq ${e.seq} · ${esc(e.kind)}">
      <span class="sr">Fold through seq ${e.seq}: ${esc(e.kind)}</span>
    </button>`,
    )
    .join('');
}
