import { describe, expect, it } from 'vitest';

import { REFUND_SWEEP_DRAFT } from './wf-draft';
import { fromServerGraph } from './wf-model';
import { freeDraftName, openGraphDocument } from './wf-open';

const FILE = { label: 'linear.json', nameFrom: 'linear.json' };
const HASH_A = `sha256:${'1'.repeat(64)}`;
const HASH_B = `sha256:${'2'.repeat(64)}`;

/** A clean two-node document, in the graph format's own `{ kind, payload }` wire shape. */
function cleanDoc(): string {
  return JSON.stringify({
    schema_version: 1,
    nodes: [
      { kind: 'agent', payload: { id: 'research', agent_hash: HASH_A } },
      { kind: 'agent', payload: { id: 'review', agent_hash: HASH_B, name: 'Review the draft' } },
    ],
    edges: [{ from: 'research', to: 'review' }],
  });
}

describe('opening a graph document', () => {
  it('reads a document into a draft: the nodes, the edges, and the display names', () => {
    const out = openGraphDocument(cleanDoc(), FILE, []);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.nodes.map((n) => n.id)).toEqual(['research', 'review']);
    // A document's optional display name is kept; a node without one reads as its id.
    expect(out.graph.nodes.map((n) => n.name)).toEqual(['research', 'Review the draft']);
    expect(out.graph.edges).toEqual([{ from: 'research', to: 'review' }]);
  });

  it('arrives as a draft: no hash, no server identity, a draft: key', () => {
    const out = openGraphDocument(cleanDoc(), FILE, []);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.hash).toBeNull();
    expect(out.graph.state).toBe('draft');
    expect(out.graph.key).toBe('draft:linear');
  });

  it('opens a document byte-identical to a PUBLISHED graph as a draft anyway', () => {
    // The published graph, as the canvas holds one: keyed by its hash, frozen.
    const hash = `sha256:${'ab'.repeat(32)}`;
    const doc = JSON.parse(cleanDoc()) as Parameters<typeof fromServerGraph>[1];
    const published = fromServerGraph(hash, doc);
    expect(published.state).toBe('published');

    // The same bytes, opened from a file. The hash is not in the document; it is computed from the
    // bytes by the server, so opening cannot recover it, and must not pretend to: a draft that
    // claimed this identity would let a local edit appear to change what runs already reference.
    const out = openGraphDocument(cleanDoc(), FILE, []);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.hash).toBeNull();
    expect(out.graph.state).toBe('draft');
    expect(out.graph.key).not.toBe(published.key);
    expect(out.graph.key.startsWith('draft:')).toBe(true);
    // And the published graph itself is untouched by the read.
    expect(published.hash).toBe(hash);
    expect(published.state).toBe('published');
  });

  it('refuses a broken document with the same node-precise errors a bad draft gets', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [
        { kind: 'agent', payload: { id: 'research', agent_hash: HASH_A } },
        { kind: 'gate', payload: { id: 'approve', approval_schema: { type: 'object' } } },
      ],
      edges: [{ from: 'research', to: 'aprove' }],
    });
    const out = openGraphDocument(doc, { label: 'dangling.json', nameFrom: 'dangling.json' }, []);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.refusal.errors).toHaveLength(1);
    const err = out.refusal.errors[0];
    expect(err.code).toBe('dangling_edge');
    expect(err.msg).toContain('aprove');
    expect(err.msg).toContain('not a node in this graph');
    // The refusal carries the document it judged, so an edge-indexed error can name its pair.
    expect(out.refusal.graph?.edges[0]).toEqual({ from: 'research', to: 'aprove' });
  });

  it('refuses a cycle with its path, exactly as a draft is refused one', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [
        { kind: 'agent', payload: { id: 'draft', agent_hash: HASH_A } },
        { kind: 'agent', payload: { id: 'critique', agent_hash: HASH_B } },
      ],
      edges: [
        { from: 'draft', to: 'critique' },
        { from: 'critique', to: 'draft' },
      ],
    });
    const out = openGraphDocument(doc, FILE, []);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.refusal.errors.map((e) => e.code)).toContain('cycle');
    expect(out.refusal.errors.find((e) => e.code === 'cycle')?.msg).toContain('draft → critique → draft');
  });

  it('names what is not a graph document, rather than failing obscurely', () => {
    const cases: readonly [string, string][] = [
      ['{ not json', 'This is not JSON'],
      ['[]', 'This is not a graph document'],
      ['{"nodes":[],"edges":[]}', 'This is not a graph document'],
      ['{"schema_version":2,"nodes":[],"edges":[]}', 'This is not a graph document'],
      ['{"schema_version":1,"nodes":[],"edges":[]}', 'This is not a graph document'],
      ['', 'There is nothing here to open'],
    ];
    for (const [text, head] of cases) {
      const out = openGraphDocument(text, FILE, []);
      expect(out.ok, text).toBe(false);
      if (out.ok) continue;
      expect(out.refusal.head, text).toBe(head);
      expect(out.refusal.why.length, text).toBeGreaterThan(0);
    }
  });

  it('names the node whose shape the reader cannot read', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [
        { kind: 'agent', payload: { id: 'research', agent_hash: HASH_A } },
        { kind: 'frobnicate', payload: { id: 'x' } },
      ],
      edges: [],
    });
    const out = openGraphDocument(doc, FILE, []);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.refusal.why).toContain('nodes[1]');
    expect(out.refusal.why).toContain('frobnicate');
  });

  it('a branch with no cases array is named as such, not read as a branch with none', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [{ kind: 'branch', payload: { id: 'route' } }],
      edges: [],
    });
    const out = openGraphDocument(doc, FILE, []);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.refusal.why).toContain('nodes[0] (branch route)');
  });

  it('an edge missing an end is named by its index', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [{ kind: 'agent', payload: { id: 'research', agent_hash: HASH_A } }],
      edges: [{ from: 'research' }],
    });
    const out = openGraphDocument(doc, FILE, []);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.refusal.why).toContain('edges[0]');
  });

  it('a document with no edges field reads as a document with no edges', () => {
    const doc = JSON.stringify({
      schema_version: 1,
      nodes: [{ kind: 'agent', payload: { id: 'research', agent_hash: HASH_A } }],
    });
    const out = openGraphDocument(doc, FILE, []);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.edges).toEqual([]);
  });

  it('takes a free draft name, so an open never lands on an existing draft', () => {
    const taken = [REFUND_SWEEP_DRAFT.name, 'linear'];
    const out = openGraphDocument(cleanDoc(), FILE, taken);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.name).toBe('linear-2');
    expect(out.graph.key).toBe('draft:linear-2');
  });

  it('a paste is named by its own word, not by a file it does not have', () => {
    const out = openGraphDocument(cleanDoc(), { label: 'a pasted document', nameFrom: 'pasted-graph' }, []);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.graph.name).toBe('pasted-graph');
  });
});

