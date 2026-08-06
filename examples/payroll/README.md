# payroll: a pay run as a Salvor graph, and the fan-out proof

A payroll batch is a loop over people, and the loop is where the money is. If the
process dies half way through paying twelve employees, exactly two things must
never happen: an employee the batch already paid gets paid again, and an employee
the batch had not reached yet never gets paid at all. Paid-twice and never-paid
are the whole risk, and a `map` over a roster driven under Salvor's durable log
is what kills both.

That is this example. One graph document ([`payroll-run.json`](payroll-run.json))
pulls a roster, flags amounts that are wildly off the median, routes the flagged
ones through a human who amends them, then pays each employee in a fan-out. The
run script kills the process in the middle of that fan-out and recovers it, and
the pay ledger settles the argument: twelve lines, one per employee, the amended
amounts, across the crash.

Everything runs offline. A scripted model server (`salvor-demo-model`) stands in
for a real endpoint, so no API key and no network are needed, and the tools are
one Python MCP server ([`server.py`](server.py)) that depends on nothing but the
standard library.

## The graph, node by node

```
pull_roster ─▶ flag_exceptions ─▶ route ──review──▶ review_exceptions ─▶ pay_each ─▶ notify_summary
                                    │                                      ▲
                                    └───────────────pay_all────────────────┘
```

- **`pull_roster`** (`tool`, Read). The entry node. Its input is the graph input,
  `{"pay_period": "2025-11-B"}`, and it resolves that period to a roster: twelve
  employees, each with an id, a name, and a gross amount in cents. It stamps the
  period onto every row and reports the median. A Read is freely retryable, so an
  interrupted lookup just re-reads.
- **`flag_exceptions`** (`tool`, Read). Scans the roster for amounts far off the
  median, more than five times it or less than a fifth of it, and returns a
  structured verdict: the clean count, the flagged rows with a reason each, and
  the roster carried through unchanged. In this period two rows are flagged: a
  bonus typo ten times the median, and a missing-digits amount a hundredth of it.
  It computes, it never writes, so it is a Read.
- **`route`** (`branch`). Two cases, both **expressions**, in author order:
  `structuredContent.flagged_count == 0` routes to `pay_all` (straight to the pay
  fan-out), and `structuredContent.flagged_count > 0` routes to `review` (through
  the gate). The path carries the `structuredContent.` prefix because the value
  being routed on is an MCP tool result: Salvor records the WHOLE tool result as
  the node's output, so the typed fields a branch reads live under
  `structuredContent`. Desk policy (the median thresholds) is in the tool and the
  routing is in the document, where a replay re-evaluates the identical condition
  against the identical recorded value. The chosen case is recorded as
  `BranchTaken`, and every node on the arm that lost is recorded `NodeSkipped`.
- **`review_exceptions`** (`gate`). The human. Entering it suspends the run and
  records the gate's `approval_schema` as the shape of the answer it waits for.
  The run is now parked in the store, not held in a process: kill the machine and
  it is still parked. The approver answers with the amended roster to pay, and
  that answer IS the pay instruction, because a gate passes its resume input
  through as its output and the map downstream consumes it verbatim. The amounts
  the approver sends are the amounts that get paid.
