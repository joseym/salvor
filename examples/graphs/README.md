# Graph documents

Canonical, language-neutral graph documents for the Salvor v0.4 graph API.
This crate (`salvor-graph`) owns the document format, strict validation, and
JSON Schema emission, and nothing else: it is a pure, IO-free leaf. A graph
document authored here is driven by a separate crate, `salvor-engine`, which
walks a frozen document's nodes and records the walk into a durable run log.
That engine backs `salvor graph run`, `POST /v1/graphs`, and
`POST /v1/graph-runs`; see "Running a graph" below.

## The node kinds

| Kind | Payload it carries | Meaning |
|---|---|---|
| `agent` | `agent_hash` (a `sha256:<64 hex>` string), optional `input_schema` / `output_schema` | A full agent loop, referenced BY CONTENT HASH, never an embedded definition. |
| `tool` | `tool` name, `input` mapping (data), optional schemas | One direct tool invocation, no model in the loop. |
| `gate` | optional `prompt`, `approval_schema` | Human approval that suspends the run. The `approval_schema` is enforced against the resume input before the approval is recorded: a non-conforming approval is refused, nothing is appended, and the run stays parked at the gate. A schema that names `required` or `properties` without a `type` is read as asking for an object, so a bare `null`, number, or string is refused too. |
| `branch` | optional `on`, `cases` (each a `name` + a `when` condition) | Routes on a typed output. Conditions are recorded as DATA and evaluated by the engine at run time, never by this crate. |
| `map` | `over`, `concurrency` cap, `body` (a node id or an embedded sub-graph), optional `output_schema` | Fan-out a sub-run per element of a list. The engine runs iterations inline and sequentially; the `concurrency` cap is accepted and validated but not enforced, a deliberate choice for v0.4. A `subgraph` body, or any body node that is not an `agent` or `tool`, is refused with a typed `UnsupportedMapBody` error rather than driven. |
| `fold` | `body` (a node id or an embedded sub-graph), `max_iterations`, `stop_when` condition, `join` strategy, optional `accumulator_schema` | A bounded iteration loop. The engine drives a `node` body: passes run inline and sequentially, each folding over the previous pass's output, until `stop_when` holds or the bound is reached, and then the `join` rule (`best_by` argmax, `last`, or `all`) picks the value the node produces. A `subgraph` body, or any body node that is not an `agent` or `tool`, is refused with a typed `UnsupportedFoldBody` error rather than driven. |

Edges are the topology: `{ "from": "<node id>", "to": "<node id>" }`, with an
optional `label` (used to name the branch case an edge realizes). Every node
serializes with the adjacent `{"kind": "...", "payload": {...}}` shape the
event log uses, and every field is strict: an unknown key is rejected, not
ignored.

## The examples

| File | Shows |
|---|---|
| [`research-review-publish.json`](research-review-publish.json) | A valid small flow: a research `agent` drafts, a review `agent` checks, a human `gate` approves, a `tool` publishes. Validates clean and runs. |
| [`linear-research-publish.json`](linear-research-publish.json) | A simpler linear flow with no gate: a research `agent` drafts, a review `agent` checks, a `tool` publishes. |
| [`branch-review.json`](branch-review.json) | An `agent` drafts, a `tool` scores it, a `branch` routes on the score: the high case reaches a `gate` then publishes, the low case reaches a rejection `tool` directly. |
| [`branch-model-decision.json`](branch-model-decision.json) | An `agent` drafts, a `tool` scores it, a `branch` carries BOTH an expression case and a `model_decision` case with a well-formed `agent_hash`: the high case reaches a `gate` then publishes, the review case reaches an escalation `tool` directly. The shared fixture the Rust, TypeScript, and Python builders all reduce to for a branch's `agent_hash`. |
| [`fold-refine.json`](fold-refine.json) | A single `fold` node whose body is an `agent`, bounded to 3 iterations with a `stop_when` condition and a `best_by` join. Validates clean, and the engine drives the loop: `tailor` runs once per pass as the fold's per-pass worker, never as a node of its own. Its `best_by` join then needs a `score` inside each pass's value, and an `agent` node's output is the model's reply text, so with a plain text reply the join has nothing to order and refuses (`FoldNoComparableCandidate`) rather than guessing a winner. A `tool` body, whose output is arbitrary JSON, is what carries a scored accumulator today. |
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
builders read:

