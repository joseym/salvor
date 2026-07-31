# graph-clients: the same refund desk, driven by client applications

An invoice dispute arrives at an application you wrote. It decides whether a
human needs to sign off, gets that signature when the amount is large enough,
moves the money, and tells the customer. The deciding, the parking, and the
recording are Salvor's; the application's job is to submit a document, start a
run, answer a gate, and read the result back.

This is the same desk as [`examples/graph-service/`](../graph-service/), and
deliberately so. That example drives the graph from the CLI. This one drives it
from application code over HTTP, against a **stock `salvor serve`**: no
`--demo-tools`, no tools compiled into the binary, nothing a reader would have to
write Rust to reproduce.

Everything runs offline. A scripted model server (`salvor-demo-model`) stands in
for a real endpoint, so no API key and no network are needed, and the tools are
the sibling example's Python MCP server ([`../graph-service/server.py`](../graph-service/server.py)),
reused by relative path rather than copied.

## The graph, node by node

```
route_by_amount ──escalate────▶ approve_refund ──▶ settle_and_notify
      │
      └─────────auto_settle──▶ small_claims
```

- **`route_by_amount`** (`branch`). The entry node. A node with no inbound edge
  receives the graph input verbatim, so this branch evaluates its cases straight
  against the dispute record the caller supplied. Two cases, both
  **expressions**, in author order: `amount_usd >= 250.0` routes to `escalate`,
  `amount_usd < 250.0` routes to `auto_settle`. The paths are **bare**, with no
  `structuredContent.` prefix, because the value being routed on is the graph
  input rather than an MCP tool result. Nothing here asks a model anything. The
  threshold is desk policy, and desk policy belongs in the document, where it is
  auditable and where a replay re-evaluates the identical condition against the
  identical recorded value. The chosen case is recorded as `BranchTaken`, and
  every node on the arm that lost is recorded `NodeSkipped`.
- **`approve_refund`** (`gate`). The human. Entering it suspends the run and
  records the gate's `approval_schema` as the shape of the answer it is waiting
  for. The run is now parked in the store, not held in a process: kill the
  machine and it is still parked, and any client that can reach the server can
  answer it. The approver's answer IS the refund instruction, because a gate
  passes its resume input through as its output and the next node consumes that
  verbatim.
- **`settle_and_notify`** (`agent`, [`agents/settle-and-notify.toml`](agents/settle-and-notify.toml)).
  The escalated arm's one node after the gate. It receives the approval and both
  issues the refund and sends the notice, in two MCP tool calls. `issue_refund`
  is pinned to `write` with `effect_overrides`, so its intent is recorded before
  it runs and it is never retried blind.
- **`small_claims`** (`agent`, [`agents/small-claims.toml`](agents/small-claims.toml)).
  The other arm, in one node. Below the threshold the desk wants no human in the
  loop, so a single agent issues the refund itself and sends the notice. Its
  input is the graph input verbatim, because the branch is a pure router and
  passes its input through unchanged. The refund is still a Write, still recorded
  write-ahead, still never replayed blind: what changed between the arms is who
  decided, not what durability applies.

## The input, and why there is no lookup step

The sibling example opens with a `lookup_dispute` tool node, because the CLI is
handed nothing but a dispute id. An application is not in that position. A
service that is starting a run already holds the dispute record, because holding
it is what its own database is for, so the run starts with the record itself:

```json
{
  "dispute_id": "DSP-4471",
  "amount_usd": 512.0,
  "customer": "Harborline Freight",
  "reason": "duplicate_charge"
}
```

[`input.json`](input.json) is that escalated dispute; [`input-small.json`](input-small.json)
is the small one, `DSP-3312` at `$47.25`. The threshold is 250.0 in both
examples, so the same two disputes take the same two arms.

