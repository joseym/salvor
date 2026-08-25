import { GraphBuilder, type GraphNode } from '@salvor-run/client';

/**
 * WHAT IS ADDABLE, derived from the format rather than listed beside it.
 *
 * A palette of node kinds hand-maintained in the canvas is exactly how a second surface drifts from
 * the first: the CLI's editor grows a field, the canvas does not, and the two disagree about what a
 * document is. So nothing here restates the format. Two mechanisms do the work, and both fail at
 * BUILD time rather than in front of an author:
 *
 * THE SET OF KINDS is a mapped type over `GraphNode['kind']`, the SDK's own adjacently tagged node
 * union (`@salvor-run/client`, itself the mirror of `salvor_graph::document::Node`). {@link WF_KINDS}
 * must therefore carry one entry per kind and no others: a seventh kind in the format makes this
 * object literal a compile error until the canvas answers for it, and a kind removed makes the
 * stale entry one. There is no place here where the six names could be quietly wrong.
 *
 * EACH KIND'S REQUIRED FIELDS come from {@link GraphBuilder}, the SDK's own constructor for these
 * documents: `seed` builds its node by calling the builder method for that kind, so the REQUIRED
 * arguments are the ones the format requires (a `map` with no `over`, a `fold` with no `stop_when`,
 * a `gate` with no approval schema does not compile), and the optional ones stay off the emitted
 * payload entirely, which is the same byte-level rule the document format states for an unset field.
 * A field added to the format as required arrives here as a missing argument.
 *
 * The one thing this file DOES choose is the value a fresh node starts at, and it chooses the
 * honest one: a field only the author can know (an agent's hash, a tool's name, a map's list
 * reference, a body's target node) starts EMPTY, so the validator names it as the next thing to
 * decide, rather than the canvas inventing a plausible-looking value the author might publish
 * without reading. The two numeric bounds start at 1, the smallest value the validator accepts,
 * because a zero-worker fan-out or a zero-pass loop is not a decision anyone would mean.
 */
export interface WfKindSpec {
  /** One line of prose for the inspector: what a node of this kind IS. */
  readonly blurb: string;
  /**
   * The document node a fresh add mints, built through {@link GraphBuilder} so the payload is the
   * same shape a TypeScript author writing the document by hand would produce.
   */
  readonly seed: (id: string) => GraphNode;
}

/** One entry per kind the format declares, enforced by the mapped key set. */
export type WfKindTable = { readonly [K in GraphNode['kind']]: WfKindSpec };

/** Build one node through the SDK's builder and hand it back on its own. */
function one(add: (builder: GraphBuilder) => GraphBuilder): GraphNode {
  return add(new GraphBuilder()).build().nodes[0];
}

export const WF_KINDS: WfKindTable = {
  agent: {
    blurb: 'a full agent loop, referenced by hash',
    // The hash cannot be invented: it names someone else's document, all 32 bytes of it. Empty, so
    // the validator's malformed-hash error is the first thing the author reads.
    seed: (id) => one((b) => b.agent(id, '')),
  },
  tool: {
    blurb: 'one direct tool invocation',
    seed: (id) => one((b) => b.tool(id, '')),
  },
  gate: {
    blurb: 'a human approval that suspends the run',
    // An object schema is the smallest gate the format accepts: an approval that declares no
    // fields, which is still a real approval. Its properties are the author's to add.
    seed: (id) => one((b) => b.gate(id, { type: 'object' })),
  },
  branch: {
    // A branch is added with NO cases, exactly as `add branch` in the CLI's editor leaves it: a case
    // is a decision with a condition behind it, and there is no default condition to guess.
    blurb: 'routes on a recorded decision',
    seed: (id) => one((b) => b.branch(id, [])),
  },
  map: {
    blurb: 'fans out a sub-run per element of a list',
    seed: (id) => one((b) => b.map(id, '', 1, { kind: 'node', value: '' })),
  },
  fold: {
    blurb: 'iterates a body until it converges, then joins the passes',
    // `false` is the never-stop predicate: a valid expression in the condition language, so the
    // bound is what ends the loop until the author writes a real one. `last` is the join that needs
    // no reference to a field the document does not have yet.
    seed: (id) => one((b) => b.fold(id, { kind: 'node', value: '' }, 1, 'false', { kind: 'last' })),
  },
  delay: {
    blurb: 'a durable wait, then the walk continues',
    // 1 second: the smallest wait the validator accepts, the same floor `map`'s worker count and
    // `fold`'s iteration bound start at above.
    seed: (id) => one((b) => b.delay(id, 1)),
  },
};

/**
 * The kinds, in the format's own order. Read off {@link WF_KINDS} rather than written again, so the
 * palette, the document reader's kind check and the inspector all count the same six.
 */
export const WF_KIND_LIST = Object.keys(WF_KINDS) as readonly GraphNode['kind'][];
