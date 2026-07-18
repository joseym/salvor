import type { WfEdge, WfGraph, WfNode } from './wf-model';

/**
 * THE CLIENT-SIDE VALIDATOR. Publish is gated on this list, the canvas marks the same offenders
 * the panel names, and the panel head IS the verdict — so there is exactly ONE error list per
 * render, derived from the graph, never cached. Each error class is a different kind of failure:
 * a duplicate id (the join key every edge references stops being unique), a dangling edge (a
 * reference to nothing), a malformed agent hash, a zero-worker fan-out, an unrealized branch
 * case, a case label on a non-branch edge, and a cycle (reported WITH its path — "there is a
 * cycle" is a rumour, not an error message).
 *
 * Fixes are DATA, not closures: a {@link WfFix} names what to change and {@link applyFix} applies
 * it to a graph immutably, so the panel's one-click offers and their unit tests share one
 * implementation.
 */
export interface WfFix {
  readonly label: string;
  readonly kind: 'rename_dupe' | 'set_concurrency' | 'repoint' | 'drop_label' | 'drop_edge';
  readonly id?: string;
  readonly edge?: number;
  readonly end?: 'from' | 'to';
  readonly to?: string;
}

export interface WfError {
  readonly code:
    | 'duplicate_id'
    | 'bad_agent_hash'
    | 'bad_concurrency'
    | 'bad_effect'
    | 'unrealized_case'
    | 'dangling_edge'
    | 'edge_type'
    | 'cycle';
  readonly msg: string;
  readonly node?: string;
  readonly edge?: number;
  readonly fix?: WfFix;
}

/** The hash shape an agent node must carry: a full `sha256:<64 hex>` reference (the server's own
 * format), or the 16-hex short form draft tooling writes. Anything else is malformed. */
export const HASH_RE = /^sha256:(?:[0-9a-f]{16}|[0-9a-f]{64})$/;

/** Levenshtein distance, small-string only (node ids). */
function lev(a: string, b: string): number {
  const m = a.length;
  const n = b.length;
  const d: number[][] = Array.from({ length: m + 1 }, (_, i) => [i, ...Array<number>(n).fill(0)]);
  for (let j = 1; j <= n; j++) d[0][j] = j;
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= n; j++) {
      d[i][j] = Math.min(
        d[i - 1][j] + 1,
        d[i][j - 1] + 1,
        d[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1),
      );
    }
  }
  return d[m][n];
}

/** The one UNAMBIGUOUS near-miss for a dangling reference: within edit distance 2 and strictly
 * closer than every other candidate — otherwise no suggestion, never a guess. */
export function nearestId(name: string, ids: readonly string[]): string | null {
  const scored = ids.map((id) => ({ id, d: lev(name, id) })).sort((x, y) => x.d - y.d);
  return scored[0] && scored[0].d <= 2 && (!scored[1] || scored[1].d > scored[0].d)
    ? scored[0].id
    : null;
}