The input has to carry `amount_usd` because the branch is the entry node, so the
graph input is the only thing it can route on. A branch path that does not
resolve evaluates **false** rather than erroring, so a document whose input
omitted `amount_usd` would not fail; it would quietly take the other arm. That is why the proof below pastes the `BranchTaken` event for
both runs rather than trusting that the right thing happened.

## Why there are no tool nodes

Over HTTP, a `tool` node resolves **only** against the tool registry compiled
into the server binary
([`crates/salvor-server/src/graph.rs`](../../crates/salvor-server/src/graph.rs)),
and `salvor serve` wires that registry **empty**. An agent's MCP tools are built
by the server, from the definitions a client registered, but they are not
reachable from a tool node. There is no HTTP surface for registering a tool.

So a graph meant to be driven by client applications, and copied by a reader who
is not going to write Rust, puts its side effects in **agent** nodes, where real
MCP tools are reachable. That is why this document's shape differs from its
sibling's: it is the shape that runs on a server nobody had to recompile.

The refusal is precise. Submitting the sibling's document to the same stock
server used for the proof below:

```
$ curl -s -X POST http://127.0.0.1:18962/v1/graphs \
    -H 'Content-Type: application/json' \
    --data-binary @examples/graph-service/dispute-refund.json
{"created":true,"graph":"sha256:ffc0a7fb12862ee5deb543e8e35949cca5a97512549432de975e388d357cee1b"}

$ curl -s -X POST http://127.0.0.1:18962/v1/graph-runs \
    -H 'Content-Type: application/json' \
    -d '{"graph_hash":"sha256:ffc0a7fb...","input":{"dispute_id":"DSP-4471"}}'
{"error":{"code":"unknown_tool","message":"tool node `pull_dispute` names tool `lookup_dispute`, which is not registered on this server"}}
```

The document stores fine and the run is refused, up front, before anything is
spawned: the server resolves everything a document references before it starts,
so a run either can finish or never begins. See
[`examples/graph-service/`](../graph-service/) for the tool-node form under the
CLI, where a `tool` node resolves against the tools the supplied `--agent` files
carry and the refund happens at a node the graph itself controls.

## The offline model

[`model-script.json`](model-script.json) plays back fixed turns, one named
conversation per agent, selected the same way as in
[`examples/graph-service/`](../graph-service/): the server picks the
conversation whose name appears as a substring of the request's system
prompt. Once a conversation is selected its messages are a tape, not logic:
any `tool_use` in it carries whatever arguments were typed into the file, for
example `small-claims`'s `issue_refund` call always names `DSP-3312` at
`47.25`. The scripted model never reads the graph's actual input or a run's
tool results, and nothing checks a scripted call's arguments against either
one. Feeding this document a different `amount_usd` changes which arm the
branch takes, since that routing is real, but it does not change what the
agent on the arm you land on says or calls: that part is fixed regardless of
input.

## The agent hashes

The document names each agent by content hash, never by path. Ask for them
yourself:

```
$ salvor agent hash examples/graph-clients/agents/settle-and-notify.toml \
                    examples/graph-clients/agents/small-claims.toml
examples/graph-clients/agents/settle-and-notify.toml: sha256:4cb91b34c7644e8ea7639080204ee292b7920a113b5763ca27a2dfca37b15673
examples/graph-clients/agents/small-claims.toml: sha256:dd0c6e4fc3dc17f26a24010fda401f4bc93ce24c140f07ea9fd7fdda71602bcd
```

Those are the two strings in [`dispute-refund.json`](dispute-refund.json), and
`run.sh` recomputes both at startup and refuses to go on if either has drifted,
so an edit to a TOML cannot silently desync the document.

The document itself validates:

