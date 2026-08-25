import { SCHEMA_VERSION, type BranchCase, type Graph, type GraphNode } from '@salvor-run/client';

import type { GraphSummary } from '../../core/api';

/**
 * The canvas's internal graph model: one flat shape both a SERVER graph (`GET /v1/graphs/{hash}`,
 * a {@link Graph} document) and an in-browser DRAFT normalise into, so the renderer, the picker,
 * the topological layout, the validator and the node menu all read one thing. Ported from the
 * prototype's `GRAPHS` fixture shape (id/kind/name per node, from/to/label per edge, a nullable
 * hash that IS the version once published), but sourced from the real control plane rather than a
 * canned object.
 */
export type WfNodeKind = GraphNode['kind'];

/** A tool node's declared effect class: the field fork safety turns on. */
export type WfEffect = 'read' | 'write' | 'idempotent';

export interface WfNode {
  readonly id: string;
  readonly kind: WfNodeKind;
  /** A human label. A server document's payload may carry its own `name` (the graph format's
   * optional node display name); when it does not, the id is the honest fallback: never an
   * invented sentence. A draft always carries an author-typed name (possibly still the id, if
   * never edited). */
  readonly name: string;
  // Kind-specific payload, optional because each field belongs to exactly one kind. The node
  // inspector panel and the validator read these; a field a document does not carry stays
  // undefined rather than defaulted.
  readonly agentHash?: string;
  readonly tool?: string;
  readonly effect?: string;
  readonly idempotencyKey?: string | null;
  readonly input?: unknown;
  readonly prompt?: string;
  readonly inputSchema?: unknown;
  readonly cases?: readonly string[];
  /** The subset of a branch's `cases` whose condition is `model_decision` rather than an
   * expression: a case the engine resolves by driving the branch's own `agentHash` at run time,
   * never by evaluating a predicate. Undefined (or empty) means every case is expression-decided,
   * so the validator's `model_decision_without_agent` check has nothing to require an agent for. */
  readonly modelCases?: readonly string[];
  readonly over?: string;
  readonly concurrency?: number;
  readonly body?: { readonly tool?: string; readonly effect?: string; readonly node?: string };
  // Fold-specific payload: the iteration bound, the stop predicate, and a short
  // label for the join rule ("best by score", "last", "all").
  readonly maxIterations?: number;
  readonly stopWhen?: string;
  readonly join?: string;
  /** Delay-specific payload: how long the run waits, in whole seconds, before the walk continues. */
  readonly seconds?: number;
  /**
   * THE DOCUMENT PAYLOAD this node is a reading of, carried verbatim.
   *
   * The fields above are a flattened VIEW: what the canvas draws, marks and edits. The format
   * carries more than that (declared input/output schemas, a tool's input mapping, an accumulated
   * value's schema, a subgraph body), and a view that dropped them would publish a DIFFERENT
   * document than the one it opened, quietly, under a new hash the author would have no way to
   * account for. So the payload it was read from rides along and is what publish emits, with the
   * canvas overwriting only the fields it genuinely edited. A node the palette minted carries the
   * payload the SDK's builder produced for it, so a drawn node and an opened one are the same kind
   * of thing.
   */
  readonly payload?: Readonly<Record<string, unknown>>;
}

export interface WfEdge {
  readonly from: string;
  readonly to: string;
  readonly label?: string;
}

export interface WfGraph {
  /** The picker's option value and the app's local id. A draft's key is `draft:<name>`; a
   * published graph's key IS its hash. */
  readonly key: string;
  /** `null` until published: a draft has no identity, because the hash IS the version. */
  readonly hash: string | null;
  readonly name: string;
  readonly state: 'draft' | 'published';
  readonly nodes: readonly WfNode[];
  readonly edges: readonly WfEdge[];
}

/** The id of a server graph node: every kind's payload carries `id` (see `@salvor-run/client` graph). */
function nodeId(n: GraphNode): string {
  return n.payload.id;
}

/** One line of plain prose: what a node DOES, not what kind it is. Guarded for the fields a
 * server document may omit. */
