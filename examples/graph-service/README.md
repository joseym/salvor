# graph-service: a refund desk, as a Salvor graph

An invoice dispute arrives. Something has to look it up, decide whether a human
needs to sign off, get that signature when the amount is large enough, move the
money, and tell the customer. That is the whole of this example, written as one
graph document ([`dispute-refund.json`](dispute-refund.json)) that Salvor walks
and records.

It is a working desk rather than a research demo: the branch routes on the
disputed amount that a system of record returned, the gate is a real human
approval that parks the run durably, and the refund is a genuine append to a
ledger file that the durability proof below counts lines in.

Everything here runs offline. A scripted model server (`salvor-demo-model`)
stands in for a real endpoint, so no API key and no network are needed, and the
tools are one Python MCP server ([`server.py`](server.py)) that depends on
nothing but the standard library.

## The graph, node by node

```
pull_dispute ──▶ route_by_amount ──escalate──▶ approve_refund ──▶ settle ──▶ notify
                       │
                       └────────auto_settle──▶ small_claims
```

- **`pull_dispute`** (`tool`, `lookup_dispute`, Read). The entry node. Its input
  is the graph input, `{"dispute_id": "DSP-4471"}`, and it resolves that id
  against the desk's records: the invoice, the customer, the disputed amount and
  the stated reason. A Read is freely retryable, so an interrupted lookup just
  re-reads.
- **`route_by_amount`** (`branch`). Two cases, both **expressions**, evaluated in
  author order against the record the lookup returned:
  `structuredContent.amount_usd >= 250.0` routes to `escalate`,
  `structuredContent.amount_usd < 250.0` routes to `auto_settle`. Nothing here
  asks a model anything. The threshold is desk policy, and desk policy belongs in
  the document, where it is auditable and where a replay of the run re-evaluates
  the identical condition against the identical recorded value. The branch's
  chosen case is recorded as `BranchTaken`, and every node on the arm that lost
  is recorded `NodeSkipped`, so the log says what did not happen as well as what
  did.
- **`approve_refund`** (`gate`). The human. Entering it suspends the run and
  records the gate's `approval_schema` as the shape of the answer it is waiting
  for. The run is now parked in the store, not held in a process: kill the
  machine and it is still parked. The approver's answer IS the refund
  instruction, because a gate passes its resume input through as its output and
  the next node consumes that verbatim.
- **`settle`** (`tool`, `issue_refund`, Write). The side effect. It appends one
  line to the refund ledger, and it is pinned to `write` in both agent files, so
  its intent is recorded before it runs and it is never retried blind.
- **`notify`** (`agent`, [`agents/customer-notice.toml`](agents/customer-notice.toml)).
  The prose. By the time this agent runs the refund is already out the door, so
  all it does is call `send_notice` once and report back. Money moves at a node
  the graph controls; the model is never the thing deciding whether to pay.
- **`small_claims`** (`agent`, [`agents/small-claims.toml`](agents/small-claims.toml)).
  The other arm, in one node. Below the threshold the desk wants no human in the
  loop, so a single agent issues the refund itself and sends the notice. The
  refund is still a Write, still recorded write-ahead, still never replayed
  blind: what changed between the arms is who decided, not what durability
  applies.

Every `agent_hash` in the document is the real content hash of a file checked in
beside it. Ask for them yourself:

```
$ salvor agent hash examples/graph-service/agents/customer-notice.toml \
                    examples/graph-service/agents/small-claims.toml
examples/graph-service/agents/customer-notice.toml: sha256:b7d78fea51de84c04878fee36bf4cbc25299f32829a0deec03f545e4b4aba081
examples/graph-service/agents/small-claims.toml: sha256:e08f574704bad055401cba7f6a289a903e6f2972a2d7a088be426f0533b54f13
```

A graph names an agent by hash and never by path, because the run's log records
only the hash and a replay has to mean the same agent. Edit either TOML and the
hash changes, and the document stops resolving until you update it.

## Running it

Build the binaries once, from the repository root:

```sh
# This example spawns the demo fixture binaries, which ship with the cargo
# install but not with the npm package:
cargo build
```

`run.sh` looks for `target/debug/salvor` and `target/debug/salvor-demo-model`
and stops if either is missing. If you would rather install than build, point it
at what you installed:

```sh
export SALVOR_BIN="$(command -v salvor)"
export SALVOR_DEMO_MODEL_BIN="$(command -v salvor-demo-model)"
```

Then, from anywhere:

```sh
examples/graph-service/run.sh
```