describe('freeDraftName', () => {
  it('strips the path and the .json, and keeps the leaf', () => {
    expect(freeDraftName('/tmp/examples/graphs/refund-sweep.JSON', [])).toBe('refund-sweep');
  });

  it('walks the suffix until it finds a name nobody holds', () => {
    expect(freeDraftName('x.json', ['x'])).toBe('x-2');
    expect(freeDraftName('x.json', ['x', 'x-2'])).toBe('x-3');
    expect(freeDraftName('x.json', ['x', 'x-2', 'x-3'])).toBe('x-4');
    // A gap is filled rather than skipped: the walk asks who is taken, not how many exist.
    expect(freeDraftName('x.json', ['x', 'x-3'])).toBe('x-2');
  });

  it('falls back to a plain word when the source has no usable name', () => {
    expect(freeDraftName('.json', [])).toBe('opened-graph');
    expect(freeDraftName('/////', [])).toBe('opened-graph');
    expect(freeDraftName('   ', [])).toBe('opened-graph');
  });

  it('reduces a name to what a picker label and a draft key can carry', () => {
    expect(freeDraftName('my graph (v2)!.json', [])).toBe('my-graph-v2');
    expect(freeDraftName(`${'a'.repeat(80)}.json`, []).length).toBe(48);
  });
});
