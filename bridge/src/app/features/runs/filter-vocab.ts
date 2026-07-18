import { type RunRow, agentIdentity, groupOf, hourKey } from './run-model';

/** hash → resolved name, the same map {@link agentIdentity} reads; empty when no registry is in scope. */
export type NameMap = ReadonlyMap<string, string>;
const NO_NAMES: NameMap = new Map();

/**
 * THE FILTER VOCABULARY — the single source of truth for what the Runs filter can answer.
 *
 * Every entry is a key `GET /v1/runs` can actually answer, the operator it takes, whether its values
 * are ENUMERABLE from the page in memory, and whether the `@` menu OFFERS it. `text` is accepted
 * by the parser but not offered — it is the long-hand of a bare word.
 *
 * The parser ({@link parseQuery}), the value autocomplete, the `@` key menu and the field
 * reader ({@link FIELDS}) all derive from THIS list. The bijection between what the menu offers
 * and what the parser accepts is proven by {@link vocabDrift} — the VOCAB_DRIFT guard's job,
 * moved out of a boot-time console probe and into a vitest unit test (see filter-vocab.spec.ts).
 */
export type FilterOp = ':' | '>' | '<';

export interface VocabEntry {
  readonly key: string;
  readonly op: FilterOp;
  readonly enumerable: boolean;
  readonly offered: boolean;
  readonly desc: string;
}

export const FILTER_VOCAB: readonly VocabEntry[] = [
  { key: 'status', op: ':', enumerable: true, offered: true,
    desc: 'the run’s folded state — completed, failed, suspended, and the rest' },
  { key: 'group', op: ':', enumerable: true, offered: true,
    desc: 'the three-way split every dot is keyed to — progress, waiting, terminal' },
  { key: 'events', op: '>', enumerable: false, offered: true,
    desc: 'runs with MORE than N recorded events' },
  { key: 'events', op: '<', enumerable: false, offered: true,
    desc: 'runs with FEWER than N recorded events' },
  { key: 'hour', op: ':', enumerable: true, offered: true,
    desc: 'the UTC hour a run was LAST active — the only time the list response carries' },
  /* agent + build joined the vocabulary once GET /v1/runs started carrying agent_def_hash and
     labels. Non-enumerable, like `events`: they parse and are offered as
     keys, but do not autocomplete a value list — that is a separate build with its own bijection
     to protect, and it is not what this item asked for (see item 15's "Observed, not commissioned"
     note). cost / steps / tokens stay OUT on purpose — the list carries usage/step_count but does
     not offer them as filters, and it does not carry cost at all; the parser still refuses them. */
  { key: 'agent', op: ':', enumerable: false, offered: true,
    desc: 'the agent that ran it — a registered name, a caller label, a hash, or “graph run”' },
  { key: 'build', op: ':', enumerable: false, offered: true,
    desc: 'the build_id a run was tagged with at creation — AARG groups its agents by it' },
  { key: 'text', op: ':', enumerable: false, offered: false,
    desc: 'match the run id — the explicit form of a bare word' },
];

/** parser accepts these keys with `:` */
export const COLON_KEYS: readonly string[] = [
  ...new Set(FILTER_VOCAB.filter((v) => v.op === ':').map((v) => v.key)),
];
/** these keys have autocompletable values present in the data */
export const VALUE_KEYS: readonly string[] = FILTER_VOCAB.filter((v) => v.enumerable).map((v) => v.key);
/** the `@` menu surfaces these */
export const MENU_KEYS: readonly VocabEntry[] = FILTER_VOCAB.filter((v) => v.offered);

const COLON_RE = new RegExp(`^(${COLON_KEYS.join('|')}):(.+)$`, 'i');
export const QUERYABLE =
  MENU_KEYS.map((v) => v.key + v.op).join(' · ') + ' · or a bare word to match the run id';

/** A parsed filter term. `op:` compares by substring; `>`/`<` compare a number. */
export interface Term {
  readonly f: string;
  readonly op: FilterOp;
  readonly v: string | number;
}

/**
 * Parse a query string into structured terms. Refuses, loudly and honestly, the fields the list
 * response does not offer as filters (`cost`, `steps`, `tokens` — `agent` and `build` joined the
 * vocabulary in item 15, see {@link FILTER_VOCAB}) and any other unparseable `key<op>value` shape,
 * so an invalid pill still renders and still explains itself rather than silently matching
 * nothing.
 */