```
$ salvor graph validate examples/graph-clients/dispute-refund.json
graph ok: 4 node(s), 3 edge(s)
entry:    route_by_amount
terminal: settle_and_notify, small_claims
```

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
examples/graph-clients/run.sh
```

`run.sh` starts the scripted model on `127.0.0.1:18961` and a stock
`salvor serve` on `127.0.0.1:18962`, waits for the control plane to answer,
re-verifies both agent hashes against the document, and then runs the three apps
in turn. Python and TypeScript drive that server. The Rust app takes a store path
instead and drives the engine inside its own process; what it shares with the
other two is the model server and the ledger. `run.sh` tears both servers down by
the pids it recorded, never by pattern. Every port and path is overridable
(`SALVOR_EXAMPLE_MODEL_PORT`, `SALVOR_EXAMPLE_BIND`, `SALVOR_EXAMPLE_STORE`,
`SALVOR_EXAMPLE_LEDGER`, `SALVOR_EXAMPLE_NOTICES`, `SALVOR_EXAMPLE_SCRATCH`).

A missing app is announced and skipped rather than failing the run.

## The proof, by hand

This is the exact sequence, with the exact output. The server below was started
stock, with no tools of any kind:

```sh
export SALVOR_DISPUTES_LEDGER=/tmp/ledger.txt
export SALVOR_DISPUTES_NOTICES=/tmp/notices.json
export SALVOR_DEMO_BASE_URL=http://127.0.0.1:18961
salvor-demo-model --port 18961 --delay-ms 20 \
    --script examples/graph-clients/model-script.json &
salvor --store /tmp/proof.db serve --bind 127.0.0.1:18962 &
```

**1. Register both agents.** The body is the TOML the CLI reads, sent as
`application/toml`. The server builds each definition to validate it, which
spawns and immediately closes its MCP session, and returns the hash:

```
$ curl -s -X POST http://127.0.0.1:18962/v1/agents \
    -H 'Content-Type: application/toml' \
    --data-binary @examples/graph-clients/agents/settle-and-notify.toml
{"agent":"sha256:4cb91b34c7644e8ea7639080204ee292b7920a113b5763ca27a2dfca37b15673","created":true}

$ curl -s -X POST http://127.0.0.1:18962/v1/agents \
    -H 'Content-Type: application/toml' \
    --data-binary @examples/graph-clients/agents/small-claims.toml
{"agent":"sha256:dd0c6e4fc3dc17f26a24010fda401f4bc93ce24c140f07ea9fd7fdda71602bcd","created":true}
```

Both match `salvor agent hash` and both match the document, which is the whole
point of content addressing: the client and the server computed the same identity
independently.

**2. Submit the document.**

```
$ curl -s -X POST http://127.0.0.1:18962/v1/graphs \
    -H 'Content-Type: application/json' \
    --data-binary @examples/graph-clients/dispute-refund.json
{"created":true,"graph":"sha256:48f64baa7f0939b6fa78900825489eeef65ed7b1089d7200420eab49a5874d8e"}
```

**3. Start the escalated run.**

```
$ curl -s -X POST http://127.0.0.1:18962/v1/graph-runs \
    -H 'Content-Type: application/json' \
    -d '{"graph_hash":"sha256:48f64baa...","input":{"dispute_id":"DSP-4471","amount_usd":512.0,"customer":"Harborline Freight","reason":"duplicate_charge"},"labels":{"desk":"disputes"}}'
{"run":"b96e1db9-09d9-4475-959c-05887dc732b6","status":"running"}
```

It parks at the gate, and the status carries the schema of the answer it wants:

```
$ curl -s http://127.0.0.1:18962/v1/runs/b96e1db9-09d9-4475-959c-05887dc732b6 \
    | python3 -m json.tool
{
    "driver": "none",
    "event_count": 6,
    "first_recorded_at": "2026-07-30T17:13:21.12861Z",
    "last_recorded_at": "2026-07-30T17:13:21.131534Z",
    "pending": null,
    "run": "b96e1db9-09d9-4475-959c-05887dc732b6",
    "status": {
        "input_schema": {
            "properties": {
                "amount_usd": {
                    "type": "number"
                },
                "approver": {
                    "type": "string"
                },
                "dispute_id": {
                    "type": "string"
                },
                "note": {
                    "type": "string"
                }
            },
            "required": [
                "dispute_id",
                "amount_usd",
                "approver"
            ],
            "type": "object"
        },
        "reason": "This dispute is at or above the review threshold. Answer with the refund to issue: dispute_id, amount_usd, approver, and an optional note. The run stays parked until you do.",
        "state": "suspended"
    },
    "usage": {
        "input_tokens": 0,
        "output_tokens": 0
    }
}
```

`pending` is `null` and `usage` is all zeros because nothing has called a model
yet: the walk reached the gate and stopped there.

The branch fired `escalate`, and the log says so rather than the reader assuming
it:

```
$ curl -sN http://127.0.0.1:18962/v1/runs/b96e1db9-09d9-4475-959c-05887dc732b6/events \
    | grep BranchTaken
