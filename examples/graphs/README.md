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

## Follow-up: per-language builders

These files are the canonical, language-neutral form. The planned follow-up is
typed per-language builders (Rust, Python, TypeScript) generated from the
published JSON Schema, so the same graph can be authored in any of the three and
reduce to identical canonical JSON with the same hash, mirroring the
`python-tools/` and `typescript-tools/` layout elsewhere in `examples/`. Those
builders are NOT here yet.
