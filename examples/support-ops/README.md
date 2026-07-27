# Example: a support-triage agent

This is Salvor's differentiators shown on a scenario a support team would
recognize rather than a research demo. An agent is handed a batch of ticket ids. For
each one it reads the ticket and the order behind it, drafts a reply grounded
in what it actually found, posts that reply, and sets the ticket's resulting
status. The tool layer is one Python MCP server
([`server.py`](server.py)), the same polyglot story as
[`../python-tools/`](../python-tools/): no Salvor package imported, no
binding compiled, no SDK beyond `mcp` itself.

## The scenario

Three seed tickets ([`tickets.json`](tickets.json)) against two seed orders
([`orders.json`](orders.json)) stand in for a ticket system and an order
system a real desk would reach over an internal API:

- **T-1001**, a shipment that stalled at label creation, six days with no
  carrier scan.
- **T-1002**, the wrong item delivered.
- **T-1003**, a billing question with no order attached to the ticket, that
  names an order in its message instead.

[`input.json`](input.json) hands the agent all three ids. The system prompt
in [`agent.toml`](agent.toml) tells it to look each one up, check the order
if there is one, post one grounded reply, and set one resulting status
(`resolved`, `pending`, or `escalated`) before moving to the next ticket.

## The tools, and why each effect is what it is

Four tools, a real mix, each with an effect that is either declared by the
server's annotations or pinned by the operator in `agent.toml` because the
annotation would be wrong or is silent. List them yourself and see the raw
annotations with the handshake in **Verify** below.

- **`lookup_ticket`, Read.** Reads the seed ticket and folds in any runtime
  state for it (a status override, posted replies). `readOnlyHint: true` in
  the server's annotation is enough on its own; Salvor classifies it Read
  with no override. A Read is safe to re-run: an interrupted or duplicated
  call just re-reads the same thing.
- **`get_order_status`, Read.** Reads `orders.json` only, which this server
  never writes. Also `readOnlyHint: true`, also no override needed.
- **`post_reply`, Write (overridden).** Appends one line to a runtime
  activity log; the customer sees this message. The server annotates it
  `idempotentHint: true`, which read literally says a retry under the same
  key is harmless. It is not: the tool APPENDS, so retrying an interrupted
  call posts the reply a second time and the customer sees it twice.
  `agent.toml` overrides this tool to `write`, so an interrupted post
  surfaces for a human to reconcile instead of being retried blind. This is
  the identical hazard `examples/python-tools/` and `examples/typescript-tools/`
  walk through with their append tools; see either README for the kill/resume
  story in full.
- **`set_ticket_status`, Idempotent (no override, and correctly so).** Also
  annotated `idempotentHint: true`, and this time the hint is right: the tool
  upserts one key in a dict keyed by ticket id and rewrites the whole file.
  Calling it three times with the same status leaves the exact same file
  content one call would. A retry restores the same state instead of
  duplicating anything, which is what idempotent means here.
  `agent.toml` takes no override here, because
  trusting the hint is correct for this tool.

The last two tools carry the identical `idempotentHint: true` annotation and
get opposite treatment. The annotation is a hint from a server you may not
fully trust, and Salvor's effect system exists so the operator can say what a
tool actually does, in either direction: an override when the hint is wrong and
none when it is right.

## The budget rail

