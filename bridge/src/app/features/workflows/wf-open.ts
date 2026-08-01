import { SCHEMA_VERSION, type Graph } from '@salvor-run/client';

import { WF_KIND_LIST } from './wf-kinds';
import { type WfGraph, documentBody } from './wf-model';
import { type WfError, validateGraph } from './wf-validate';

/**
 * OPENING A GRAPH DOCUMENT. A document written somewhere else (by `salvor graph edit`, a file under
 * `examples/graphs/`, a paste out of a terminal) reaches the canvas through this one function,
 * whatever affordance carried the bytes. The file picker, the drop on the canvas and the paste box
 * all hand their text here and read one answer back, so there is a single place where "a document
 * from outside" becomes something the canvas holds.
 *
 * Three rules the surface cannot bend, because they live here rather than in a handler:
 *
 * It ARRIVES AS A DRAFT, always: `hash: null`, `state: 'draft'`, a `draft:`-prefixed key. A document
 * carries no identity, because the hash is computed from the bytes and IS the version, so an opened file
 * has none to carry. Even a document byte-identical to a published graph opens as a draft:
 * recognising it as the published one would let a local edit appear to modify the very document runs
 * already reference.
 *
 * It is VALIDATED BEFORE IT LANDS, by the same {@link validateGraph} a draft is judged by. A broken
 * document is refused with the same node-precise errors, and nothing is written: no half-loaded
 * canvas, and no draft left behind to clean up.
 *
 * It NEVER TOUCHES A PUBLISHED GRAPH. Nothing here writes to the server, and the only name it can
 * produce is a draft name; a published graph is keyed by its hash and lives in the server's list,
 * out of reach of every path below.
 */

/** Where a document came from, in the two registers the operator needs: the name it is REPORTED
 * under, and the base its draft name is DERIVED from. A file supplies one string for both; a paste
 * has no file name, so it reads as prose and is named by a plain word. */
export interface WfOpenSource {
  readonly label: string;
  readonly nameFrom: string;
}

export interface WfOpenRefusal {
  /** The document, named the way it was offered. */
  readonly source: string;
  /** The headline: what is wrong, in a phrase. */
  readonly head: string;
  /** One sentence of why, and what to do about it. */
  readonly why: string;
  /** The validator's own errors, when the document read far enough to be judged by them. */
  readonly errors: readonly WfError[];
  /** The graph those errors were computed against, so an error naming an edge index can be rendered
   * as the edge it means. Held only for that rendering; a refused document is never stored. */
  readonly graph?: WfGraph;
}

export type WfOpenOutcome =
  | { readonly ok: true; readonly graph: WfGraph }
  | { readonly ok: false; readonly refusal: WfOpenRefusal };

/** The node kinds a document may declare, in the graph format's own order: the same derived list the
 * palette adds from (see wf-kinds.ts), so what this refuses to read is exactly what cannot be drawn.
 *
 * The schema version comes from the SDK's own `SCHEMA_VERSION`, the constant the builder stamps onto
 * every document it writes: a document declaring another version may mean something different by the
 * same field names, and guessing is worse than saying so. */
const KINDS = WF_KIND_LIST;

/** The ceiling on a derived draft name, in characters: long enough for a real file name, short
 * enough that the picker stays readable. */
const NAME_MAX = 48;

/** The word a paste (or an unnameable file) is named after. */
const FALLBACK_NAME = 'opened-graph';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** What a JSON value IS, for an error message that names the thing the author actually wrote. */
function shapeOf(value: unknown): string {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'an array';
  return `a ${typeof value}`;
}

/**
 * THE STRUCTURAL READ, and only that. File bytes are not typechecked by anything on the way in, so
 * before the document may be treated as a {@link Graph} every field the normaliser dereferences is
 * confirmed to be there. This is deliberately NOT a second validator: a dangling edge, a malformed
 * agent hash, a zero-worker fan-out, a cycle are all graph facts, and they belong to
 * {@link validateGraph}, which reports them with fixes. What is checked here is only whether the
 * JSON is a graph document at all, the one question the validator cannot be asked, because it takes a
 * graph as its argument.
 */
