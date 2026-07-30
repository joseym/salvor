import { describe, expect, it } from 'vitest';

import {
  addEdge,
  addNode,
  deleteNode,
  edgesTouching,
  freeNodeId,
  removeCase,
  removeEdgeAt,
  setCase,
  setEdgeCase,
} from './wf-edit';
import { type WfGraph, toServerDocument } from './wf-model';
import { validateGraph } from './wf-validate';

const EMPTY: WfGraph = { key: 'draft:blank', hash: null, name: 'blank', state: 'draft', nodes: [], edges: [] };
const FULL_HASH = `sha256:${'0123456789abcdef'.repeat(4)}`;

/** Codes only: what the one judge says about a graph, in the order it says it. */
function codes(g: WfGraph): string[] {
  return validateGraph(g).map((e) => e.code);
}

describe('adding a node', () => {
  it('takes the lowest free ordinal for its kind, so ids do not climb as work is undone', () => {
    const one = addNode(EMPTY, 'tool');
    expect(one.id).toBe('tool_1');
    const two = addNode(one.graph, 'tool');
    expect(two.id).toBe('tool_2');
    // drop the first: the next tool takes the name that was freed rather than tool_3
    expect(freeNodeId(deleteNode(two.graph, 'tool_1'), 'tool')).toBe('tool_1');
  });

  it('lands a node the canvas can read and the document can carry', () => {
    const { graph, id } = addNode(EMPTY, 'gate');
    const node = graph.nodes.find((n) => n.id === id);
    expect(node?.kind).toBe('gate');
    expect(toServerDocument(graph).nodes[0]).toEqual({
      kind: 'gate',
      payload: { id: 'gate_1', approval_schema: { type: 'object' } },
    });
  });
});

describe('deleting a node leaves its edges behind, on purpose', () => {
  const built = (() => {
    let g = addNode(EMPTY, 'agent').graph;
    g = addNode(g, 'tool').graph;
    g = addNode(g, 'gate').graph;
    g = addEdge(g, 'agent_1', 'tool_1');
    g = addEdge(g, 'tool_1', 'gate_1');
    return g;
  })();

  it('says how many edges a delete would orphan, before it happens', () => {
    expect(edgesTouching(built, 'tool_1')).toBe(2);
    expect(edgesTouching(built, 'agent_1')).toBe(1);
  });

  it('turns every edge that named the deleted node into a dangling-edge error', () => {
    const after = deleteNode(built, 'tool_1');
    expect(after.edges).toHaveLength(2); // the edges are still in the document
    const dangling = validateGraph(after).filter((e) => e.code === 'dangling_edge');
    expect(dangling).toHaveLength(2);
    expect(dangling.map((e) => e.msg).join(' ')).toContain('tool_1');
  });

  it('a map body that named the deleted node becomes a dangling-body error, not a silent nothing', () => {
    let g = addNode(EMPTY, 'tool').graph;
    const added = addNode(g, 'map');
    g = added.graph;
    g = {
      ...g,
      nodes: g.nodes.map((n) => (n.id === 'map_1' ? { ...n, body: { node: 'tool_1' }, over: 'items' } : n)),
    };
    expect(codes(g)).toEqual([]);
    expect(codes(deleteNode(g, 'tool_1'))).toEqual(['dangling_body']);
  });
});

describe('edges', () => {
  const two = addNode(addNode(EMPTY, 'agent').graph, 'tool').graph;

  it('draws one, and refuses to draw the identical one twice', () => {
    const once = addEdge(two, 'agent_1', 'tool_1');
    expect(once.edges).toEqual([{ from: 'agent_1', to: 'tool_1' }]);
    expect(addEdge(once, 'agent_1', 'tool_1').edges).toHaveLength(1);
    // a different case IS a different edge
    expect(addEdge(once, 'agent_1', 'tool_1', 'x').edges).toHaveLength(2);
  });

  it('cuts by position, leaving the rest of the document alone', () => {
    let g = addEdge(two, 'agent_1', 'tool_1');
    g = addEdge(g, 'tool_1', 'agent_1');
    expect(removeEdgeAt(g, 0).edges).toEqual([{ from: 'tool_1', to: 'agent_1' }]);
    expect(removeEdgeAt(g, 0).nodes).toEqual(g.nodes);
  });

  it('clearing a case OMITS the label rather than setting it empty', () => {
    const g = setEdgeCase(addEdge(two, 'agent_1', 'tool_1', 'x'), 0, undefined);
    expect(g.edges[0]).toEqual({ from: 'agent_1', to: 'tool_1' });
    expect('label' in g.edges[0]).toBe(false);
  });
});

