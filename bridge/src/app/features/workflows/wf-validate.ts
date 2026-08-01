import { type WfEdge, type WfGraph, type WfNode, withDocFields } from './wf-model';

/**
 * THE CLIENT-SIDE VALIDATOR. Publish is gated on this list, the canvas marks the same offenders
 * the panel names, and the panel head IS the verdict, so there is exactly ONE error list per
 * render, derived from the graph, never cached. Each error class is a different kind of failure:
 * a duplicate id (the join key every edge references stops being unique), a dangling edge (a
 * reference to nothing), a map or fold body naming a node the document does not have, a malformed
 * agent hash, a zero-worker fan-out, an unrealized branch case, a model-decision case with no agent
 * to decide it, an oversized or blank display name, a case label on a non-branch edge, and a cycle
 * (reported WITH its path: "there is a cycle" is a rumour, not an error message).
 *
 * It is also what a STRUCTURAL edit is answerable to. Deleting a node leaves the edges that named it
 * in the document on purpose (see wf-edit.ts), so each one lands here as a dangling edge naming what
 * it used to reach, rather than disappearing with the node.
 *
 * Aligned to the server's own rules (`salvor_graph::validate`, sync-ledger item 4): the agent
 * hash check accepts only a full `sha256:<64 hex>` string (no transitional short form), a
 * `model_decision` branch case requires the branch's own `agentHash`, and a node's display name
 * carries the same length/blank bounds the server enforces.
 *
 * Fixes are DATA, not closures: a {@link WfFix} names what to change and {@link applyFix} applies
 * it to a graph immutably, so the panel's one-click offers and their unit tests share one
 * implementation.
 */
export interface WfFix {
  readonly label: string;
  readonly kind:
    | 'rename_dupe'
    | 'set_concurrency'
    | 'repoint'
    | 'repoint_body'
    | 'drop_label'
    | 'drop_edge'
    | 'complete_hash'
    | 'attach_agent'
    | 'truncate_name'
    | 'clear_name';
  readonly id?: string;
  readonly edge?: number;
  readonly end?: 'from' | 'to';
  readonly to?: string;
  /** The donor agent hash an `attach_agent` fix reuses: always a hash already present on some
   * other node in the same document, never invented. */
  readonly hash?: string;
}

export interface WfError {
  readonly code:
    | 'duplicate_id'
    | 'bad_agent_hash'
    | 'bad_concurrency'
    | 'bad_effect'
    | 'unrealized_case'
    | 'model_decision_without_agent'
    | 'name_too_long'
    | 'name_empty'
    | 'dangling_edge'
    | 'dangling_body'
    | 'edge_type'
    | 'cycle';
  readonly msg: string;
  readonly node?: string;
  /** The offending case name, set only alongside `model_decision_without_agent`: the same
   * node/case precision the server's own error carries. */
  readonly case?: string;
  readonly edge?: number;
  readonly fix?: WfFix;
}

/** The server's rule, not this app's own: a full sha256 digest, 64 lowercase hex characters, no
 * matter how short a draft's own short-hash tooling elsewhere spells one. An agent hash names
 * someone else's document: all 32 bytes of it are the pin, or none are. Sync-ledger item 4: the
 * 16-hex transitional tolerance is gone; the server never accepted it either. */
export const HASH_RE = /^sha256:[0-9a-f]{64}$/;

/** The server's ceiling on a node's display name, in characters (mirrors
 * `salvor_graph::validate::MAX_NODE_NAME_LEN`). A name field that IS set must carry real text;
 * empty or all-whitespace says nothing a missing field wouldn't say more honestly. */