```
$ salvor graph schema
{ "$defs": { ... }, "properties": { "schema_version": ..., "nodes": ..., "edges": ... } }
```

The same schema is checked in at [`docs/graph-schema.json`](../../docs/graph-schema.json),
generated from the Rust types and gated against drift, so it always describes
the format the binary accepts.

## Editor support

A graph cannot carry a `$schema` key. The format sets `deny_unknown_fields` and
the schema's root is closed to match, so adding one makes the file fail
`salvor graph validate`. That strictness is deliberate: a silently dropped field
could drop a gate or leave a budget unenforced.

Editors are pointed at the schema by file pattern instead, which gives the same
completion, hover text and live validation with no change to any document. This
repository ships the mapping for VS Code in `.vscode/settings.json`:

```json
"json.schemas": [
  { "fileMatch": ["/examples/graphs/*.json"], "url": "./docs/graph-schema.json" }
]
```

A relative path rather than a URL, so it resolves offline and always describes
the schema in your own checkout rather than whichever revision a remote copy
happens to hold. Editing a graph somewhere else, point the same setting at

```
https://raw.githubusercontent.com/joseym/salvor/main/docs/graph-schema.json
```

Other editors take the same idea through their own settings: Neovim and Helix
through a language server such as `vscode-json-languageserver` or `taplo`,
JetBrains IDEs through Settings, Languages and Frameworks, Schemas and DTDs,
JSON Schema Mappings. All that changes is where the pattern and the schema path
are written down.

Schema completion and `salvor graph validate` answer different questions, and
the two deliberately invalid examples show the gap. A schema checks SHAPE, so it
catches a misspelled field or a number where a string belongs. It cannot catch a
cycle or an edge naming a node that does not exist, because both are legal JSON
of the right shape. `invalid-cycle.json` and `invalid-dangling-edge.json` satisfy
the schema and are refused by the validator, which is the division of labour
working rather than a gap in either.

## Running a graph

`salvor graph run` drives a document locally over the store, exactly as
`salvor run` drives a single agent: each `agent` node resolves against a
provided `--agent` file, keyed by that file's computed definition hash, and
each `tool` node resolves from the tools those agents carry.

```
$ salvor graph run examples/graphs/research-review-publish.json \
    --input '{}' \
    --agent agents/research.toml --agent agents/review.toml
```

A `gate` node parks the run the same way a tool suspension does; continue it
with `salvor resume <RUN_ID> --graph examples/graphs/research-review-publish.json --input '{"approved": true}'`.
`salvor fork <RUN_ID> --from-node <NODE> --graph <FILE>` re-walks a run from a
node boundary into a new run.

Over HTTP the same document is submitted with `POST /v1/graphs` and driven
with `POST /v1/graph-runs`; see `crates/salvor-server/API.md`. A submitted
document lives in the server's registry in memory only, so a server restart
drops it and it must be resubmitted before a run or fork can reference it
again; content addressing (the document is keyed by its own hash) makes that
resubmission safe.

A stock `salvor serve` wires an empty tool registry, mirroring how it wires no
demo tools by default: a `tool` node's name resolves against nothing until a
host registers one, so on a default server every `tool` node is refused with
`unknown_tool`. Run `salvor serve --demo-tools` to exercise a `tool` node
end to end, or register real tools through the same mechanism.

## Editing a graph

`salvor graph edit` builds a document one line at a time, reading commands
from stdin with Tab completion. Nothing is saved until a line names a file
(`write <PATH>`), and the only files touched are the ones a line names,
including an agent node's `--file <PATH>`, which is resolved to a definition
hash by building the agent exactly as `salvor agent hash` does. Type `help` at
the prompt for the grammar, and `history` to dump the session as a script that
replays into the identical document.

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

