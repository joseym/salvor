import { describe, expect, it } from 'vitest';

import { GraphBuilder } from '@salvor-run/client';

import { WF_KINDS, WF_KIND_LIST } from './wf-kinds';
import { documentNode } from './wf-model';
import { openGraphDocument } from './wf-open';
import { validateGraph } from './wf-validate';

/**
 * THE PIN ON THE PALETTE. The kind table's shape is enforced by the compiler (a mapped type over the
 * SDK's node union, and the SDK builder's own required arguments: see wf-kinds.ts), and none of that
 * is visible at run time. These are the run-time halves of the same guarantee.
 *
 * The first test is the one that catches a format that moved: {@link GraphBuilder} is the SDK's
 * constructor for these documents, and it grows a method when the format grows a node kind. If it
 * ever offers a seventh and the palette does not, this fails, by name.
 */

/** The builder's methods that are not node constructors: an edge is not a node, and `build` ends the
 * chain. Three names, so a new one that is neither is caught rather than silently classified. */
const NOT_A_KIND = new Set(['constructor', 'edge', 'build']);

function builderKinds(): string[] {
  return Object.getOwnPropertyNames(GraphBuilder.prototype)
    .filter((m) => !NOT_A_KIND.has(m))
    .sort();
}

describe('the palette is the format s own set of kinds', () => {
  it('offers exactly the kinds the SDK builder can construct', () => {
    expect(WF_KIND_LIST.slice().sort()).toEqual(builderKinds());
  });

  it('keeps the entries and the list in step, in the format s own order', () => {
    expect(WF_KIND_LIST).toEqual(Object.keys(WF_KINDS));
    expect(WF_KIND_LIST).toEqual(['agent', 'tool', 'gate', 'branch', 'map', 'fold']);
  });

  it('seeds a node of the kind it is filed under, under the id it was asked for', () => {
    for (const kind of WF_KIND_LIST) {
      const node = WF_KINDS[kind].seed('n_1');
      expect(node.kind).toBe(kind);
      expect(node.payload.id).toBe('n_1');
    }
  });

  it('seeds no display name: a fresh node has none to state, and an unset field stays off the wire', () => {
    for (const kind of WF_KIND_LIST) {
      expect(Object.keys(WF_KINDS[kind].seed('n_1').payload)).not.toContain('name');
    }
  });
});

describe('a seeded node is a document node, structurally', () => {
  /** One seed, wrapped as the smallest document that can carry it. */
  function docOf(kind: (typeof WF_KIND_LIST)[number]): string {
    return JSON.stringify({ schema_version: 1, nodes: [WF_KINDS[kind].seed('n_1')], edges: [] });
  }

  it('reads as a graph document for every kind: never a STRUCTURAL refusal', () => {
    for (const kind of WF_KIND_LIST) {
      const outcome = openGraphDocument(docOf(kind), { label: kind, nameFrom: kind }, []);
      // A refusal with no validator errors is the structural reader saying "this is not a node of
      // that shape", which is exactly what a seed missing a required field would produce. A refusal
      // WITH errors is the semantic gap the seed leaves on purpose, and the next test names those.
      if (!outcome.ok) expect(outcome.refusal.errors.length).toBeGreaterThan(0);
    }
  });

  it('leaves only the decisions an author has to make, and nothing else', () => {
    const gaps = Object.fromEntries(
      WF_KIND_LIST.map((kind) => {
        const graph = {
          key: 'draft:seed',
          hash: null,
          name: 'seed',
          state: 'draft' as const,
          nodes: [documentNode(WF_KINDS[kind].seed('n_1'))],
          edges: [],
        };
        return [kind, validateGraph(graph).map((e) => e.code)];
      }),
    );
    expect(gaps).toEqual({
      // the hash names someone else's document; nothing here could know it
      agent: ['bad_agent_hash'],
      // a tool name, a gate's schema and a branch's cases are all legal as seeded
      tool: [],
      gate: [],
      branch: [],
      // a body names a node of this document, and a one-node document has no other node to name
      map: ['dangling_body'],
      fold: ['dangling_body'],
    });
  });
});