`[budgets]` in `agent.toml` caps the run at 40 steps, 300k tokens, $1.00, and
450 seconds of wall time. Three tickets at up to four tool calls each, plus
the model's reasoning between calls, comfortably fits inside that; the rail
exists to catch a run that has clearly gone wrong (a loop re-reading the same
ticket, a model that won't stop asking clarifying questions) rather than to
pinch a normal one. A budget in Salvor suspends the run rather than killing
it: crossing a cap parks the run for a human to inspect and either extend the
budget and resume, or stop it there. Nothing is lost either way, because
every step up to the cap is already in the durable event log.

## Setup

From the repository root:

```sh
python3 -m venv examples/support-ops/.venv
examples/support-ops/.venv/bin/pip install mcp
```

That is the only dependency. `agent.toml` runs the server through this
venv's interpreter so the `mcp` package is on its path.

## Running it

```sh
# This example spawns the demo fixture binaries, which ship with the cargo install but not with
# the npm package:
cargo install salvor-cli            # or, from a checkout: cargo build

export DEMO_ANTHROPIC_API_KEY=sk-ant-...

salvor --store /tmp/salvor-support-ops.db \
    run --agent examples/support-ops/agent.toml \
        --input @examples/support-ops/input.json
```

The run prints its id first (`run <uuid>`), then works each ticket: a
`lookup_ticket`, usually a `get_order_status`, one `post_reply`, one
`set_ticket_status`, and finally a short summary. Paths in `agent.toml` are
relative to the repository root, so run it from there. Inspect what
actually happened:

```sh
cat examples/support-ops/activity.jsonl
cat examples/support-ops/status_overrides.json
```

Neither file exists until the agent's first write; both are runtime state
and are not committed (see `.gitignore`). An Anthropic API key is required
for a full run; it is read from `DEMO_ANTHROPIC_API_KEY` at run time and
never written to any file. A subscription OAuth token works too: set
`api_key_kind = "oauth"` in `[llm]` and export the token as
`DEMO_ANTHROPIC_API_KEY`.

## What was verified offline, and what needs a model

There is no scripted offline model here, unlike the research demo under
`demo/`. `salvor-demo-model` is scripted for that agent's exact
question-answering shape; it would not produce sensible ticket triage calls,
and faking a run against it would misrepresent what this example shows. The
honest offline claim is narrower, and everything below was actually run, not
assumed:

**The MCP server starts and advertises the right effects.** A direct MCP
handshake against `server.py` (no salvor, no model) listed all four tools and
printed their raw annotations:

```
- lookup_ticket: readOnlyHint=True
- get_order_status: readOnlyHint=True
- post_reply: idempotentHint=True
- set_ticket_status: idempotentHint=True
```

which is exactly the mix described above: two reads self-declared correctly,
and two writes carrying the same hint that Salvor's operator overrides
disambiguate. The same handshake called all four tools directly: a
`lookup_ticket` before and after a `post_reply` and a `set_ticket_status`
showed the reply and the new status folded in, and calling `set_ticket_status`
a second time with the same value left `status_overrides.json` byte-identical,
confirming it really is idempotent and not just hinted to be.

**`agent.toml` validates and wires the server.** Running the agent for real
(with a deliberately invalid key, so no account is billed) confirms the whole
non-model path: config parsed, budgets and pricing accepted, the MCP server
spawned and its tools registered onto the agent, a run created in the store,
and only then a clean failure at the model call:

```sh
export DEMO_ANTHROPIC_API_KEY=sk-ant-invalid-for-offline-verification
salvor --store /tmp/salvor-support-ops.db \
    run --agent examples/support-ops/agent.toml \
        --input @examples/support-ops/input.json
```

```
run e4cf2454-e366-4388-a76f-af40f8e0588e
salvor: model call: Messages API returned HTTP 401 (authentication_error): invalid x-api-key
```

`salvor history <run-id>` after that shows exactly `RunStarted`,
`NowObserved`, `ModelCallRequested`, then nothing: the run parked
`awaiting-model` before any tool was ever called, which is why no
`activity.jsonl` or `status_overrides.json` appeared from this check. That is
as far as offline goes.

**What needs a model.** Everything past that line: reading a ticket and
deciding whether the order behind it is actually a delivery, a wrong-item, or
a billing problem; writing a reply that is specific to what the tools
returned rather than generic; and choosing `resolved` versus `pending` versus
`escalated` with real judgment. A live Anthropic model, or a capable local
model reachable through the same `[llm]` settings, is required to see the
triage decisions themselves; nothing here fakes that part.