data: {"run_id":"b96e1db9-09d9-4475-959c-05887dc732b6","seq":2,"schema_version":1,"recorded_at":"2026-07-30T17:13:21.131254Z","event":{"kind":"BranchTaken","payload":{"node":"route_by_amount","case":"escalate"}}}
```

**4. Answer the gate over HTTP.** A parked graph run resumes through the ordinary
run resume endpoint; the server looks the document back up by the hash the log
records and re-drives it.

```
$ curl -s -X POST http://127.0.0.1:18962/v1/runs/b96e1db9-09d9-4475-959c-05887dc732b6/resume \
    -H 'Content-Type: application/json' \
    -d '{"input":{"dispute_id":"DSP-4471","amount_usd":512.0,"approver":"j.okafor","note":"Duplicate charge confirmed against INV-90210."}}'
{"outcome":"driving","run":"b96e1db9-09d9-4475-959c-05887dc732b6","status":"running"}

$ curl -s http://127.0.0.1:18962/v1/runs/b96e1db9-09d9-4475-959c-05887dc732b6 \
    | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["status"], indent=2))'
{
  "output": "Refunded $512.00 on DSP-4471 as approved by j.okafor, and told Harborline Freight it is on the way.",
  "state": "completed"
}

$ cat -n /tmp/ledger.txt
     1  {"amount_usd": 512.0, "approver": "j.okafor", "dispute_id": "DSP-4471", "note": "Duplicate charge confirmed against INV-90210."}
```

One refund, for the amount the approver named, by the approver the gate recorded.
The per-node projection shows the arm that lost:

```
$ curl -s http://127.0.0.1:18962/v1/runs/b96e1db9-09d9-4475-959c-05887dc732b6/graph
{"graph_hash":"sha256:48f64baa7f0939b6fa78900825489eeef65ed7b1089d7200420eab49a5874d8e","nodes":[{"branch_case":"escalate","node":"route_by_amount","state":"exited"},{"node":"approve_refund","state":"exited"},{"node":"settle_and_notify","state":"exited"},{"node":"small_claims","reason":"no live inbound edge: an upstream branch routed to another case","state":"skipped"}]}
```

**5. Run the auto-settle arm.** Same document, same server, a smaller amount.

```
$ curl -s -X POST http://127.0.0.1:18962/v1/graph-runs \
    -H 'Content-Type: application/json' \
    -d '{"graph_hash":"sha256:48f64baa...","input":{"dispute_id":"DSP-3312","amount_usd":47.25,"customer":"Kettle Row Bakery","reason":"late_delivery_credit"},"labels":{"desk":"disputes"}}'
{"run":"e002883a-2244-4f1a-a65b-e766946a773e","status":"running"}

$ curl -s http://127.0.0.1:18962/v1/runs/e002883a-2244-4f1a-a65b-e766946a773e \
    | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["status"], indent=2))'
{
  "output": "Settled DSP-3312: refunded $47.25 to Kettle Row Bakery under the auto-settle policy and sent the notice.",
  "state": "completed"
}
```

It never suspended, and the recorded walk shows the gate skipped rather than
answered. The stream frames are the pinned event envelopes, one per line;
reduced to just the sequence number and the kind, the walk reads:

```
$ curl -sN http://127.0.0.1:18962/v1/runs/e002883a-2244-4f1a-a65b-e766946a773e/events \
    | python3 -c '