`run.sh` starts the scripted model on `127.0.0.1:18942`, walks the escalated arm
to the gate, approves it, kills the process mid-flight, recovers it, checks the
ledger, and then runs the auto-settle arm. It tears the model server down by the
pid it recorded. Every port and path is overridable
(`SALVOR_EXAMPLE_MODEL_PORT`, `SALVOR_EXAMPLE_STORE`, `SALVOR_EXAMPLE_LEDGER`,
`SALVOR_EXAMPLE_NOTICES`, `SALVOR_EXAMPLE_SCRATCH`), and nothing here binds a
control-plane port at all: `salvor graph run` drives the store directly.

### By hand

```sh
# from the repository root: the agent files name their MCP server by a path
# relative to the directory salvor is invoked from
export SALVOR_DEMO_BASE_URL=http://127.0.0.1:18942
export SALVOR_DISPUTES_LEDGER=/tmp/salvor-graph-service-ledger.txt
export SALVOR_DISPUTES_NOTICES=/tmp/salvor-graph-service-notices.json
salvor-demo-model --port 18942 --delay-ms 2000 \
    --script examples/graph-service/model-script.json &

salvor --store /tmp/salvor-graph-service.db \
    graph run examples/graph-service/dispute-refund.json \
    --input @examples/graph-service/input.json \
    --agent examples/graph-service/agents/customer-notice.toml \
    --agent examples/graph-service/agents/small-claims.toml
```

The run parks and prints the exact `salvor resume` line to continue it. Answer
the gate with the refund to issue:

```sh
salvor resume <RUN_ID> --graph examples/graph-service/dispute-refund.json \
    --agent examples/graph-service/agents/customer-notice.toml \
    --agent examples/graph-service/agents/small-claims.toml \
    --input '{"dispute_id":"DSP-4471","amount_usd":512.0,"approver":"j.okafor","note":"Duplicate charge confirmed against INV-90210."}'
```

`salvor history <RUN_ID>` prints the whole recorded walk afterwards. Feed it
[`input-small.json`](input-small.json) instead to take the other arm, which never
parks.

## What the durability guarantee buys you here

The gate is the obvious thing it buys: a run can wait days for a human without a
process waiting with it, because the park lives in the log.

The one worth proving is narrower and harder. Between "the approver said yes" and
"the customer has been told", real money leaves the account. If the process dies
in that window, the only two wrong answers are refunding twice and never
refunding at all. Salvor's claim is that neither can happen, and `run.sh` kills
the process there on purpose to show it.

The kill lands after the refund's completion is recorded and while the notice
agent is waiting on the model. The log stops mid-air:

```
  13  ToolCallRequested    issue_refund [Write] input {"amount_usd":512.0,"approver":"j.okafor",…
  14  ToolCallCompleted    output {"content":[{"text":"refunded $512.0 on DSP-4471, approved by j.okafor",…
  15  NodeExited           exit settle
  16  NodeEntered          enter notify
  17  NowObserved          2026-07-30 15:28:04Z
  18  ModelCallRequested   request sha256:cc88cc0…
```

`salvor resume <RUN_ID>` recovers it. Seq 0 to 18 replay from the log with no
live calls at all: the lookup is not re-run, the branch is not re-decided, the
gate is not re-asked, and, the point of the whole exercise, the refund is not
re-issued. The first live call of the recovered process is the model call that
never got an answer. The ledger settles it:

```
== refund ledger after the recovery ==
     1	{"amount_usd": 512.0, "approver": "j.okafor", "dispute_id": "DSP-4471", "note": "Duplicate charge confirmed against INV-90210."}
PROOF: the refund executed exactly once across the crash.
```

One line before the crash, one line after the recovery. The write-ahead rule is
what makes that safe rather than lucky: the intent at seq 13 was recorded before
the tool ran, so a crash in the *other* window (intent recorded, completion not)
is a dangling write that `resume` refuses to guess about, and a human resolves
with `salvor resolve`. `examples/reconciliation/` walks through that case in
full. Between the two of them there is no window where a refund quietly happens
twice or quietly disappears.

Declining an approval is the case with no event of its own, and deliberately so:
a run nobody resumes stays parked forever, which costs nothing, and
`salvor abandon <RUN_ID>` retires it by hand when the desk is sure.

## The tools, and why each effect is what it is

[`server.py`](server.py) is a three-tool MCP server in pure Python standard
library, one tool per effect class:

- **`lookup_dispute`, Read** (`readOnlyHint: true`). Reads
  [`disputes.json`](disputes.json), writes nothing. The hint is honest, so no
  operator override is needed.
