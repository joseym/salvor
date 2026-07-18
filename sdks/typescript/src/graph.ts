/**
 * A typed builder for Salvor graph documents.
 *
 * The canonical format lives in Rust (`crates/salvor-graph`): a strict,
 * adjacently tagged document that `salvor graph validate` accepts or rejects.
 * This module mirrors that format in TypeScript interfaces and adds a builder
 * so a TypeScript author gets editor completion and compile-time typing while
 * producing the exact same wire JSON. It invents no new format: whatever
 * {@link GraphBuilder.build} returns is the same document a hand-written file
 * would parse to.
 *
 * The types make a STRUCTURALLY malformed document hard to write: each node
 * kind has its own payload interface, and the builder method for a kind demands
 * that kind's required fields. SEMANTIC checks (hash shape, referential
 * integrity, acyclicity) are not done here; they stay with
 * `salvor graph validate`, run over the emitted JSON.
 *
 * Zero runtime dependencies: everything here is plain objects and one class.
 *
 * ## The optional node display name
 *
 * Every node payload may carry an optional `name`: a short, purely
 * presentational label ("Approve the draft"). Bounds mirror the agent
 * definition's own `name` precedent: at most 64 characters, and, when set,
 * not empty or all whitespace — `salvor graph validate` (and `POST
 * /v1/graphs`) enforce both, node-precise. Unlike an agent's `name` (excluded
 * from its definition hash so a rename never mints a new agent identity), a
 * node's `name` is an ordinary field on the payload and hashes like any
 * other: a graph document IS its content hash, so renaming a node is
 * authoring a new document version, by design.
 */

/** Any JSON value. Used for the schema fields a node may declare. */
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

/** The `agent` node payload: a full agent loop referenced by content hash. */
export interface AgentPayload {
  id: string;
  agent_hash: string;
  /** Optional short display label. See the module docs' "The optional node display name" note. */
  name?: string;
  input_schema?: JsonValue;
  output_schema?: JsonValue;
}

/** The `tool` node payload: one direct tool invocation. */
export interface ToolPayload {
  id: string;
  tool: string;
  /** Optional short display label. See the module docs' "The optional node display name" note. */
  name?: string;
  input?: Record<string, string>;
  input_schema?: JsonValue;
  output_schema?: JsonValue;
}

/** The `gate` node payload: human approval that suspends the run. */
export interface GatePayload {
  id: string;
  /** Optional short display label. See the module docs' "The optional node display name" note. */
  name?: string;
  prompt?: string;
  approval_schema: JsonValue;
}

/** How a branch case is selected. Adjacently tagged, data only. */
export type BranchCondition =
  | { kind: "expression"; value: string }
  | { kind: "model_decision" };

/** One case of a branch node: a name and the condition that selects it. */
export interface BranchCase {
  name: string;
  when: BranchCondition;
}

/** The `branch` node payload: routes on a typed output. */
export interface BranchPayload {
  id: string;
  /** Optional short display label. See the module docs' "The optional node display name" note. */
  name?: string;
  on?: string;
  cases: BranchCase[];
}

/** The body a map node maps each element through. Adjacently tagged. */
export type MapBody =
  | { kind: "node"; value: string }
  | { kind: "subgraph"; value: Graph };

/** The `map` node payload: fan-out a sub-run per element of a typed list. */
export interface MapPayload {
  id: string;
  /** Optional short display label. See the module docs' "The optional node display name" note. */
  name?: string;
  over: string;
  concurrency: number;
  body: MapBody;
  output_schema?: JsonValue;
}

/**
 * One node: exactly one of the five kinds, adjacently tagged as
 * `{ kind, payload }`, mirroring the Rust enum's wire shape.
 */
export type GraphNode =
  | { kind: "agent"; payload: AgentPayload }
  | { kind: "tool"; payload: ToolPayload }
  | { kind: "gate"; payload: GatePayload }
  | { kind: "branch"; payload: BranchPayload }
  | { kind: "map"; payload: MapPayload };

/** A directed edge. An optional label names a branch case an edge realizes. */
export interface Edge {
  from: string;
  to: string;
  label?: string;
}

/** A graph document: schema version, nodes, and the edges connecting them. */
export interface Graph {
  schema_version: number;
  nodes: GraphNode[];
  edges: Edge[];
}

