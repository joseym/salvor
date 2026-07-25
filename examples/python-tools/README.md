# Example: extending a Salvor agent with a Python MCP server

This example is the v0.1 polyglot story from one language over. A Salvor agent
gets three new tools, an expense tracker, and every one of them is a plain
Python function in [`server.py`](server.py). There is no Salvor package imported
anywhere in this directory, no binding compiled, no SDK beyond `mcp` itself.
This demonstrates the polyglot claim: **a Python developer extends a Salvor
agent by writing an MCP server. No bindings, no SDK, no Salvor code.** Your
Python is the tool layer; Salvor reaches it over stdio.

## What is here

- `server.py`: an expense-tracker MCP server, about eighty lines, commented for
  a Python developer meeting MCP for the first time. Three tools: `add_expense`
  (appends one JSON line to a ledger file), `list_expenses`, and
  `total_by_category`. `FastMCP` turns each decorated function into a tool: its
  type hints become the input schema, its docstring becomes the description.
- `agent.toml`: the agent definition. Model, system prompt, budgets with
  pricing, and one MCP server (the venv Python running `server.py`). The single
  effect override is grounded in what the server actually advertises; see below.
- `input.json`: a handful of natural-language expenses to log and summarize,
  passed with `--input @examples/python-tools/input.json`.
- `ledger.jsonl`: not committed. The server appends here as the agent logs
  expenses. It is the durable state, and the duplicate-witness for the
  kill/resume check below.

## Setup

From the repository root:

```sh
python3 -m venv examples/python-tools/.venv
examples/python-tools/.venv/bin/pip install mcp
```

That is the only dependency. `agent.toml` runs the server through this venv's
interpreter so the `mcp` package is on its path.

## Running it

```sh
cargo build
export DEMO_ANTHROPIC_API_KEY=sk-ant-...

salvor --store /tmp/salvor-python.db \
    run --agent examples/python-tools/agent.toml \
        --input @examples/python-tools/input.json
```

The run prints its id first (`run <uuid>`), then logs each expense with one
`add_expense` call, totals them once, and prints a short summary. The paths in
`agent.toml` are relative to the repository root, so run it from there. Inspect
the ledger and the totals:

```sh
cat examples/python-tools/ledger.jsonl
```

An Anthropic API key is required; the run bills your account. It is read from
`DEMO_ANTHROPIC_API_KEY` at run time and never written to any file. The name is
the agent file's choice via `api_key_env`; a dedicated demo variable keeps a
walkthrough run from spending your primary `ANTHROPIC_API_KEY`. A subscription
OAuth token works too: set `api_key_kind = "oauth"` in `[llm]` and export the
token (an `sk-ant-oat...` value) as `DEMO_ANTHROPIC_API_KEY`.

A full run of the six expenses in `input.json` stays well under the
`cost_usd = 0.50` rail in `agent.toml`; logging expenses is cheap, a dozen or so
short model calls.

## The kill/resume story, with the ledger as duplicate-witness

`add_expense` appends a line. If a crash and resume re-ran a completed append,
the ledger would grow a duplicate line, and the count would give it away. It
does not grow one, and the ledger is the evidence.

Start the run, let a few expenses land, then kill the process dead:

```sh
salvor --store /tmp/salvor-python.db \
    run --agent examples/python-tools/agent.toml \
        --input @examples/python-tools/input.json &
SALVOR_PID=$!

# once a few lines have appeared, note the count, then kill:
wc -l examples/python-tools/ledger.jsonl
kill -9 $SALVOR_PID
```

Resume from the durable log:

```sh
salvor --store /tmp/salvor-python.db \
    resume <run-id> --agent examples/python-tools/agent.toml

wc -l examples/python-tools/ledger.jsonl
```

Every `add_expense` that finished before the kill is recorded in the event log
with its result; on resume those calls are replayed from the log, never
re-executed, so no expense is logged twice. When the kill lands cleanly (during
a model call, or between recorded steps), the resumed run completes and the
final line count equals the number of expenses in `input.json`, not more.
`salvor history <run-id>` after the resume shows one continuous log, identical up
to the kill point; `salvor replay --dry-run <run-id>` re-derives the run's state
from the log without executing anything.

There is a second, equally correct outcome, and it is the reason `add_expense`
is a Write. If the kill lands in the narrow window where an append reached the
ledger but its completion was not yet recorded, resume does not guess and does
not blindly retry: it parks the run as needing reconciliation and surfaces the
recorded write intent for a human to resolve (`salvor resume` prints
`needs reconciliation and cannot be resumed automatically`). The ledger still
does not grow a duplicate, because the attempted-but-unrecorded write is never
re-run. That refusal to guess is exactly what pinning the tool to `write` buys,
and why an Idempotent classification (which would retry the append and duplicate
the line) would be wrong here.

Reconciliation requires a human first, then the run continues. Check what
actually happened before telling Salvor anything: open
`examples/python-tools/ledger.jsonl` and compare its last line against the
amount and category `salvor history <run-id>` shows for the pending
`add_expense` call.

- If the line is there, the append reached disk before the kill landed. The
  write happened; record what it wrote:

  ```sh
  salvor --store /tmp/salvor-python.db \
      resolve <run-id> --output '{"content":[{"type":"text","text":"Recorded $8.75 in Food."}]}'
  ```

- If the line is missing, the append never reached the ledger. Append it by
  hand, with the same amount and category the model was recording, then
  record that same completion with the same `resolve` call.

Either way `resolve` appends exactly one event, the missing
`ToolCallCompleted`, and executes nothing itself: it takes a human's word for
what happened, not a guess. The run is no longer stuck. Continue it with
`salvor resume <run-id> --agent examples/python-tools/agent.toml`, exactly as
`resolve` tells you to.

## The effect override, and why it is the operator's call

MCP tool annotations are hints, and the protocol is explicit that a server may
state them incorrectly. Salvor treats them conservatively and lets the operator
pin a tool's true effect class. This example pins one, for a concrete reason you
can confirm by listing the server's tools:

- **`add_expense` is pinned to `write`.** `server.py` annotates it
  `idempotentHint: true`. Read literally, that says "retrying this call under
  the same key is harmless," and Salvor's default mapping would classify it as
  Idempotent and auto-retry an interrupted call. But the tool APPENDS a line to
  the ledger, so a retry does not restore the same state, it writes a second,
  duplicate expense. The hint is wrong for this tool, and the override exists to
  correct exactly this kind of misstatement. Pinning `write` says what
  the tool really does: an interrupted append surfaces for a human to reconcile
  rather than being retried blind.
- **`list_expenses` and `total_by_category` need no override.** They annotate
  `readOnlyHint: true` and only ever read the ledger, so Salvor classifies them
  as Read on their own.

The shape of the rule: annotations come from a server you may not fully trust;
the override is where you record what you actually know, and it wins over the
wire hint.