export function nodeDoes(n: WfNode): string {
  switch (n.kind) {
    case 'agent':
      return `runs the agent at ${String(n.agentHash ?? '').slice(0, 13)}…`;
    case 'tool':
      return `calls ${n.tool ?? 'a tool'}`;
    case 'gate':
      return n.prompt ?? 'waits for a human approval';
    case 'branch':
      return `picks one of ${n.cases?.length ?? 0} cases`;
    case 'map':
      return `fans out over a list, ${n.concurrency ?? 0} at a time`;
    case 'fold':
      return `iterates up to ${n.maxIterations ?? 0} times, then ${n.join ?? 'joins'}`;
    case 'delay':
      return `waits ${n.seconds ?? 0}s, then continues`;
  }
}

/** Read one DOCUMENT node into the canvas model, keeping every kind-specific payload field the panel
 * or validator reads, and the whole payload besides (see {@link WfNode.payload}: the fields below are
 * a view, not a replacement). A node payload may carry its own optional `name`; when it does, the
 * node reads by that display name, and when it does not, the id is the honest fallback (never an
 * invented sentence). The one path from the format into the canvas: a document opened off disk, a
 * graph read from the server and a node the palette mints all arrive through here. */
export function documentNode(n: GraphNode): WfNode {
  const base = {
    id: nodeId(n),
    kind: n.kind,
    name: n.payload.name ?? nodeId(n),
    // An interface with named fields is not an index signature, so the widening is spelled out: a
    // payload IS a JSON object, and this is the one place the canvas holds it as one.
    payload: n.payload as unknown as Readonly<Record<string, unknown>>,
  } as const;
  switch (n.kind) {
    case 'agent':
      return { ...base, agentHash: n.payload.agent_hash };
    case 'tool':
      return {
        ...base,
        tool: n.payload.tool,
        ...(n.payload.input !== undefined ? { input: n.payload.input } : {}),
      };
    case 'gate':
      return {
        ...base,
        ...(n.payload.prompt !== undefined ? { prompt: n.payload.prompt } : {}),
        inputSchema: n.payload.approval_schema,
      };
    case 'branch':
      return {
        ...base,
        cases: n.payload.cases.map((c) => c.name),
        // A model-decided case is realized by an agent's judgement rather than by an edge label, so
        // the validator has to know WHICH cases those are and which agent the node names for them.
        // Without both, the `model_decision_without_agent` check has nothing to fire on and a branch
        // that promised a decision with nobody behind it would publish and be refused by the server.
        modelCases: n.payload.cases.filter((c) => c.when.kind === 'model_decision').map((c) => c.name),
        ...branchAgentHash(n.payload),
      };
    case 'map':
      return {
        ...base,
        over: n.payload.over,
        concurrency: n.payload.concurrency,
        body: n.payload.body.kind === 'node' ? { node: n.payload.body.value } : {},
      };
    case 'fold':
      return {
        ...base,
        maxIterations: n.payload.max_iterations,
        stopWhen: n.payload.stop_when,
        join: joinLabel(n.payload.join),
        body: n.payload.body.kind === 'node' ? { node: n.payload.body.value } : {},
      };
    case 'delay':
      return { ...base, seconds: n.payload.seconds };
  }
}

/** The `agent_hash` a branch names for its model-decided cases, when it names one: optional on the
 * node, so its ABSENCE is a fact about the document rather than a missing read. Taken off the payload
 * value rather than a declared field, which keeps this true of any document that carries one. */
function branchAgentHash(payload: object): { agentHash?: string } {
  const hash = (payload as { agent_hash?: unknown }).agent_hash;
  return typeof hash === 'string' ? { agentHash: hash } : {};
}

/**
 * One node with part of its DOCUMENT payload rewritten. Every edit to a field the format carries goes
 * through here, so the flattened view the canvas renders and the payload publish emits are written in
 * the same expression and cannot drift apart. A patch value of `undefined` OMITS the key, which is
 * how the format spells an unset optional field: absent, never present-and-empty.
 */