export function validateGraph(g: WfGraph): WfError[] {
  const errs: WfError[] = [];
  const ids = g.nodes.map((n) => n.id);
  const known = new Set(ids);

  // duplicate node id — an edge that references it points at two things at once
  const seen = new Set<string>();
  const dupes = new Set<string>();
  ids.forEach((id) => (seen.has(id) ? dupes.add(id) : seen.add(id)));
  dupes.forEach((id) =>
    errs.push({
      code: 'duplicate_id',
      node: id,
      msg: `Two nodes share the id ${id}. Every edge referencing it is ambiguous.`,
      fix: { label: `Rename the second to ${id}_2`, kind: 'rename_dupe', id },
    }),
  );

  g.nodes.forEach((n) => {
    if (n.kind === 'agent' && !HASH_RE.test(n.agentHash ?? '')) {
      errs.push({
        code: 'bad_agent_hash',
        node: n.id,
        msg: `${n.id} references an agent by a malformed hash (${n.agentHash}). An agent node carries a sha256: hash and nothing else — no prompt, no model.`,
      });
    }
    if (n.kind === 'map' && !((n.concurrency ?? 0) > 0)) {
      errs.push({
        code: 'bad_concurrency',
        node: n.id,
        msg: `${n.id} has concurrency ${n.concurrency}. A fan-out over a list needs at least one worker.`,
        fix: { label: 'Set concurrency to 1', kind: 'set_concurrency', id: n.id },
      });
    }
    if (n.kind === 'tool' && n.effect !== undefined && !['read', 'write', 'idempotent'].includes(n.effect)) {
      errs.push({
        code: 'bad_effect',
        node: n.id,
        msg: `${n.id} declares no effect class. Fork safety is decided by this field.`,
      });
    }
    if (n.kind === 'branch') {
      const realized = new Set(g.edges.filter((e) => e.from === n.id).map((e) => e.label));
      (n.cases ?? [])
        .filter((c) => !realized.has(c))
        .forEach((c) =>
          errs.push({
            code: 'unrealized_case',
            node: n.id,
            msg: `${n.id} declares the case "${c}" but no edge realizes it.`,
          }),
        );
    }
  });

  g.edges.forEach((e, i) => {
    (['from', 'to'] as const).forEach((end) => {
      if (known.has(e[end])) return;
      const sug = nearestId(e[end], ids);
      errs.push({
        code: 'dangling_edge',
        edge: i,
        msg: `An edge ${end === 'from' ? 'leaves' : 'arrives at'} ${e[end]}, which is not a node in this graph.`,
        ...(sug ? { fix: { label: `Point it at ${sug}`, kind: 'repoint', edge: i, end, to: sug } } : {}),
      });
    });
    // edge type mismatch: a case label is a BRANCH's vocabulary — anywhere else it is a lie about
    // how the edge is chosen
    const src = g.nodes.find((n) => n.id === e.from);
    if (src && src.kind === 'branch') {
      if (!e.label) {
        errs.push({
          code: 'edge_type',
          edge: i,
          msg: `An edge out of the branch ${src.id} carries no case. Every branch edge names the case it realizes.`,
        });
      } else if (!(src.cases ?? []).includes(e.label)) {
        errs.push({
          code: 'edge_type',
          edge: i,
          msg: `The branch ${src.id} has no case "${e.label}". Its cases are: ${(src.cases ?? []).join(', ')}.`,
        });
      }
    } else if (src && e.label) {
      errs.push({
        code: 'edge_type',
        edge: i,
        msg: `This edge carries the case "${e.label}", but ${src.id} is a ${src.kind}, not a branch. Only a branch picks a case.`,
        fix: { label: 'Drop the label', kind: 'drop_label', edge: i },
      });
    }
  });

  // a cycle, WITH the path
  const out: Record<string, string[]> = {};
  g.nodes.forEach((n) => (out[n.id] = out[n.id] ?? []));
  g.edges.forEach((e) => {
    if (out[e.from] && known.has(e.to)) out[e.from].push(e.to);
  });
  const state: Record<string, number> = {};
  const stack: string[] = [];
  const walk = (id: string): string[] | null => {
    if (state[id] === 2) return null;
    if (state[id] === 1) return stack.slice(stack.indexOf(id)).concat(id);
    state[id] = 1;
    stack.push(id);
    for (const nxt of out[id]) {
      const c = walk(nxt);
      if (c) return c;
    }
    stack.pop();
    state[id] = 2;
    return null;
  };
  for (const n of g.nodes) {
    const cyc = walk(n.id);
    if (cyc) {
      const back = g.edges.findIndex((e) => e.from === cyc[cyc.length - 2] && e.to === cyc[cyc.length - 1]);
      errs.push({
        code: 'cycle',
        edge: back,
        msg: `A cycle: ${cyc.join(' → ')}. A graph runs forward; this one would never finish.`,
        ...(back >= 0 ? { fix: { label: 'Remove the closing edge', kind: 'drop_edge', edge: back } } : {}),
      });
      break;
    }
  }
  return errs;
}

/** The panel-head verdict — one sentence, derived from the error count every render. */
export function verdictOf(errs: readonly WfError[]): string {
  return errs.length
    ? `${errs.length} error${errs.length === 1 ? '' : 's'} — publish is blocked`
    : 'No errors · this graph can be published';
}

/** Apply one {@link WfFix} to a graph, immutably. Unknown targets are a no-op (the fix was
 * computed from the same graph, so a miss means the graph already changed under it). */
export function applyFix(g: WfGraph, fix: WfFix): WfGraph {
  switch (fix.kind) {
    case 'rename_dupe': {
      let seen = 0;
      const nodes: WfNode[] = g.nodes.map((n) => {
        if (n.id !== fix.id) return n;
        seen += 1;
        return seen >= 2 ? { ...n, id: `${n.id}_2` } : n;
      });
      return { ...g, nodes };
    }
    case 'set_concurrency':
      return {
        ...g,
        nodes: g.nodes.map((n) => (n.id === fix.id ? { ...n, concurrency: 1 } : n)),
      };
    case 'repoint':
      return {
        ...g,
        edges: g.edges.map((e, i) =>
          i === fix.edge && fix.end && fix.to ? { ...e, [fix.end]: fix.to } : e,
        ),
      };
    case 'drop_label':
      return {
        ...g,
        edges: g.edges.map((e, i) => {
          if (i !== fix.edge) return e;
          const rest: WfEdge = { from: e.from, to: e.to };
          return rest;
        }),
      };
    case 'drop_edge':
      return { ...g, edges: g.edges.filter((_, i) => i !== fix.edge) };
  }
}
