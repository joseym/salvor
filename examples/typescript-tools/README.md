# Example: extending a Salvor agent with a TypeScript MCP server

This is the same v0.1 polyglot story as [`../python-tools/`](../python-tools/),
told in the other launch language. A Salvor agent gets three new tools, a
bookmarks manager, and every one of them is a plain Node function in
[`server.mjs`](server.mjs). There is no Salvor package imported anywhere in this
directory, no binding compiled, no SDK beyond `@modelcontextprotocol/sdk`
itself. This demonstrates the polyglot claim: **a TypeScript developer extends a
Salvor agent by writing an MCP server. No bindings, no SDK, no Salvor code.** Your
TypeScript is the tool layer; Salvor reaches it over stdio.

## What is here

- `server.mjs`: a bookmarks-manager MCP server, commented for a TypeScript or
  JavaScript developer meeting MCP for the first time. Three tools:
  `save_bookmark` (appends one JSON line to a store file), `list_bookmarks`, and
  `find_bookmark`. `McpServer.registerTool` (the current SDK 1.29.0 API) turns
  each into a tool: a zod schema becomes the input schema, and the config
  object carries the description and side-effect annotations.
- `agent.toml`: the agent definition. Model, system prompt, budgets with
  pricing, and one MCP server (Node running `server.mjs`). The single effect
  override is grounded in what the server actually advertises; see below.
- `input.json`: a handful of pages to bookmark, passed with
  `--input @examples/typescript-tools/input.json`.
- `package.json`: the two dependencies. `node_modules/` and the
  `bookmarks.jsonl` store are not committed.

### Why `.mjs` and not TypeScript

`server.mjs` is plain ESM JavaScript on purpose: it is the lowest-friction
runnable form. Node executes it directly, with no compile step and no `tsx` to
download; `npm install` fetches only the MCP SDK and zod. A `.ts` file run
through `npx tsx` would add a toolchain and a first-run download for no gain in
an example this size. The MCP SDK ships its own types, so an editor still gives
you full type information as you read the file.

## Setup

From the repository root:

```sh
( cd examples/typescript-tools && npm install )
```

That installs the MCP SDK and zod into `examples/typescript-tools/node_modules`.
You also need `node` on your `PATH`, which `agent.toml` spawns the server with.

## Running it

```sh
# the CLI, however you like it:
npm install -g @salvor-run/cli      # or: cargo install salvor-cli
# or, from a checkout of this repository:
cargo build

export DEMO_ANTHROPIC_API_KEY=sk-ant-...

salvor --store /tmp/salvor-typescript.db \
    run --agent examples/typescript-tools/agent.toml \
        --input @examples/typescript-tools/input.json
```

The run prints its id first (`run <uuid>`), then saves each page with one
`save_bookmark` call, lists them once, and prints a short summary. The paths in
`agent.toml` are relative to the repository root, so run it from there. Inspect
the store:

```sh
cat examples/typescript-tools/bookmarks.jsonl
```

An Anthropic API key is required; the run bills your account. It is read from
`DEMO_ANTHROPIC_API_KEY` at run time and never written to any file. The name is
the agent file's choice via `api_key_env`; a dedicated demo variable keeps a
walkthrough run from spending your primary `ANTHROPIC_API_KEY`. A subscription
OAuth token works too: set `api_key_kind = "oauth"` in `[llm]` and export the
token (an `sk-ant-oat...` value) as `DEMO_ANTHROPIC_API_KEY`.

A full run of the five pages in `input.json` stays well under the
`cost_usd = 0.50` rail in `agent.toml`; saving bookmarks is cheap, a dozen or so
short model calls.

## The kill/resume story, with the store as duplicate-witness

`save_bookmark` appends a line. If a crash and resume re-ran a completed save,
the store would grow a duplicate line, and the count would give it away. It does
not grow one, and the store is the evidence.

Start the run, let a few bookmarks land, then kill the process dead:

```sh
salvor --store /tmp/salvor-typescript.db \
    run --agent examples/typescript-tools/agent.toml \
        --input @examples/typescript-tools/input.json &
SALVOR_PID=$!

# once a few lines have appeared, note the count, then kill:
wc -l examples/typescript-tools/bookmarks.jsonl
kill -9 $SALVOR_PID
```

Resume from the durable log:

```sh
salvor --store /tmp/salvor-typescript.db \
    resume <run-id> --agent examples/typescript-tools/agent.toml

wc -l examples/typescript-tools/bookmarks.jsonl
```

Every `save_bookmark` that finished before the kill is recorded in the event log
with its result; on resume those calls are replayed from the log, never
re-executed, so no page is saved twice. When the kill lands cleanly (during a
model call, or between recorded steps), the resumed run completes and the final
line count equals the number of pages in `input.json`, not more.
`salvor history <run-id>` after the resume shows one continuous log, identical up
to the kill point; `salvor replay --dry-run <run-id>` re-derives the run's state
from the log without executing anything.

There is a second, equally correct outcome, and it is the reason `save_bookmark`
is a Write. If the kill lands in the narrow window where an append reached the
store but its completion was not yet recorded, resume does not guess and does
not blindly retry: it parks the run as needing reconciliation and surfaces the
recorded write intent for a human to resolve (`salvor resume` prints
`needs reconciliation and cannot be resumed automatically`). The store still
does not grow a duplicate, because the attempted-but-unrecorded write is never
re-run. That refusal to guess is exactly what pinning the tool to `write` buys,
and why an Idempotent classification (which would retry the append and duplicate
the line) would be wrong here.

Reconciliation requires a human first, then the run continues. Check what
actually happened before telling Salvor anything: open
`examples/typescript-tools/bookmarks.jsonl` and compare its last line against
the bookmark `salvor history <run-id>` shows for the pending `save_bookmark`
call.

- If the line is there, the append reached disk before the kill landed. The
  write happened; record what it wrote:

  ```sh
  salvor --store /tmp/salvor-typescript.db \
      resolve <run-id> --output '{"content":[{"type":"text","text":"Saved \"The Raft Consensus Algorithm\"."}]}'
  ```

- If the line is missing, the append never reached the store. Append it by
  hand, with the same url, title, and tags the model was recording, then
  record that same completion with the same `resolve` call.

Either way `resolve` appends exactly one event, the missing
`ToolCallCompleted`, and executes nothing itself: it takes a human's word for
what happened, not a guess. The run is no longer stuck. Continue it with
`salvor resume <run-id> --agent examples/typescript-tools/agent.toml`, exactly
as `resolve` tells you to.

## The effect override, and why it is the operator's call

MCP tool annotations are hints, and the protocol is explicit that a server may
state them incorrectly. Salvor treats them conservatively and lets the operator
pin a tool's true effect class. This example pins one, for a concrete reason you
can confirm by listing the server's tools:

- **`save_bookmark` is pinned to `write`.** `server.mjs` annotates it
  `idempotentHint: true`. Read literally, that says "retrying this call under
  the same key is harmless," and Salvor's default mapping would classify it as
  Idempotent and auto-retry an interrupted call. But the tool APPENDS a line to
  the store, so a retry does not restore the same state, it writes a second,
  duplicate bookmark. The hint is wrong for this tool, and the override exists to
  correct exactly this kind of misstatement. Pinning `write` says what
  the tool really does: an interrupted append surfaces for a human to reconcile
  rather than being retried blind.
- **`list_bookmarks` and `find_bookmark` need no override.** They annotate
  `readOnlyHint: true` and only ever read the store, so Salvor classifies them as
  Read on their own.

The shape of the rule: annotations come from a server you may not fully trust;
the override is where you record what you actually know, and it wins over the
wire hint.