export function withDocFields(n: WfNode, patch: Readonly<Record<string, unknown>>): WfNode {
  const payload: Record<string, unknown> = { ...(n.payload ?? {}) };
  for (const [key, value] of Object.entries(patch)) {
    if (value === undefined) delete payload[key];
    else payload[key] = value;
  }
  return { ...n, payload };
}

/**
 * A branch's cases as the DOCUMENT carries them: a name and the condition that selects it. The
 * flattened {@link WfNode.cases}/{@link WfNode.modelCases} pair is what the canvas and the validator
 * read; this is the one function that turns it back into the format's own shape, so the two can
 * never disagree about which case is model-decided. A node read from a document answers off its
 * payload; a node built by hand (a seeded fixture) is reconstructed from the flattened view, which
 * is lossless for everything that view can express.
 */
export function branchCases(n: WfNode): readonly BranchCase[] {
  const fromPayload = (n.payload as { cases?: unknown } | undefined)?.cases;
  if (Array.isArray(fromPayload)) return fromPayload as readonly BranchCase[];
  const model = new Set(n.modelCases ?? []);
  return (n.cases ?? []).map((name) =>
    model.has(name)
      ? { name, when: { kind: 'model_decision' } as const }
      : { name, when: { kind: 'expression', value: '' } as const },
  );
}

/** The same branch with a new case list: the payload and the flattened view rewritten together,
 * from the one list, so a case can never be present in one and absent from the other. */
export function withBranchCases(n: WfNode, cases: readonly BranchCase[]): WfNode {
  return withDocFields(
    {
      ...n,
      cases: cases.map((c) => c.name),
      modelCases: cases.filter((c) => c.when.kind === 'model_decision').map((c) => c.name),
    },
    { cases: [...cases] },
  );
}

/** A short prose label for a fold's join rule, for the node's `does` line and
 * the inspector. */
function joinLabel(join: { kind: string; value?: string }): string {
  switch (join.kind) {
    case 'best_by':
      return `keeps the best by ${join.value ?? ''}`.trimEnd();
    case 'last':
      return 'takes the last pass';
    case 'all':
      return 'collects every pass';
    default:
      return 'joins the passes';
  }
}

/**
 * Normalise a graph DOCUMENT's body into the canvas model: the part that is identical whether the
 * document arrived off the wire or out of a file, because a document does not carry its own
 * identity: the hash is computed from the bytes, and only the server has computed it. Callers
 * supply the identity (or the honest absence of one), which is what keeps an opened file a draft.
 */
export function documentBody(doc: Graph): { nodes: WfNode[]; edges: WfEdge[] } {
  return {
    nodes: doc.nodes.map(documentNode),
    edges: doc.edges.map((e) =>
      e.label !== undefined ? { from: e.from, to: e.to, label: e.label } : { from: e.from, to: e.to },
    ),
  };
}

/**
 * THE BODY LINKS a canvas draws: for every map or fold node whose payload names another node as its
 * body (`body.node`, a node id STRING), a link from that node to the one the id resolves to. These
 * are NOT document edges. The format references a body by id inside the node's own payload, never
 * through the edge list, so a link here is a pure RENDERING affordance: it is derived from the nodes,
 * kept out of `g.edges`, and therefore out of everything that reads the edge list as topology (the
 * topological order, the layered columns, the run projection, the fork reasoning). An embedded
 * sub-graph body carries no string id and yields no link; a body naming a node this document does
 * not hold yields none either, so a link always has both ends to draw between. Pure and DOM-free, so
 * the layout and the renderer derive one and the same set.
 */
export function bodyLinks(g: WfGraph): WfEdge[] {
  const ids = new Set(g.nodes.map((n) => n.id));
  const links: WfEdge[] = [];
  for (const n of g.nodes) {
    const target = n.body?.node;
    if (typeof target === 'string' && target.length > 0 && ids.has(target)) {
      links.push({ from: n.id, to: target });
    }
  }
  return links;
}

/**
 * Normalise a stored server document into the canvas model. The `name` shown in the picker is the
 * short hash: a published graph's identity is its hash.
 */
export function fromServerGraph(hash: string, doc: Graph, summary?: GraphSummary): WfGraph {
  const { nodes, edges } = documentBody(doc);
  void summary;
  return { key: hash, hash, name: shortHash(hash), state: 'published', nodes, edges };
}