const NODE_NAME_MAX = 64;

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
 * closer than every other candidate; otherwise no suggestion, never a guess. */
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

  // duplicate node id: an edge that references it points at two things at once
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
      // A short-but-valid hex hash can be COMPLETED (padded out to length); a hash that is not
      // hex at all cannot: there is nothing honest to pad. That distinction is what decides
      // whether this error gets a one-click fix or sends the author to the field by hand.
      const hex = (n.agentHash ?? '').replace(/^sha256:/, '');
      const canComplete = /^[0-9a-f]{1,63}$/.test(hex);
      errs.push({
        code: 'bad_agent_hash',
        node: n.id,
        msg: `${n.id} references an agent by a malformed hash (${n.agentHash}). An agent node carries a full sha256: hash (64 hex characters) and nothing else: no prompt, no model.`,
        ...(canComplete ? { fix: { label: 'Complete to 64 hex characters', kind: 'complete_hash', id: n.id } } : {}),
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
    // A map's or fold's body names a node in THIS document (the format's other form, an embedded
    // subgraph, names no id and so has nothing to dangle). The server checks this at submit
    // (`dangling_map_body`, `dangling_fold_body`), so a client that did not would call a document
    // publishable and then watch the server refuse it, which is worse than not checking at all.
    // It is also what a delete leaves behind: removing the node a body runs never removes the body.
    if ((n.kind === 'map' || n.kind === 'fold') && n.body?.node !== undefined && !known.has(n.body.node)) {
      const what = n.kind === 'map' ? 'maps each element through' : 'runs each pass through';
      const sug = nearestId(n.body.node, ids);
      errs.push({
        code: 'dangling_body',
        node: n.id,
        msg: n.body.node
          ? `${n.id} ${what} ${n.body.node}, which is not a node in this graph.`
          : `${n.id} names no body: pick the node it ${what}.`,
        ...(sug ? { fix: { label: `Point the body at ${sug}`, kind: 'repoint_body', id: n.id, to: sug } } : {}),
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

      // A model-decided case is not realized by an edge label at all; it is realized by an
      // agent's judgement at run time, every time the node runs. That agent has to be NAMED on
      // the node, by hash, the same as an agent node names one, or the case is a promise with
      // no one behind it.
      const modelCases = n.modelCases ?? [];
      if (modelCases.length && !HASH_RE.test(n.agentHash ?? '')) {
        // Never fabricate an agent. Offer the fix only when the graph already names a real one
        // elsewhere: attaching that is reusing evidence already in the document, not guessing.
        const donor = g.nodes.find((x) => x.kind === 'agent' && HASH_RE.test(x.agentHash ?? ''));
        modelCases.forEach((c) =>
          errs.push({
            code: 'model_decision_without_agent',
            node: n.id,
            case: c,
            msg: `${n.id}'s case "${c}" is a model's decision at run time, but the node names no agent_hash for which agent decides it.`,
            ...(donor ? { fix: { label: `Attach ${donor.id}'s agent`, kind: 'attach_agent', id: n.id, hash: donor.agentHash } } : {}),
          }),
        );
      }
    }

    // The name is optional in the server document: a node need not carry one. But WfNode always
    // carries a `name` (the id, honestly, when the document set none: see wf-model.ts), and a
    // value that IS present has to be a name: within the server's character ceiling, and not
    // empty or whitespace masquerading as one. Two node-precise errors, not one, so a fix targets
    // the actual problem.
    if (n.name.length > NODE_NAME_MAX) {
      errs.push({
        code: 'name_too_long',
        node: n.id,
        msg: `${n.id}'s display name is ${n.name.length} characters; the ceiling is ${NODE_NAME_MAX}.`,
        fix: { label: `Truncate to ${NODE_NAME_MAX} characters`, kind: 'truncate_name', id: n.id },
      });
    } else if (!n.name.trim()) {
      errs.push({
        code: 'name_empty',
        node: n.id,
        msg: `${n.id} carries a name field that is empty or all whitespace. Omit the field, or give it real text.`,
        fix: { label: 'Clear the name', kind: 'clear_name', id: n.id },
      });
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
    // edge type mismatch: a case label is a BRANCH's vocabulary; anywhere else it is a lie about
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

/** The panel-head verdict: one sentence, derived from the error count every render. */
export function verdictOf(errs: readonly WfError[]): string {
  return errs.length
    ? `${errs.length} error${errs.length === 1 ? '' : 's'}: publish is blocked`
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
        nodes: g.nodes.map((n) =>
          n.id === fix.id ? withDocFields({ ...n, concurrency: 1 }, { concurrency: 1 }) : n,
        ),
      };
    case 'repoint':
      return {
        ...g,
        edges: g.edges.map((e, i) =>
          i === fix.edge && fix.end && fix.to ? { ...e, [fix.end]: fix.to } : e,
        ),
      };
    case 'repoint_body':
      return {
        ...g,
        nodes: g.nodes.map((n) =>
          n.id === fix.id && fix.to !== undefined
            ? withDocFields(
                { ...n, body: { ...(n.body ?? {}), node: fix.to } },
                { body: { kind: 'node', value: fix.to } },
              )
            : n,
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
    case 'complete_hash':
      // Mechanical, not invented: the hex the author already typed, padded out to the length the
      // server requires. It looks exactly as fake as it is: a real hash still has to come from
      // the real agent, but it gets the field past "malformed" and into "author, keep going".
      return {
        ...g,
        nodes: g.nodes.map((n) => {
          if (n.id !== fix.id) return n;
          const hash = `sha256:${(n.agentHash ?? '').replace(/^sha256:/, '').padEnd(64, '0')}`;
          return withDocFields({ ...n, agentHash: hash }, { agent_hash: hash });
        }),
      };
    case 'attach_agent':
      return {
        ...g,
        nodes: g.nodes.map((n) =>
          n.id === fix.id && fix.hash !== undefined
            ? withDocFields({ ...n, agentHash: fix.hash }, { agent_hash: fix.hash })
            : n,
        ),
      };
    case 'truncate_name':
      return {
        ...g,
        nodes: g.nodes.map((n) => {
          if (n.id !== fix.id) return n;
          const name = n.name.slice(0, NODE_NAME_MAX);
          return withDocFields({ ...n, name }, { name });
        }),
      };
    case 'clear_name':
      // WfNode's `name` is required (unlike the server document's optional field: see
      // wf-model.ts), so "clear" means revert to the same honest fallback documentNode uses
      // when a document sets none at all: the node's own id.
      // The document's field is OMITTED, not set to the id: an absent name is what a document
      // that never named the node says, and it is what publish must write back.
      return {
        ...g,
        nodes: g.nodes.map((n) =>
          n.id === fix.id ? withDocFields({ ...n, name: n.id }, { name: undefined }) : n,
        ),
      };
  }
}
