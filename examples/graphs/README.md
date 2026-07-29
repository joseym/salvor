# Graph documents

Canonical, language-neutral graph documents for the Salvor v0.4 graph API,
plus the two commands that read them. A graph is a declarative CONTROL
document: authored once, submitted, hashed into a run, then frozen. This layer
is FORMAT + VALIDATION only. There is no execution engine yet: nothing here
runs a graph, fans out a `map`, evaluates a `branch` condition, or drives a
`gate`. That comes later.

## The node kinds

| Kind | Payload it carries | Meaning |
|---|---|---|
| `agent` | `agent_hash` (a `sha256:<64 hex>` string), optional `input_schema` / `output_schema` | A full agent loop, referenced BY CONTENT HASH, never an embedded definition. |
| `tool` | `tool` name, `input` mapping (data), optional schemas | One direct tool invocation, no model in the loop. |
| `gate` | optional `prompt`, `approval_schema` | Human approval that suspends the run. |
| `branch` | optional `on`, `cases` (each a `name` + a `when` condition) | Routes on a typed output. Conditions are recorded as DATA and never evaluated here. |
| `map` | `over`, `concurrency` cap, `body` (a node id or an embedded sub-graph), optional `output_schema` | Fan-out a sub-run per element of a list, joined with a concurrency cap. |

Edges are the topology: `{ "from": "<node id>", "to": "<node id>" }`, with an
optional `label` (used to name the branch case an edge realizes). Every node
serializes with the adjacent `{"kind": "...", "payload": {...}}` shape the
event log uses, and every field is strict: an unknown key is rejected, not
ignored.

## The examples

| File | Shows |
|---|---|
| [`research-review-publish.json`](research-review-publish.json) | A valid small flow: a research `agent` drafts, a review `agent` checks, a human `gate` approves, a `tool` publishes. Validates clean. |
| [`invalid-dangling-edge.json`](invalid-dangling-edge.json) | An edge whose target `aprove` is a typo of the node `approve`. Produces a precise dangling-edge error with a nearest-name suggestion. |
| [`invalid-cycle.json`](invalid-cycle.json) | Two agents pointing at each other. Produces a precise cycle error naming the path. |

## Learning an agent's hash first

An `agent` node names its agent by content hash and never by path, so writing
one by hand starts with asking a definition file what its hash is:

```
$ salvor agent hash examples/local-model/agent.toml
sha256:dfaed5fd736c1463e9a9e8cd01f9dea26efafbe97f5cb29c94e5d51f7d2ab222
```

One file prints the bare hash and nothing else, so it reads straight into the
document being authored:

```
$ jq --arg h "$(salvor agent hash examples/local-model/agent.toml)" \
    '.nodes[0].payload.agent_hash = $h' skeleton.json > flow.json
```

Several files each carry their path, since then the question is which hash
belongs to which file:

```
$ salvor agent hash agents/research.toml agents/review.toml
agents/research.toml: sha256:8f...
agents/review.toml: sha256:2c...
```

The hash covers the built definition (model, system prompt, tool schemas,
budgets, pricing), not the file's bytes, which is why an MCP server the file
declares is connected to collect its tool schemas. It is the same string the
run's log records and the same key `graph run` resolves a node against: pass
the file to `graph run --agent` and a node carrying this hash resolves, while
any other hash is refused with the list of hashes the `--agent` files supplied.

## Validating and inspecting

Validate a document (parse strictly, run every check, print a summary or the
precise node/edge errors):

```
$ salvor graph validate examples/graphs/research-review-publish.json
graph ok: 4 node(s), 3 edge(s)
entry:    research
terminal: publish
```

A failure prints every error at once (validation collects them all) and exits
non-zero:

```
$ salvor graph validate examples/graphs/invalid-dangling-edge.json
examples/graphs/invalid-dangling-edge.json: 1 validation error(s):
  - edge `research` -> `aprove` references unknown node id `aprove` (did you mean `approve`?)
```

Print the document JSON Schema, the single source of truth editors and the
future builders read:

```
$ salvor graph schema
{ "$defs": { ... }, "properties": { "schema_version": ..., "nodes": ..., "edges": ... } }
```

## What validation checks

All checks run and every failure is reported (never just the first):

- **Referential integrity.** Every edge endpoint, and every `map` body that
  names a node, must be a real node id. A near miss gets a suggestion.