/**
 * Convert a canvas draft back to a document for publish: `{ kind, payload }` per node, exactly the
 * format's own envelope.
 *
 * THE PAYLOAD IS THE NODE'S OWN (see {@link WfNode.payload}), which is what makes publishing a graph
 * the canvas opened emit that graph rather than a lossy re-description of it: a declared input or
 * output schema, a tool's input mapping, an accumulator schema, a subgraph body all survive, because
 * they were never taken apart. The canvas's edits are already in that payload, written there by the
 * same expression that wrote the flattened field (`withDocFields`).
 *
 * Two things are applied over it. The `id`, because the one edit that changes an id is the
 * duplicate-rename fix, which renames the node rather than the payload. And the display name, which
 * is present exactly when the node has one to state: a name equal to the id is what the canvas shows
 * for a node the document never named, and writing it back would add a field the author never set and
 * mint a different hash for the same graph.
 *
 * {@link requiredPayload} sits UNDER the payload as a floor, never over it: a node built by hand
 * rather than read from a document (a seeded fixture) carries no payload, and a document missing a
 * field the format requires is not something to hand to the server.
 */
export function toServerDocument(g: WfGraph): Graph {
  const nodes = g.nodes.map(
    (n) =>
      ({
        kind: n.kind,
        payload: {
          ...requiredPayload(n),
          ...(n.payload ?? {}),
          id: n.id,
          ...(n.payload?.['name'] !== undefined || n.name !== n.id ? { name: n.name } : {}),
        },
      }) as GraphNode,
  );
  const edges = g.edges.map((e) => (e.label !== undefined ? { from: e.from, to: e.to, label: e.label } : { from: e.from, to: e.to }));
  return { schema_version: SCHEMA_VERSION, nodes, edges };
}

/**
 * The floor under a node's payload: every field its kind REQUIRES, at the emptiest value that is
 * still of the right type. Only reached for a field a node carries no payload for at all, which is a
 * hand-built draft, never a document that was read; a value here is a placeholder the validator
 * already refuses (an empty hash, a bodiless body), never a plausible-looking invention that could
 * publish unnoticed.
 */
function requiredPayload(n: WfNode): Record<string, unknown> {
  switch (n.kind) {
    case 'agent':
      return { id: n.id, agent_hash: n.agentHash ?? '' };
    case 'tool':
      return { id: n.id, tool: n.tool ?? '' };
    case 'gate':
      return { id: n.id, approval_schema: n.inputSchema ?? { type: 'object' } };
    case 'branch':
      return { id: n.id, cases: [...branchCases(n)] };
    case 'map':
      return {
        id: n.id,
        over: n.over ?? '',
        concurrency: n.concurrency ?? 1,
        body: { kind: 'node', value: n.body?.node ?? '' },
      };
    case 'fold':
      return {
        id: n.id,
        body: { kind: 'node', value: n.body?.node ?? '' },
        max_iterations: n.maxIterations ?? 1,
        stop_when: n.stopWhen ?? 'false',
        join: { kind: 'last' },
      };
    case 'delay':
      // 1 second: the smallest wait the validator accepts, the same floor `map`'s worker count
      // and `fold`'s iteration bound start at above.
      return { id: n.id, seconds: n.seconds ?? 1 };
  }
}

/** `sha256:4f1c8ab3…`: the picker/name face of a published graph. */
export function shortHash(hash: string): string {
  return 'sha256:' + hash.replace(/^sha256:/, '').slice(0, 8);
}

/** One picker option: the value the `<select>` carries and the label it shows. Drafts first (the
 * author's own work), then published server graphs, each by its short hash. */
export interface WfPickOption {
  readonly value: string;
  readonly label: string;
  readonly state: 'draft' | 'published';
}

export function pickOptions(drafts: readonly WfGraph[], server: readonly WfGraph[]): WfPickOption[] {
  const d = drafts.map((g) => ({ value: g.key, label: g.name, state: g.state }));
  const s = server.map((g) => ({ value: g.key, label: g.name, state: g.state }));
  return [...d, ...s];
}