- **`issue_refund`, Write.** Carries no annotation at all, which Salvor's
  conservative default already reads as a Write. Both agent files pin it anyway
  with `effect_overrides`, because inheriting the safe default and choosing it
  are different things and only one of them is recorded as the operator's
  decision.
- **`send_notice`, Idempotent** (`idempotentHint: true`). Upserts one entry keyed
  by dispute id and rewrites the file, so three calls leave exactly what one
  leaves. Here the hint is right and no override contradicts it. Compare
  `examples/support-ops/`, where two tools carry the identical hint and get
  opposite treatment.

Every tool answers with `structuredContent` as well as human-readable text,
because Salvor records the WHOLE tool result as the node's output and the branch
has to have something typed to route on. That is why the condition reads
`structuredContent.amount_usd`, not `amount_usd`: the path is into the tool
result, and a path that does not resolve is false rather than an error.

## The offline model

[`model-script.json`](model-script.json) is a `--script` file in the named-
conversation form: an object mapping a name to that conversation's turns. A graph
needs that form. Every `agent` node is its own message list, so all of them make
their first model call carrying exactly one message and would collide if the
script were selected by message count alone. The server instead selects the one
conversation whose **name appears as a substring of the request's system
prompt**, and refuses loudly when none or several match.

That is why each agent's system prompt opens by naming itself, and why the two
names (`customer-notice`, `small-claims`) appear in one prompt each. Renaming an
agent means renaming its conversation, or the script server answers with a 500
instead of guessing.

Once a conversation is selected, its turns are a fixed tape: the messages in
`model-script.json` play back in order, and any `tool_use` in them carries
whatever arguments were typed into the file, for example `small-claims`'s
`issue_refund` call always names `DSP-3312` at `47.25`. Nothing in the script
server reads the graph's actual input or the tool results the run produced so
far, and nothing checks a scripted call's arguments against them either. That
is fine for a fixture built to always take one path with one input, and it is
the thing to unlearn if you are used to a model that looks at what it is
actually given: change `input.json` here and the scripted calls answer with
the same numbers regardless.

## Driving this same document over HTTP

The document is portable; the tools are not.

`salvor graph run` has no standalone tool inventory. A `tool` node resolves
against the tools the supplied `--agent` files carry, which is exactly how
`lookup_dispute` and `issue_refund` resolve here: both agent files declare the
same MCP server, so both carry all three tools, and the graph's tool nodes borrow
them.

A `salvor serve` control plane resolves tool nodes against its own registry
instead, and that registry is **empty** in a stock server. `--demo-tools`
registers three tools, but they are a different set (`lookup_invoice`,
`issue_refund`, `send_email`) with different shapes. Submitting this document to
such a server stores it fine and then refuses the run, precisely:

```
$ curl -s -X POST http://127.0.0.1:18943/v1/graphs \
    -H 'content-type: application/json' --data @examples/graph-service/dispute-refund.json
{"created":true,"graph":"sha256:ffc0a7fb12862ee5deb543e8e35949cca5a97512549432de975e388d357cee1b"}

$ curl -s -X POST http://127.0.0.1:18943/v1/graph-runs -H 'content-type: application/json' \
    -d '{"graph_hash":"sha256:ffc0a7fb...","input":{"dispute_id":"DSP-4471"}}'
{"error":{"code":"unknown_tool","message":"tool node `pull_dispute` names tool `lookup_dispute`, which is not registered on this server"}}
```

That refusal is the mechanism working: the server resolves everything the
document references up front, so a run is spawned only once it is sure the run
can finish, rather than stranding it at the offending node. Driving this graph
over HTTP therefore means a host that registers `lookup_dispute` and
`issue_refund` on its `ToolRegistry`, the way `--demo-tools` does for its own
three. A native tool's output is its typed struct rather than an MCP
`CallToolResult`, so such a host also decides whether to match this document's
`structuredContent.` prefix or to carry a branch condition of its own.

## Files

- `dispute-refund.json`: the graph document.
- `agents/customer-notice.toml`, `agents/small-claims.toml`: the two agents the
  document references by hash.
- `server.py`: the three-tool MCP server, standard library only.
- `disputes.json`: the seed dispute records `lookup_dispute` reads.
- `model-script.json`: the offline scripted model's named conversations.
- `input.json`, `input-small.json`: the escalated and the auto-settle inputs.
- `run.sh`: the whole walkthrough, offline, tearing down what it started.

The refund ledger and the customer notices are runtime state written to a scratch
path, never into this directory. See `examples/graphs/README.md` for the document
format itself, and `crates/salvor-server/API.md` for the HTTP contract.