What `validate` does NOT do: check an expression branch case's `when` string
against the grammar below. That string is stored as opaque data by this crate,
so a malformed or wrong expression parses fine as a document and only
surfaces once the engine evaluates it, at run time. The two subsections that
follow exist because of that gap.

## Branch condition expressions

An expression case's `when.value` (the string inside
`{"kind": "expression", "value": "..."}`, as used in `branch-review.json`
above) is a small language defined in
[`crates/salvor-graph/src/expr.rs`](../../crates/salvor-graph/src/expr.rs).
`salvor graph validate` never parses it, so getting the grammar right means
reading it here, not learning it from a `graph run` failure. This is the
grammar, exactly as implemented:

```text
or         := and ( "||" and )*
and        := unary ( "&&" unary )*
unary      := "!" unary | atom
atom       := "(" or ")" | comparison
comparison := operand ( cmp_op operand )?
cmp_op     := "==" | "!=" | "<" | "<=" | ">" | ">="
operand    := literal | path
literal    := string | number | "true" | "false" | "null"
path       := ident ( "." segment )*
segment    := ident | integer
```

- **Operators.** `==`, `!=`, `<`, `<=`, `>`, `>=` compare two operands; `&&`,
  `||`, and unary `!` combine booleans. `||` binds loosest, then `&&`, then
  `!`, then a comparison, so `!score > 0.8` parses as `!(score > 0.8)`, and
  comparisons do not chain (`a < b < c` is a parse error). Parentheses group a
  whole boolean sub-expression, never a comparison operand, so `(a > b) > c`
  is also a parse error.
- **Literals.** A double-quoted string (the only escapes are `\"`, `\\`,
  `\/`, `\n`, `\t`, `\r`), an integer or decimal number (no exponents, an
  optional leading `-`), and the bare keywords `true`, `false`, `null`.
- **Paths.** Dot-separated segments read from the root of the value the
  branch node received as its input (the routed value), for example `score`,
  `output.score`, `items.0.score`. A segment is an object key unless the
  container is an array and the segment is a bare non-negative integer, in
  which case it is a zero-based index; an object key spelled with digits
  (`{"0": ...}`) is therefore never reachable this way. A branch's optional
  `on` field, seen on the node in `branch-review.json`, is a separate,
  opaque display hint this crate stores but never resolves; it plays no part
  in which value an expression's paths are read from.
- **Truthiness.** A bare path or literal used where a boolean is expected
  (`ready`, or `ready && ok`) is true only when it resolves to the JSON
  boolean `true`. Every other value, including a path that does not resolve,
  is false.
- **Unresolved paths.** A path that does not resolve (an absent key, an index
  past the end, a descent into a non-container) is MISSING, distinct from
  JSON `null`. Any comparison touching a missing operand is false, and a
  missing operand in boolean position is false; `!` is the deliberate way to
  test for absence (`!(score > 0.8)` is true when `score` is missing).
- **Equality and ordering.** Equality (`==`, `!=`) is defined across all
  types: values of different types are never equal, so `"5" == 5` is false.
  Ordering (`<`, `<=`, `>`, `>=`) is defined only for two numbers or two
  strings; every other pairing is false. Two numbers compare by mathematical
  value (`1 == 1.0` is true), and two integers compare exactly rather than
  through floating point.

## A branch with no matching case fails the run, not validate

There is no default or else case. When the routed value matches none of a
branch's cases, the run fails at that node with the message
``branch node `<id>`: no case condition matched the routed value``, and
`salvor graph validate` has no way to see this coming, since it never
evaluates a condition against a real value. The branch node itself records no
`NodeEntered` for this failure, but everything upstream of it, including the
graph's entry node, has already run by the time the run fails, and any Write
that node made has already taken effect. The mitigation available today is
authoring cases whose conditions are complementary so one always fires, the
way `branch-review.json` pairs `score >= 0.8` with `score < 0.8`.

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