function readDocument(raw: unknown): { readonly doc: Graph } | { readonly problem: string } {
  if (!isRecord(raw)) {
    return {
      problem: `A graph document is a JSON object of the form { schema_version, nodes, edges }. This one's top level is ${shapeOf(raw)}.`,
    };
  }
  const version = raw['schema_version'];
  if (version === undefined) {
    return {
      problem: 'This document declares no schema_version. A graph document opens with { "schema_version": 1, … }.',
    };
  }
  if (version !== SCHEMA_VERSION) {
    return {
      problem: `This document declares schema_version ${JSON.stringify(version)}; this canvas reads version ${SCHEMA_VERSION}, and another version may mean something different by the same fields.`,
    };
  }
  const nodes = raw['nodes'];
  if (!Array.isArray(nodes)) {
    return { problem: `This document's nodes field is ${shapeOf(nodes)}, not an array of nodes.` };
  }
  if (nodes.length === 0) {
    return { problem: 'This document has no nodes, so there is no graph in it to open.' };
  }
  const edges = raw['edges'];
  if (edges !== undefined && !Array.isArray(edges)) {
    return { problem: `This document's edges field is ${shapeOf(edges)}, not an array of edges.` };
  }

  for (let i = 0; i < nodes.length; i++) {
    const problem = readNode(nodes[i], i);
    if (problem) return { problem };
  }
  for (let i = 0; i < (edges ?? []).length; i++) {
    const problem = readEdge((edges as unknown[])[i], i);
    if (problem) return { problem };
  }
  // Every field the normaliser reads is present and of the right shape; `edges` is optional in the
  // read above, so an absent one becomes the empty list the model expects.
  return { doc: { ...raw, edges: edges ?? [] } as unknown as Graph };
}

/** @returns the structural problem with node `i`, or `undefined` when it reads. */
function readNode(node: unknown, i: number): string | undefined {
  const at = `nodes[${i}]`;
  if (!isRecord(node)) return `${at} is ${shapeOf(node)}, not a node object.`;
  const kind = node['kind'];
  if (typeof kind !== 'string' || !(KINDS as readonly string[]).includes(kind)) {
    return kind === undefined
      ? `${at} declares no kind. Every node names one of: ${KINDS.join(', ')}.`
      : `${at} declares the kind ${JSON.stringify(kind)}. A node is one of: ${KINDS.join(', ')}.`;
  }
  const payload = node['payload'];
  if (!isRecord(payload)) {
    return `${at} is a ${kind} with ${shapeOf(payload)} for a payload. Every node carries its fields in a payload object.`;
  }
  const id = payload['id'];
  if (typeof id !== 'string' || !id) {
    return `${at} (${kind}) carries no payload.id. The id is the join key every edge references.`;
  }
  const name = payload['name'];
  if (name !== undefined && typeof name !== 'string') {
    return `${at} (${kind} ${id}) has ${shapeOf(name)} for a display name.`;
  }
  const where = `${at} (${kind} ${id})`;
  switch (kind) {
    case 'branch': {
      const cases = payload['cases'];
      if (!Array.isArray(cases)) {
        return `${where} has ${shapeOf(cases)} for its cases. A branch names the cases it routes between.`;
      }
      for (let c = 0; c < cases.length; c++) {
        const one = cases[c];
        if (!isRecord(one) || typeof one['name'] !== 'string') {
          return `${where} has a case at index ${c} with no name.`;
        }
      }
      return undefined;
    }
    case 'map':
      return isRecord(payload['body'])
        ? undefined
        : `${where} has ${shapeOf(payload['body'])} for its body. A map names the body each element runs through.`;
    case 'fold': {
      if (!isRecord(payload['body'])) {
        return `${where} has ${shapeOf(payload['body'])} for its body. A fold names the body each pass runs.`;
      }
      const join = payload['join'];
      return isRecord(join) && typeof join['kind'] === 'string'
        ? undefined
        : `${where} names no join. A fold folds its passes by best_by, last, or all.`;
    }
    default:
      return undefined;
  }
}

