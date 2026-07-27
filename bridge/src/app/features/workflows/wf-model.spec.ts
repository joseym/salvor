import { describe, expect, it } from 'vitest';

import type { Graph } from '@salvor-run/client';
import { fromServerGraph } from './wf-model';

/**
 * `fromServerGraph`'s node-name fallback: a server document's node payload may carry its own
 * optional `name` (the graph format's optional node display name); when it does the canvas must
 * show that display name, and when it does not the node's id is the honest fallback — never an
 * invented sentence. Both facts must hold in the SAME document, since a document can name some
 * nodes and not others.
 */
const doc: Graph = {
  schema_version: 1,
  nodes: [
    {
      kind: 'agent',
      payload: { id: 'research', agent_hash: `sha256:${'1'.repeat(64)}`, name: 'Research the topic' },
    },
    {
      kind: 'gate',
      payload: { id: 'approve', approval_schema: { type: 'object' } },
    },
  ],
  edges: [{ from: 'research', to: 'approve' }],
};

describe('fromServerGraph — node display name fallback', () => {
  it('renders a node payload name when the document carries one', () => {
    const graph = fromServerGraph('sha256:abc', doc);
    const research = graph.nodes.find((n) => n.id === 'research');
    expect(research?.name).toBe('Research the topic');
  });

  it('falls back to the node id when the document carries no name for it', () => {
    const graph = fromServerGraph('sha256:abc', doc);
    const approve = graph.nodes.find((n) => n.id === 'approve');
    expect(approve?.name).toBe('approve');
  });

  it('a wholly unnamed document falls back to id for every node', () => {
    const unnamed: Graph = {
      schema_version: 1,
      nodes: [{ kind: 'gate', payload: { id: 'approve', approval_schema: { type: 'object' } } }],
      edges: [],
    };
    const graph = fromServerGraph('sha256:def', unnamed);
    expect(graph.nodes[0].name).toBe('approve');
  });
});

describe('fromServerGraph — fold node', () => {
  const foldDoc: Graph = {
    schema_version: 1,
    nodes: [
      { kind: 'agent', payload: { id: 'tailor', agent_hash: `sha256:${'3'.repeat(64)}` } },
      {
        kind: 'fold',
        payload: {
          id: 'refine',
          name: 'Refine to threshold',
          body: { kind: 'node', value: 'tailor' },
          max_iterations: 3,
          stop_when: 'score >= 0.85',
          join: { kind: 'best_by', value: 'score' },
        },
      },
    ],
    edges: [],
  };

  it('maps a fold node bound, predicate, join label, and body', () => {
    const graph = fromServerGraph('sha256:fold', foldDoc);
    const refine = graph.nodes.find((n) => n.id === 'refine');
    expect(refine?.kind).toBe('fold');
    expect(refine?.name).toBe('Refine to threshold');
    expect(refine?.maxIterations).toBe(3);
    expect(refine?.stopWhen).toBe('score >= 0.85');
    expect(refine?.join).toBe('keeps the best by score');
    expect(refine?.body?.node).toBe('tailor');
  });
});