/** The schema version this build stamps onto every document it writes. */
export const SCHEMA_VERSION = 1;

/** The optional fields an agent node may declare. */
export interface AgentOptions {
  name?: string;
  inputSchema?: JsonValue;
  outputSchema?: JsonValue;
}

/** The optional fields a tool node may declare. */
export interface ToolOptions {
  name?: string;
  input?: Record<string, string>;
  inputSchema?: JsonValue;
  outputSchema?: JsonValue;
}

/** The optional fields a gate node may declare. */
export interface GateOptions {
  name?: string;
  prompt?: string;
}

/** The optional fields a branch node may declare. */
export interface BranchOptions {
  name?: string;
  on?: string;
}

/** The optional fields a map node may declare. */
export interface MapOptions {
  name?: string;
  outputSchema?: JsonValue;
}

/**
 * Accumulates nodes and edges, then freezes them into a {@link Graph}.
 *
 * Each node method takes the id plus that kind's required fields, then an
 * options object for the optional ones. An unset optional field is left OFF the
 * emitted object entirely (never set to `undefined` or `null`), so the JSON
 * matches the canonical document byte for byte after normalization. Every
 * method returns `this`, so a whole document reads as one chain ending in
 * {@link GraphBuilder.build}.
 */
export class GraphBuilder {
  private readonly nodes: GraphNode[] = [];
  private readonly edges: Edge[] = [];

  /** Adds an `agent` node, referenced by its `sha256:<64 hex>` hash. */
  agent(id: string, agentHash: string, options: AgentOptions = {}): this {
    const payload: AgentPayload = { id, agent_hash: agentHash };
    if (options.name !== undefined) payload.name = options.name;
    if (options.inputSchema !== undefined) payload.input_schema = options.inputSchema;
    if (options.outputSchema !== undefined) payload.output_schema = options.outputSchema;
    this.nodes.push({ kind: "agent", payload });
    return this;
  }

  /** Adds a `tool` node for one direct invocation of a registered tool. */
  tool(id: string, tool: string, options: ToolOptions = {}): this {
    const payload: ToolPayload = { id, tool };
    if (options.name !== undefined) payload.name = options.name;
    if (options.input !== undefined) payload.input = options.input;
    if (options.inputSchema !== undefined) payload.input_schema = options.inputSchema;
    if (options.outputSchema !== undefined) payload.output_schema = options.outputSchema;
    this.nodes.push({ kind: "tool", payload });
    return this;
  }

  /** Adds a `gate` node. The approval schema is required. */
  gate(id: string, approvalSchema: JsonValue, options: GateOptions = {}): this {
    const payload: GatePayload = { id, approval_schema: approvalSchema };
    if (options.name !== undefined) payload.name = options.name;
    if (options.prompt !== undefined) payload.prompt = options.prompt;
    this.nodes.push({ kind: "gate", payload });
    return this;
  }

  /** Adds a `branch` node with its named cases. */
  branch(id: string, cases: BranchCase[], options: BranchOptions = {}): this {
    const payload: BranchPayload = { id, cases };
    if (options.name !== undefined) payload.name = options.name;
    if (options.on !== undefined) payload.on = options.on;
    this.nodes.push({ kind: "branch", payload });
    return this;
  }

  /** Adds a `map` node with its fan-out reference, cap, and body. */
  map(
    id: string,
    over: string,
    concurrency: number,
    body: MapBody,
    options: MapOptions = {},
  ): this {
    const payload: MapPayload = { id, over, concurrency, body };
    if (options.name !== undefined) payload.name = options.name;
    if (options.outputSchema !== undefined) payload.output_schema = options.outputSchema;
    this.nodes.push({ kind: "map", payload });
    return this;
  }

  /** Adds an edge. Pass a `label` to name the branch case it realizes. */
  edge(from: string, to: string, label?: string): this {
    const edge: Edge = { from, to };
    if (label !== undefined) edge.label = label;
    this.edges.push(edge);
    return this;
  }

  /**
   * Freezes the accumulated nodes and edges into a {@link Graph}, stamping the
   * current {@link SCHEMA_VERSION}. Structure only: pipe the result through
   * `salvor graph validate` for the semantic checks.
   */
  build(): Graph {
    return {
      schema_version: SCHEMA_VERSION,
      nodes: this.nodes,
      edges: this.edges,
    };
  }
}