import json, sys
for line in sys.stdin:
    if line.startswith("data: "):
        frame = json.loads(line[6:])
        if "seq" in frame:
            print(frame["seq"], frame["event"]["kind"])'
 0  GraphRunStarted
 1  NodeEntered
 2  BranchTaken
 3  NodeExited
 4  NodeSkipped
 5  NodeSkipped
 6  NodeEntered
 7  NowObserved
 8  ModelCallRequested
 9  ModelCallCompleted
10  ToolCallRequested
11  ToolCallCompleted
12  NowObserved
13  ModelCallRequested
14  ModelCallCompleted
15  RandomObserved
16  ToolCallRequested
17  ToolCallCompleted
18  NowObserved
19  ModelCallRequested
20  ModelCallCompleted
21  NodeExited
22  RunCompleted
```

Seq 4 and 5 are `approve_refund` and `settle_and_notify`, skipped. And seq 2 is
the other `BranchTaken`, the auto-settle one:

```
data: {"run_id":"e002883a-2244-4f1a-a65b-e766946a773e","seq":2,"schema_version":1,"recorded_at":"2026-07-30T17:13:54.700454Z","event":{"kind":"BranchTaken","payload":{"node":"route_by_amount","case":"auto_settle"}}}
```

**6. The ledger, after both runs.** Two lines, one per refund, no more:

```
$ cat -n /tmp/ledger.txt
     1  {"amount_usd": 512.0, "approver": "j.okafor", "dispute_id": "DSP-4471", "note": "Duplicate charge confirmed against INV-90210."}
     2  {"amount_usd": 47.25, "approver": "auto-settle-policy", "dispute_id": "DSP-3312", "note": "Late-delivery credit under the review threshold; settled without escalation."}