export function parseQuery(s: string): Term[] {
  return s
    .trim()
    .split(/\s+/)
    .filter(Boolean)
    .map((raw): Term => {
      let m: RegExpMatchArray | null;
      if ((m = raw.match(COLON_RE))) {
        return { f: m[1].toLowerCase(), op: ':', v: m[2].toLowerCase() };
      }
      if ((m = raw.match(/^(events)([<>])(\d+)$/i))) {
        return { f: m[1].toLowerCase(), op: m[2] as FilterOp, v: parseInt(m[3], 10) };
      }
      /* STALE ERROR, FIXED (item 15): this used to also refuse "agent" with "GET /v1/runs carries
         no agent" — false since e3182c5 enriched the list with agent_def_hash, and item 15 offers
         it as a filter. The refusal now names only what is genuinely still unqueryable: the list
         carries usage/step_count but does not offer them as filters, and it never carries cost. */
      if (/^(cost|steps|tokens)\s*[:<>]/i.test(raw)) {
        const key = raw.split(/[:<>]/)[0];
        throw new Error(
          `“${key}” is not an offered filter key. The list carries usage and step_count now, but ` +
            `does not offer them as filters, and it does not carry cost at all. Queryable: ${QUERYABLE}.`,
        );
      }
      if (/[:<>]/.test(raw)) {
        throw new Error(`Can’t parse “${raw}”. Queryable: ${QUERYABLE}.`);
      }
      return { f: 'text', op: ':', v: raw.toLowerCase() };
    });
}

/**
 * The field readers — the SAME map the matcher and the value autocomplete read, so a third
 * enumerable key added here cannot silently list another key's values. `group` classifies
 * `status`; `hour` truncates `last`; both are pure reshapings of a field the response carries.
 *
 * Every reader takes the resolved agent-name map as a second argument (defaulting to empty), even
 * though only `agent` reads it — one shape for the whole map, so a caller can pass `names` once
 * without special-casing which key needs it. `agent` matches the human identity OR the raw hash,
 * so `agent:triage` and `agent:9f2c` both work; `build` matches the `build_id` label (empty string
 * for an untagged run, which no `build:` query hits).
 */
export const FIELDS: Readonly<Record<string, (r: RunRow, names?: NameMap) => string | number>> = {
  status: (r) => r.status,
  group: (r) => groupOf(r.status),
  events: (r) => r.eventCount,
  hour: (r) => hourKey(r.last),
  agent: (r, names = NO_NAMES) => `${agentIdentity(r, names).text} ${r.agentDefHash ?? ''}`,
  build: (r) => r.labels?.['build_id'] ?? '',
  text: (r) => r.id,
};

/** Does a row satisfy every term? `names` is the resolved agent-name map (see `FIELDS.agent`). */
export function matchesAll(r: RunRow, terms: readonly Term[], names: NameMap = NO_NAMES): boolean {
  return terms.every((t) => {
    const read = FIELDS[t.f];
    const v = read ? read(r, names) : '';
    if (t.op === ':') return String(v).toLowerCase().includes(String(t.v));
    if (t.op === '>') return Number(v) > Number(t.v);
    return Number(v) < Number(t.v);
  });
}

/**
 * THE FILTER-VOCABULARY BIJECTION, as a function the unit test drives.
 *
 * Probes the REAL parser (not the vocab-derived key lists, which would agree tautologically):
 *   1. NO PHANTOM OFFERS — every key the `@` menu offers must parse to a structured term.
 *   2. NO UNLISTED ACCEPTANCE — the parser must refuse keys the vocabulary does not list. The
 *      canary list below is probed by name because these are the tempting ones a careless edit
 *      might wrongly wire up; it self-adjusts as the vocabulary grows — `agent` was in this list
 *      when it was still refused, and now skips (via the `FILTER_VOCAB.some` guard) because item
 *      15 listed it, rather than the canary going stale and asserting something no longer true.
 *
 * Returns the list of drift descriptions; an empty list is the bijection holding.
 */
export function vocabDrift(): string[] {
  const drift: string[] = [];
  const parses = (raw: string): boolean => {
    try {
      const t = parseQuery(raw);
      return t.length === 1 && t[0].f !== 'text';
    } catch {
      return false;
    }
  };
  const refuses = (raw: string): boolean => {
    try {
      parseQuery(raw);
      return false;
    } catch {
      return true;
    }
  };

  for (const v of MENU_KEYS) {
    const probe = v.key + v.op + (v.op === ':' ? 'x' : '1');
    if (!parses(probe)) {
      drift.push(`offered "${v.key}${v.op}" does not parse — the @ menu would surface a key the parser refuses`);
    }
  }
  for (const k of ['zznotarealkey', 'agent', 'cost', 'steps', 'tokens']) {
    if (FILTER_VOCAB.some((v) => v.key === k)) continue;
    if (!refuses(k + ':x')) {
      drift.push(`"${k}:" is accepted but absent from FILTER_VOCAB — the parser accepts a key the vocabulary does not list`);
    }
  }
  return drift;
}