- **`pay_each`** (`map`, body `pay_employee`). The fan-out, and the point of the
  example. It maps over the roster list, one iteration per employee, running each
  through the `pay_employee` tool, which appends one durable line to the pay
  ledger keyed by `pay_period:id`. Iterations run inline and in index order (the
  `concurrency` cap of 1 is accepted, and v0.4 runs the fan-out sequentially
  regardless). Its `over` reference is
  [`structuredContent.roster`](#which-roster-the-map-pays), the roster the
  upstream node carries.
- **`notify_summary`** (`agent`, [`agents/notify-summary.toml`](agents/notify-summary.toml)).
  The prose, and the one node that can consume the map's output at all. A `map`
  joins its iterations into a JSON array, and an MCP tool call's arguments must be
  a JSON object, so a plain `tool` node cannot take the joined array as its input.
  An `agent` node can: it reads any input and issues its own well-formed
  `send_summary` call. By the time it runs every employee has been paid, so all it
  does is write one closing notice and report back. Money moves at the map's tool
  iterations the graph controls; the model is never the thing deciding whether to
  pay.

Every `agent_hash` in the document is the real content hash of the file checked
in beside it. Ask for it yourself:

```
$ salvor agent hash examples/payroll/agents/notify-summary.toml
sha256:a3d44fe7b1a00365f92b2f0472be1b8f5fab03a4a69c5e3bfc49f9faa4a5aff4
```

A graph names an agent by hash and never by path, because the run's log records
only the hash and a replay has to mean the same agent. Edit the TOML and the hash
changes, and the document stops resolving until you update it.

The document validates:

```
$ salvor graph validate examples/payroll/payroll-run.json
graph ok: 7 node(s), 6 edge(s)
entry:    pay_employee, pull_roster
terminal: notify_summary, pay_employee
```

(`pay_employee` shows as an entry and terminal because it is the map's body: it
has no edges of its own and is walked only inside the fan-out, never on the main
path.)

## Which roster the map pays

The map's `over` is `structuredContent.roster`, and it resolves against whatever
node feeds the map on the arm that ran:

- On the **`review`** arm the map's input is the gate's answer, so `over` reads
  the **amended** roster the approver sent. That is the whole reason the amounts
  can be corrected: the map pays the roster the human signed off, not the one the
  desk pulled.
- On the **`pay_all`** arm the map's input is the branch passing `flag_exceptions`'
  result through unchanged, so `over` reads that tool's `structuredContent.roster`,
  the original roster, because a clean roster needs no amendment.

Both arms therefore expose the roster at the same path, `structuredContent.roster`,
which is why one `over` serves both. The gate's answer is shaped to match: it
carries a `structuredContent` object holding the `roster`, the same shape the
reviewer produced, so the approver is handing back the reviewed roster with the
flagged amounts corrected. A `map` whose `over` does not resolve to a list is a
typed refusal (`MapOverNotAList`) before the fan-out records anything, so getting
this path right is not optional.

## The offline model

[`model-script.json`](model-script.json) is a `--script` file in the named-
conversation form: an object mapping a name to that conversation's turns. The
server selects the conversation whose **name appears as a substring of the
request's system prompt**, which is why the agent's system prompt opens by naming
itself (`notify-summary`), and refuses loudly when none or several match.

Once a conversation is selected its turns are a fixed tape: the `send_summary`
call plays back with whatever message was typed into the file, and nothing in the
script server reads the graph's actual input or the payments the run made. That is
fine for a closing notice built to say the same true thing every run ("every
employee on the roster was paid exactly once"), and it is the thing to unlearn if
you are used to a model that looks at what it was actually given.

## Running it

Build the binaries once, from the repository root:

```sh
# This example spawns the demo fixture binaries, which ship with the cargo
# install but not with the npm package:
cargo build
```

`run.sh` looks for `target/debug/salvor` and `target/debug/salvor-demo-model` and
stops if either is missing. If you would rather install than build, point it at
what you installed:

```sh
export SALVOR_BIN="$(command -v salvor)"
export SALVOR_DEMO_MODEL_BIN="$(command -v salvor-demo-model)"
```

Then, from anywhere:

```sh
examples/payroll/run.sh
```

`run.sh` starts the scripted model on `127.0.0.1:18946`, pulls the roster and
parks at the gate, approves the amended roster, kills the process part way
through the pay batch, recovers it, checks the ledger, and then runs the clean
arm. It tears the model server down by the pid it recorded. Every port and path
is overridable (`SALVOR_EXAMPLE_MODEL_PORT`, `SALVOR_EXAMPLE_STORE`,
`SALVOR_EXAMPLE_LEDGER`, `SALVOR_EXAMPLE_NOTICES`, `SALVOR_EXAMPLE_PAY_DELAY_MS`,
`SALVOR_EXAMPLE_SCRATCH`), and nothing here binds a control-plane port at all:
`salvor graph run` drives the store directly.

### By hand

```sh
# from the repository root: the agent file names its MCP server by a path
# relative to the directory salvor is invoked from
export SALVOR_DEMO_BASE_URL=http://127.0.0.1:18946
export SALVOR_PAYROLL_LEDGER=/tmp/salvor-payroll-ledger.txt
export SALVOR_PAYROLL_NOTICES=/tmp/salvor-payroll-notices.txt
salvor-demo-model --port 18946 --delay-ms 50 \
    --script examples/payroll/model-script.json &

salvor --store /tmp/salvor-payroll.db \
    graph run examples/payroll/payroll-run.json \
    --input '{"pay_period":"2025-11-B"}' \
    --agent examples/payroll/agents/notify-summary.toml
```

The run parks and prints the exact `salvor resume` line to continue it. Answer the
gate with the amended roster to pay (the run script's `AMENDED_ANSWER` is the full
twelve-row object). `salvor history <RUN_ID>` prints the whole recorded walk
afterwards. Feed it `{"pay_period":"2025-11-A"}`, a roster with no flagged rows,
to take the clean arm, which never parks.

## What the durability guarantee buys you here

The gate is the obvious thing it buys: a pay run can wait for a human without a
process waiting with it, because the park lives in the log.

The one worth proving is the fan-out. Between the first employee paid and the last,
the process can die, and the two wrong answers are paying someone twice and paying
someone not at all. `run.sh` kills the process there on purpose. It waits until the
ledger holds four to eight lines, so the batch is under way but not finished, then
sends `kill -9`:

```
pay ledger at the instant of the kill:
     1  {"amount_cents": 410000, "id": "E01", "key": "2025-11-B:E01", "name": "Ada Okonkwo"}
     2  {"amount_cents": 455000, "id": "E02", "key": "2025-11-B:E02", "name": "Bhavna Rao"}
     3  {"amount_cents": 390000, "id": "E03", "key": "2025-11-B:E03", "name": "Cyrus Alizadeh"}
     4  {"amount_cents": 512000, "id": "E04", "key": "2025-11-B:E04", "name": "Deepa Menon"}
```

`salvor resume <RUN_ID>` recovers it. The finished iterations replay from the log
with no live calls: the roster is not re-pulled, the branch is not re-decided, the
gate is not re-asked, and the employees already paid are not paid again. The map
re-drives only the iterations that had not finished. The ledger settles it:

```
== pay ledger after the recovery ==
     1  {"amount_cents": 410000, "id": "E01", ...}
     ...
    12  {"amount_cents": 498000, "id": "E12", ...}
PROOF: 12 ledger lines, one per employee, across the crash.
PROOF: 12 distinct employee ids, so no employee was paid twice and none was skipped.
PROOF: the amended amounts were paid (E07 470000, E10 420000), never the flagged 4500000 or 4200.
PROOF: the branch recorded BranchTaken route -> review, so the run went through the human gate.
PROOF: 12 MapIterationJoined events, the whole fan-out visible in the recorded walk.
```

Two things make the fan-out safe rather than lucky, and they are worth separating.

The **replay** is what stops a re-charge: an iteration whose `ToolCallCompleted`
is already in the log is not re-run at all on resume. The **idempotency** is what
covers the seam. A `map` re-drives a not-yet-completed iteration live, and a
`kill -9` can leave the pay tool's write in flight (its intent recorded, its
completion not), including a still-running tool child the crashed process
orphaned. So `pay_employee` is an Idempotent tool that keys each payment on
`pay_period:id` and, under one exclusive file lock, checks that key before it
appends. A re-driven call, or an orphaned one, finds the key already on the ledger
and charges nothing more. That is the difference between an Idempotent body and a
plain Write body: a Write killed mid-write parks the run for a human to reconcile
(see [`examples/reconciliation/`](../reconciliation/)), while an Idempotent one
recovers on its own and is still exactly-once.

This hand-rolled check predates `idempotency_keys`, the config-level mechanism
`agent_config.rs` documents and the main README's "No duplicate side effects"
bullet points to. The two are different patterns, worth telling apart. Here the
tool owns its own identity: `pay_employee` checks `pay_period:id` under its own
lock before it appends, so it stays exactly-once no matter what calls it or how
many times. `idempotency_keys` is the runtime owning identity instead: the
operator declares which input field names the operation, and the store refuses
a second execution across separate `salvor run` invocations, which is what
covers a tool that cannot make that promise on its own, most commonly a
third-party MCP server or a wasm component you did not write. Use
tool-owns-identity when the tool already sits in front of the durable state it
is protecting, as this one does. Use runtime-owns-identity when it does not.

## The tools, and why each effect is what it is

[`server.py`](server.py) is a four-tool MCP server in pure Python standard
library:

- **`pull_roster`, Read** (`readOnlyHint: true`). Reads [`roster.json`](roster.json),
  writes nothing. The hint is honest, so no override is needed.
- **`flag_exceptions`, Read** (`readOnlyHint: true`). Computes a verdict over the
  roster it is handed, writes nothing.
- **`pay_employee`, Idempotent** (`idempotentHint: true`). Appends one ledger line
  per employee, keyed by `pay_period:id`, and collapses a re-run of the same key.
  The agent pins it Idempotent besides, because the map body must recover on its
  own rather than park, and inheriting the safe hint and choosing it are different
  things.
- **`send_summary`, Write.** Appends one closing notice. It carries no annotation,
  which Salvor's conservative default already reads as a Write, and the agent pins
  it, because a re-run would add a second notice.

Every tool answers with `structuredContent` as well as human-readable text,
because Salvor records the WHOLE tool result as the node's output and the branch
and the map's `over` have to have something typed to read. That is why the branch
condition reads `structuredContent.flagged_count` and the map reads
`structuredContent.roster`: the paths are into the tool result, and a path that
does not resolve is false (for a branch) or a typed refusal (for a map's `over`),
never a silent wrong answer.

## What to unlearn

- **An agent node's output is a string, not a record.** A model turn ends in
  text, so an `agent` node hands its caller that text. It is why the roster review
  and the routing here are tool and branch nodes, not an agent "returning" a
  structured verdict: a branch reads typed fields, and only a tool result carries
  them. The one agent in this graph is the summary writer, whose output is meant to
  be prose.
- **A tool node cannot eat a list.** The map joins to a JSON array, and MCP
  arguments must be a JSON object, so the node after a map that needs to act on the
  whole batch is an agent (which reads any input and forms its own tool call), or a
  branch or another map (which route or fan out over the list), not a plain tool.
- **The gate's answer is the map's list, so its shape is load-bearing.** The map
  reads `structuredContent.roster`; the gate answer carries exactly that. Change
  one and you change the other. The `approval_schema` is where that contract is
  written down.
- **Idempotent is not a synonym for safe-by-default.** `pay_employee` is
  exactly-once because it keys on the employee and checks under a lock, not because
  the word "idempotent" was applied to it. A Write with no such key, killed
  mid-batch, is the reconciliation story, not this one.
- **The inline names here are a demo convenience, not a template.** The roster
  carries each employee's `name` straight through tool arguments and results, and
  it is synthetic data made up for this example. A deployment paying real people
  who can invoke erasure should pass a reference instead and resolve it against a
  system that can actually delete, the pattern
  [SECURITY.md](../../SECURITY.md#what-the-event-log-records) sets out for
  erasure-bound data.

## Files

- `payroll-run.json`: the graph document.
- `agents/notify-summary.toml`: the agent the document references by hash, and the
  file whose one MCP server carries the tools every tool node and the map body
  resolve against.
- `server.py`: the four-tool MCP server, standard library only.
- `roster.json`: the seed rosters `pull_roster` reads, one anomalous period and
  one clean one.
- `model-script.json`: the offline scripted model's one named conversation.
- `run.sh`: the whole walkthrough, offline, tearing down what it started.

The pay ledger and the notices are runtime state written to a scratch path, never
into this directory. See [`examples/graphs/README.md`](../graphs/README.md) for the
document format itself, [`examples/graph-service/`](../graph-service/) for the same
gate-and-branch shape without a map, and
[`examples/reconciliation/`](../reconciliation/) for the write-safety case a plain
Write body would land in.