$ cat /tmp/notices.json
{
  "DSP-3312": "Your late-delivery credit of $47.25 on invoice INV-90144 has been refunded. No further action is needed.",
  "DSP-4471": "Your dispute on invoice INV-90210 was approved. A refund of $512.00 has been issued and should reach your account within three business days."
}
```

## Submitted graphs live in memory

The server holds submitted graph documents in a **process-local, in-memory**
registry. Restart it and every submitted document is gone, and a run or a fork
that references one is refused with `unknown_graph` until it is resubmitted. The
same is true of registered agent definitions.

That is less fragile than it sounds, because both are content-addressed:
resubmitting the identical bytes yields the identical hash, so a client that
resubmits on startup, or that resubmits on a `404` and retries, is correct rather
than merely lucky. Run logs are the durable half and live in the store, so a
parked run survives the restart even though the document it walks has to be
handed back. Both HTTP apps below register the two agents and submit the document
before they start anything, for exactly this reason.

## The three apps

The same story, three ways: run `DSP-4471` to the gate, answer it, then run
`DSP-3312` straight through. Python and TypeScript do that against the one server
`run.sh` started, registering the agents and submitting the document first. The
Rust app registers nothing and submits nothing, because there is no server in
front of it to register with.

That split is deliberate. Python and TypeScript have Salvor clients, so they
drive the control plane the way any service would. There is no Rust HTTP client
crate, and hand-rolling one for an example would teach the wrong lesson: from
Rust the runtime is a library, so the Rust app embeds the engine in its own
process and drives the same document against a store of its own, which `run.sh`
passes it on the command line.

### Why these apps use the SDK sources rather than the published packages

[`examples/polyglot-service/`](../polyglot-service/) installs the published
`salvor` from PyPI and the published `@salvor-run/client` from npm, on purpose:
an example that a reader can follow with an ordinary `pip install` is worth more
than one that assumes a checkout.

This example does not, and the reason is narrow. The graph methods these apps
need (`submit_graph`, `start_graph_run`, `validate_graph`, `get_graph`,
`list_graphs` in Python; `submitGraph`, `startGraphRun`, `validateGraph` in
TypeScript) are newer than the latest release, 0.7.0. Installing that release
would not produce a subtly different result; it would fail on a missing attribute
before the app did anything. So `run.sh` installs this checkout's Python SDK in
editable form from [`sdks/python`](../../sdks/python), and the TypeScript app
imports [`sdks/typescript`](../../sdks/typescript)'s built output by relative path
rather than by package name, with `run.sh` building it first and refusing to run
against a build that predates the graph methods.

**This should be reverted once a release ships them.** At that point both apps go
back to the published packages, exactly like the sibling example, and `run.sh`
loses the local-SDK handling entirely. The comments in `run.sh` say the same
thing at each of the two places that would change.

### Why the Rust app is an example target

The Rust app is not a standalone Cargo project. It is an `[[example]]` target
named `graph_clients` declared on `salvor-cli`, with its source at
`rust/main.rs`, so `run.sh` invokes it as
`cargo run -p salvor-cli --example graph_clients`. That is the pattern this
repository already uses, the same way `salvor-runtime` declares the
`todo_agent` example pointing at `examples/todo-agent/main.rs`, and the reason is
that a workspace build then compiles every example on every build. A standalone
project would need a workspace `exclude` entry, would never be built by CI, and
would rot without anyone noticing.

### Python

[`python/service.py`](python/service.py) rebuilds the document with the typed
`GraphBuilder` and submits both encodings, which have to hash the same. It
streams each run until the stream closes, then reads the resting state back with
`get_run` rather than taking it off the frame it just printed. Its evidence that
the auto-settle arm skipped the gate comes from the per-node projection,
`get_run_graph`.

### TypeScript

[`typescript/service.ts`](typescript/service.ts) does the same rebuild and takes
its evidence from the stream instead: it collects `NodeSkipped` frames as they
arrive and fails if the auto-settle run produced none. The terminal `end` frame
carries the resting status, so there is no follow-up `GET`. It runs under
`node --experimental-strip-types`, so it has no build step of its own.

### Rust

[`rust/main.rs`](rust/main.rs) has no HTTP in it. It opens a store, builds both
agents in process, and calls `run_graph`. Between parking at the gate and
answering it, the app closes the MCP sessions and drops the agents, the run
context, and the store handle, then reopens the store by path and rebuilds the
run from the log. Only the run id crosses that gap. The ledger it counts is
shared with whatever ran before it, so its assertions are on deltas.

## Files

- `dispute-refund.json`: the graph document.
- `agents/settle-and-notify.toml`, `agents/small-claims.toml`: the two agents the
  document references by hash.
- `model-script.json`: the offline scripted model's named conversations, one per
  agent, selected by finding the conversation's name as a substring of the
  request's system prompt. That is why each agent's system prompt opens by naming
  itself, and why neither name is a substring of the other.
- `input.json`, `input-small.json`: the escalated and the auto-settle disputes.
- `run.sh`: the offline stack plus all three apps, tearing down what it started.
- `python/service.py`, `typescript/service.ts`, `rust/main.rs`: the three client
  apps. The Rust one is built as an `[[example]]` on `salvor-cli` rather than as
  a project of its own, so its source sits here with no `Cargo.toml` beside it.

The MCP server and its seed records are **not** copied here: the agents declare
`examples/graph-service/server.py` by relative path, so both examples run the
same three tools against the same code. The refund ledger and the customer
notices are runtime state written to a scratch path, never into this directory.

See [`examples/graphs/README.md`](../graphs/README.md) for the document format
itself, [`crates/salvor-server/API.md`](../../crates/salvor-server/API.md) for the
HTTP contract, and [`examples/polyglot-service/`](../polyglot-service/) for the
same client-app shape driving a single agent rather than a graph.