- **Per-node required fields.** An `agent` hash is a well-formed
  `sha256:<64 hex>` string; a `map` concurrency cap is at least 1; a `gate`
  approval schema is a JSON object.
- **Acyclic.** The edge topology must have no cycle; a cycle is reported as a
  path that closes on itself. This encodes the current acyclic lean and is a
  single isolated check that can be relaxed later.
- **Edge type-compatibility.** Where BOTH endpoints of an edge declare a
  schema, the source's `output_schema` and the target's `input_schema` must be
  structurally identical. Where either is absent, the edge passes unchecked.
  This is deliberately conservative: it does NOT implement JSON Schema
  subtyping, so a compatible-but-not-identical pair is reported as a mismatch.
  Relaxing it to true schema compatibility is a later change to that one check.

## Authoring with the typed builders

These JSON files are the canonical, language-neutral form and the single source
of truth. On top of them sit three typed builders, one per language, that let an
author construct a graph with editor completion and compile-time typing and
reduce to the exact same canonical JSON. Each builder wraps the same format
without inventing a new one.

- **Rust**: `salvor_graph::GraphBuilder` (crate `salvor-graph`).
- **TypeScript**: `GraphBuilder` from `@salvor-run/client` (`sdks/typescript`), zero
  runtime dependencies.
- **Python**: `salvor.GraphBuilder` (`sdks/python`), standard library only.

All three author the `research-review-publish.json` flow below. Side by side:

Rust:

```rust
use salvor_graph::{AgentSpec, GateSpec, GraphBuilder, ToolSpec};
use serde_json::json;

let graph = GraphBuilder::new()
    .agent(AgentSpec::new("research", format!("sha256:{}", "1".repeat(64)))
        .output_schema(draft.clone()))
    .agent(AgentSpec::new("review", format!("sha256:{}", "2".repeat(64)))
        .input_schema(draft.clone()).output_schema(draft))
    .gate(GateSpec::new("approve", approval).prompt("Approve this draft for publication?"))
    .tool(ToolSpec::new("publish", "http_post")
        .input("body", "approve.draft").input("url", "config.publish_url"))
    .edge("research", "review")
    .edge("review", "approve")
    .edge("approve", "publish")
    .build();
```

TypeScript:

```ts
import { GraphBuilder } from "@salvor-run/client";

const graph = new GraphBuilder()
  .agent("research", `sha256:${"1".repeat(64)}`, { outputSchema: draft })
  .agent("review", `sha256:${"2".repeat(64)}`, { inputSchema: draft, outputSchema: draft })
  .gate("approve", approval, { prompt: "Approve this draft for publication?" })
  .tool("publish", "http_post", { input: { body: "approve.draft", url: "config.publish_url" } })
  .edge("research", "review")
  .edge("review", "approve")
  .edge("approve", "publish")
  .build();
```

Python:

```python
from salvor import GraphBuilder

graph = (
    GraphBuilder()
    .agent("research", f"sha256:{'1' * 64}", output_schema=draft)
    .agent("review", f"sha256:{'2' * 64}", input_schema=draft, output_schema=draft)
    .gate("approve", approval, prompt="Approve this draft for publication?")
    .tool("publish", "http_post", input={"body": "approve.draft", "url": "config.publish_url"})
    .edge("research", "review")
    .edge("review", "approve")
    .edge("approve", "publish")
    .build()
)
```

A runnable authoring example lives with each SDK:
`sdks/typescript/example/build_graph.ts`, `sdks/python/example/build_graph.py`,
and a doctest in `crates/salvor-graph/src/builder.rs`. Each constructs the flow
above; the two SDK examples print the JSON so you can pipe it straight into
`salvor graph validate`.

### Type-checking vs semantic validation

The builders draw a firm line. TYPES catch STRUCTURAL mistakes at author time:
a field that belongs to one node kind is not reachable on another, a required
field cannot be omitted, and the adjacent `kind`/`payload` shape is not
something you assemble by hand. A structurally malformed document is a type
error, so it never reaches the wire.

SEMANTIC rules are NOT the builders' job. Whether an agent hash is 64 hex
digits, whether a `map` concurrency is positive, whether every edge names a real
node, and whether the graph is acyclic are all checked by `salvor graph
validate`, which runs over the emitted JSON exactly as it runs over a
hand-written file. Building produces a well-shaped document; validation
determines whether it is a legal one.