/** @returns the structural problem with edge `i`, or `undefined` when it reads. */
function readEdge(edge: unknown, i: number): string | undefined {
  const at = `edges[${i}]`;
  if (!isRecord(edge)) return `${at} is ${shapeOf(edge)}, not an edge object.`;
  if (typeof edge['from'] !== 'string' || !edge['from'] || typeof edge['to'] !== 'string' || !edge['to']) {
    return `${at} does not name both a from and a to. An edge is a pair of node ids.`;
  }
  const label = edge['label'];
  return label === undefined || typeof label === 'string'
    ? undefined
    : `${at} has ${shapeOf(label)} for a case label.`;
}

/** The stem a draft name starts from: the file's leaf, without its `.json`, reduced to the
 * characters a picker label and a `draft:` key can carry without surprise. */
function baseName(source: string): string {
  const leaf = source.split(/[\\/]/).pop() ?? '';
  const cleaned = leaf
    .replace(/\.json$/i, '')
    .trim()
    .replace(/\s+/g, '-')
    .replace(/[^A-Za-z0-9._-]/g, '')
    .replace(/^[-._]+/, '')
    .slice(0, NAME_MAX);
  return cleaned || FALLBACK_NAME;
}

/**
 * A draft name no existing draft is using. THE ANTI-CLOBBER RULE, in one function: a draft's key is
 * `draft:<name>`, so a free name is a free key, and an open can never land on top of work the author
 * already has: not the seeded draft, not a previous open of the same file. The second open of
 * `refund-sweep.json` becomes `refund-sweep-2`, the third `refund-sweep-3`, which also says in the
 * picker that they are separate documents rather than one document twice.
 */
export function freeDraftName(source: string, takenNames: readonly string[]): string {
  const base = baseName(source);
  const used = new Set(takenNames);
  if (!used.has(base)) return base;
  // `used` is finite, so a free suffix exists at or before `used.size + 2`; the walk terminates.
  let n = 2;
  let candidate = `${base}-${n}`;
  while (used.has(candidate)) candidate = `${base}-${++n}`;
  return candidate;
}

/**
 * Read one graph document and either hand back the draft it becomes, or a refusal that names what is
 * wrong. Pure: it writes nothing, so a refusal costs the author nothing and a success is the
 * caller's to persist.
 *
 * @param text the document's bytes, as text
 * @param source how the document is named and where its draft name comes from
 * @param takenNames the names of the drafts that already exist, so this one gets its own
 */
export function openGraphDocument(
  text: string,
  source: WfOpenSource,
  takenNames: readonly string[],
): WfOpenOutcome {
  const refuse = (head: string, why: string): WfOpenOutcome => ({
    ok: false,
    refusal: { source: source.label, head, why, errors: [] },
  });

  if (!text.trim()) {
    return refuse('There is nothing here to open', 'The document is empty.');
  }
  let raw: unknown;
  try {
    raw = JSON.parse(text) as unknown;
  } catch (err) {
    return refuse(
      'This is not JSON',
      `${err instanceof Error ? err.message : String(err)}. Nothing was opened.`,
    );
  }
  const read = readDocument(raw);
  if ('problem' in read) return refuse('This is not a graph document', read.problem);

  // A DRAFT, unconditionally: no hash, no server identity, a `draft:` key under a free name. The
  // document's own bytes cannot argue with this, which is the point: a file that happens to match
  // something published still opens as the author's own copy to edit.
  const { nodes, edges } = documentBody(read.doc);
  const name = freeDraftName(source.nameFrom, takenNames);
  const graph: WfGraph = { key: `draft:${name}`, hash: null, name, state: 'draft', nodes, edges };

  // The gate, run BEFORE anything lands: the same validator, so a bad document reports the same
  // node-precise errors a bad draft does.
  const errors = validateGraph(graph);
  if (errors.length) {
    return {
      ok: false,
      refusal: {
        source: source.label,
        head: `${errors.length} error${errors.length === 1 ? '' : 's'} in this document`,
        why: 'A document is judged before it is opened, by the validator that gates publish. Fix the document and open it again; nothing here was written.',
        errors,
        graph,
      },
    };
  }
  return { ok: true, graph };
}