describe('branch cases', () => {
  const withBranch = addNode(addNode(EMPTY, 'branch').graph, 'tool').graph;

  it('adds a case with its condition, and replaces rather than repeats a name', () => {
    let g = setCase(withBranch, 'branch_1', 'over_500', { kind: 'expression', value: 'amount >= 500' });
    g = setCase(g, 'branch_1', 'over_500', { kind: 'expression', value: 'amount > 500' });
    const branch = g.nodes.find((n) => n.id === 'branch_1');
    expect(branch?.cases).toEqual(['over_500']);
    expect(toServerDocument(g).nodes[0].payload).toMatchObject({
      cases: [{ name: 'over_500', when: { kind: 'expression', value: 'amount > 500' } }],
    });
  });

  it('a model-decided case is one the validator asks for an agent for', () => {
    const g = setCase(withBranch, 'branch_1', 'ask', { kind: 'model_decision' });
    expect(g.nodes.find((n) => n.id === 'branch_1')?.modelCases).toEqual(['ask']);
    expect(codes(g)).toContain('model_decision_without_agent');
  });

  it('an unrealized case is an error until an edge carries it', () => {
    const declared = setCase(withBranch, 'branch_1', 'yes', { kind: 'expression', value: 'ok' });
    expect(codes(declared)).toEqual(['unrealized_case']);
    expect(codes(addEdge(declared, 'branch_1', 'tool_1', 'yes'))).toEqual([]);
  });

  it('dropping a case leaves the edge that realized it, and the edge says so', () => {
    let g = setCase(withBranch, 'branch_1', 'yes', { kind: 'expression', value: 'ok' });
    g = addEdge(g, 'branch_1', 'tool_1', 'yes');
    expect(codes(g)).toEqual([]);
    const after = removeCase(g, 'branch_1', 'yes');
    expect(after.edges).toHaveLength(1);
    expect(codes(after)).toEqual(['edge_type']);
  });
});

describe('a graph drawn entirely from nothing', () => {
  it('reaches a document the validator passes, and writes it in the format s own shape', () => {
    let g = addNode(EMPTY, 'agent').graph;
    g = { ...g, nodes: g.nodes.map((n) => (n.id === 'agent_1' ? { ...n, agentHash: FULL_HASH, payload: { id: 'agent_1', agent_hash: FULL_HASH } } : n)) };
    g = addNode(g, 'tool').graph;
    g = addNode(g, 'branch').graph;
    g = addNode(g, 'gate').graph;
    g = addEdge(g, 'agent_1', 'branch_1');
    g = setCase(g, 'branch_1', 'small', { kind: 'expression', value: 'amount < 500' });
    g = setCase(g, 'branch_1', 'large', { kind: 'expression', value: 'amount >= 500' });
    g = addEdge(g, 'branch_1', 'tool_1', 'small');
    g = addEdge(g, 'branch_1', 'gate_1', 'large');

    expect(codes(g)).toEqual([]);
    const doc = toServerDocument(g);
    expect(doc.schema_version).toBe(1);
    expect(doc.nodes.map((n) => n.kind)).toEqual(['agent', 'tool', 'branch', 'gate']);
    expect(doc.edges).toEqual([
      { from: 'agent_1', to: 'branch_1' },
      { from: 'branch_1', to: 'tool_1', label: 'small' },
      { from: 'branch_1', to: 'gate_1', label: 'large' },
    ]);
    // no node carries a display name: none was typed, and the id is not one
    expect(doc.nodes.every((n) => !('name' in n.payload))).toBe(true);
  });
});

describe('publishing a document the canvas opened emits that document', () => {
  it('keeps the fields the canvas does not model, rather than dropping them', () => {
    const opened: WfGraph = {
      key: 'draft:opened',
      hash: null,
      name: 'opened',
      state: 'draft',
      nodes: [
        {
          id: 'refine',
          kind: 'fold',
          name: 'refine',
          maxIterations: 3,
          stopWhen: 'score >= 0.85',
          join: 'keeps the best by score',
          body: { node: 'refine' },
          payload: {
            id: 'refine',
            body: { kind: 'node', value: 'refine' },
            max_iterations: 3,
            stop_when: 'score >= 0.85',
            join: { kind: 'best_by', value: 'score' },
            accumulator_schema: { type: 'object' },
          },
        },
      ],
      edges: [],
    };
    // The join rule and the accumulator schema are fields the inspector never shows, and the join is
    // held in the view as prose. Publishing must still write the document that was opened.
    expect(toServerDocument(opened).nodes[0].payload).toEqual({
      id: 'refine',
      body: { kind: 'node', value: 'refine' },
      max_iterations: 3,
      stop_when: 'score >= 0.85',
      join: { kind: 'best_by', value: 'score' },
      accumulator_schema: { type: 'object' },
    });
  });
});
