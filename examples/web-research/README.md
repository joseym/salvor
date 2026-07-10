# Example: a live web-research agent

This is a worked example, not a test. Where `demo/` is hermetic and scripted
(a canned MCP server, a mock model, no network), this one runs for real: the
public Anthropic endpoint drives `claude-opus-4-8`, and two MCP servers people
actually use do the work. The agent fetches a few web pages, reads them, and
writes one markdown report to disk. It is here to show how Salvor's durability
and effect classification behave against real tools, and to be a template you
can point at your own question.

## What is here

- `agent.toml`: the agent definition. Model, system prompt, budgets with
  pricing, and two MCP servers (`mcp-server-fetch` for pages,
  `@modelcontextprotocol/server-filesystem` for the report file). The
  effect overrides are grounded in what these servers actually declare; see
  the rationale below.
- `input.json`: the run input, passed with `--input @examples/web-research/input.json`.
  A research `question`, three stable Wikipedia `seed_urls`, and the
  `report_path` the agent writes to.
- `out/`: where the report lands. Kept in git (empty) because the filesystem
  server refuses to start if its allowed directory is missing; the reports
  themselves are gitignored.

## Prerequisites

1. **An Anthropic API key.** This run bills your account. Export it as
   `ANTHROPIC_API_KEY`; it is read at run time and never written to any file.
2. **Node**, for `npx`, which runs the filesystem server. Any current Node
   works; `npx` ships with it.
3. **A way to run the Python fetch server.** Two options:
   - **uv** (what `agent.toml` assumes). Install it from
     <https://docs.astral.sh/uv/>, and `uvx mcp-server-fetch` just works: uv
     fetches and runs the server in one step.
   - **pip and a venv**, if you would rather not install uv. Create a venv and
     install the server into it:
     ```sh
     python3 -m venv .venv-fetch
     .venv-fetch/bin/pip install mcp-server-fetch
     ```
     Then change the fetch server block in `agent.toml` to run it directly:
     ```toml
     command = ".venv-fetch/bin/python"
     args = ["-m", "mcp_server_fetch"]
     ```

## Running it

From the repository root:

```sh
cargo build
export ANTHROPIC_API_KEY=sk-ant-...
mkdir -p examples/web-research/out    # the filesystem server needs this to exist

./target/debug/salvor --store /tmp/salvor-web.db \
    run --agent examples/web-research/agent.toml \
        --input @examples/web-research/input.json
```

The run prints its id first (`run <uuid>`), before any step executes. It
fetches the seed pages, follows a link or two, writes
`out/raft-vs-paxos.md`, and prints a one-paragraph summary. Open the report:

```sh
cat examples/web-research/out/raft-vs-paxos.md
```

## The kill -9 walkthrough, and what durability buys here

Start the run in the background, let a couple of fetches land, then kill it
dead:

```sh
./target/debug/salvor --store /tmp/salvor-web.db \
    run --agent examples/web-research/agent.toml \
        --input @examples/web-research/input.json &
SALVOR_PID=$!

# watch the history until a fetch or two has completed, then:
./target/debug/salvor --store /tmp/salvor-web.db history <run-id>
kill -9 $SALVOR_PID
```

Resume from the durable log:

```sh
./target/debug/salvor --store /tmp/salvor-web.db \
    resume <run-id> --agent examples/web-research/agent.toml
```

Here is what that buys you, in real terms rather than demo terms:

- **Completed fetches are never re-fetched, and never re-paid.** Every fetch
  that finished before the kill is recorded in the event log with its result.
  On resume those calls are replayed from the log, not re-executed: no second
  HTTP request goes out, and no second model turn is billed to read the page
  back. You pay for each page once, across any number of crashes. On a slow
  crawl over large pages, that saving is substantial.
- **The report write is a Write, with reconciliation stakes.** Writing the
  report file is a real side effect. If the crash lands *after* the write was
  recorded, resume replays it from the log and does not write again. If the
  crash lands in the narrow window where the write may have reached the disk
  but was not yet recorded, resume does not guess and does not blindly retry:
  it parks the run as needing reconciliation and surfaces the recorded intent
  as evidence for a human to resolve. That is the safe behavior for a write
  whose outcome is genuinely unknown, and it is exactly why the effect class
  of `write_file` matters (below).

`salvor history <run-id>` after the resume shows one continuous log: everything
up to the kill is byte-identical to what was recorded before it.
`salvor replay --dry-run <run-id>` re-derives the run's state from the log
without executing anything.

## The effect overrides, and why they are the operator's call

MCP tool annotations are *hints*. The protocol is explicit that a server may
misstate them, so Salvor treats them conservatively and lets the operator pin a
tool's true effect class. This example pins two, each for a concrete reason we
confirmed by listing the servers' real tools:

- **`fetch` is pinned to `read`.** The fetch server ships it with no
  annotations at all. Salvor's default for an unannotated tool is Write, the
  safe guess for something unclassified. But we know what `fetch` does: an
  outbound GET that returns the page, with no server-side state to reconcile.
  Pinning it to `read` says so. The payoff is on resume: an in-flight fetch
  that crashed before it was recorded simply re-runs (a read re-executes
  freely), instead of parking the entire run to reconcile a "write" that never
  changed anything.
- **`write_file` is pinned to `write`.** The filesystem server annotates it
  `idempotentHint: true` (overwriting a file with identical bytes is, narrowly,
  idempotent). Salvor's default mapping would read that hint as Idempotent,
  which means "retry an interrupted call under the same key." That is not the
  stance we want for the report: saving it is a side effect we care about, so
  we pin it to `write`. An interrupted report write then surfaces for a human
  rather than being retried blind. The server's read tools (`read_file`,
  `list_directory`, and the rest) annotate `readOnlyHint: true` and map to Read
  on their own, so they need no override.

The shape of the rule: annotations come from a server you did not write and do
not fully trust; the override is where you record what you actually know, and
it takes precedence over the wire hint.

## Cost estimate for one run

At `claude-opus-4-8` list price ($5 per million input tokens, $25 per million
output tokens) a single run of this example costs roughly **$1 to $2**. The
fetch server returns pages in bounded chunks (about 5,000 characters by
default), so context stays modest, and a focused run is ten to twenty model
calls. The `cost_usd = 3.00` rail in `agent.toml` sits above that with room to
spare: crossing it parks the run rather than letting a runaway crawl keep
spending. Your actual number moves with how many links the model follows and
how large the pages are that day.

## A note on live-run variance

This is a real run, so it does not reproduce byte for byte, and that is fine.
Wikipedia pages change between runs. The model is nondeterministic: it may
follow different links, phrase the report differently, or take a different
number of steps. None of that undermines the durability guarantee, and in fact
it is the reason the guarantee is stated the way it is. Salvor's promise is narrower
than an identical answer: whatever a run did, its event log is the exact and
only record of it, a resume continues from that log without repeating a
completed step, and a write whose fate is unknown is reconciled rather than
guessed. The event log is what turns a nondeterministic live run
into something you can kill, inspect, and resume with confidence.
