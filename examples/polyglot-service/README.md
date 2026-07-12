# polyglot-service: Salvor as a durable backend from any language

Two apps, Python and TypeScript, drive one running Salvor control plane through
the language SDKs and do the same thing end to end:

1. register an agent,
2. start a run,
3. stream its events live,
4. handle a human-in-the-loop suspension by resuming it,
5. stream to completion.

This is the polyglot control-plane story. One durable Rust process (`salvor
serve`) owns the event store, drives runs, and enforces every guarantee: exact
replay, crash-safe resume, the write-ahead reconciliation rule. The clients
stay thin. They submit an agent and an input, read events, and grant a budget
extension when a run parks. Nothing about durability lives in the client, so
the Python app and the TypeScript app are mirror images of each other and of
any other language a client is written for.

Everything here runs offline. A scripted model server (`salvor-demo-model`)
stands in for a real endpoint, so no API key is needed.

## The suspension is real

The run parks on purpose, and the park is a genuine one held on the server, not
a client-side pause. The agent in `agent.toml` is the reference research agent
from `demo/agent.toml` with one change: its step budget is set low
(`max_steps = 8`). After eight model calls the run stops as `budget_exceeded`
and waits. The server has recorded that park in the log; the run would still be
parked after a server restart, and any client could pick it up.

Each app reads the park over HTTP, sees which budget was crossed, and resumes
with a budget extension. The resume body is exactly what
`POST /v1/runs/{id}/resume` accepts for a parked budget:

```json
{ "input": { "extend": { "steps": 40 } } }
```

The SDK's `resume(run_id, {"extend": {"steps": 40}})` wraps the extension under
`input` for you. The server validates it against the recorded budget shape
before recording anything, records a `Resumed` event carrying the extension,
and drives the run to completion. Because the extension lives in the log,
replay sees the same budget the live run saw. The extension of 40 steps clears
the 20-model-call run with headroom.

## Bring up the offline stack

From the repository root, build the binaries once:

```sh
cargo build
```

That produces `target/debug/salvor` (the control plane), `salvor-demo-model`
(the scripted offline model), and `salvor-demo-research` (the MCP server the
agent calls).

### The one-command path

```sh
examples/polyglot-service/run.sh
```

`run.sh` starts the model server on `127.0.0.1:8893`, starts `salvor serve` on
`127.0.0.1:8080` against a fresh temp store at `/tmp/salvor-polyglot.db`, runs
the Python app and then the TypeScript app, and tears both servers down on
exit.

### The manual path

If you would rather run each piece by hand, start the scripted model:

```sh
# from the repository root
./target/debug/salvor-demo-model --port 8893 --delay-ms 50 &
```

Then start the control plane, pointing the agent's model calls at that server
with `SALVOR_DEMO_BASE_URL`, and pointing the research tool's findings file at a
scratch path:

```sh
export SALVOR_DEMO_BASE_URL=http://127.0.0.1:8893
export SALVOR_DEMO_FINDINGS=/tmp/salvor-polyglot-findings.txt
./target/debug/salvor --store /tmp/salvor-polyglot.db serve --bind 127.0.0.1:8080 &
```

Now run each app against `http://127.0.0.1:8080`.

**Python** (the SDK's one dependency is `httpx`):

```sh
pip install -e sdks/python
python examples/polyglot-service/python/service.py http://127.0.0.1:8080
```

**TypeScript** (zero runtime dependencies; the example imports the SDK's built
output by relative path):

```sh
npm --prefix sdks/typescript install
npm --prefix sdks/typescript run build
node --experimental-strip-types \
    examples/polyglot-service/typescript/service.ts http://127.0.0.1:8080
```

When you are done, stop the two background servers (`kill %1 %2`, or whatever
job numbers they took).

## What you see

Each app prints one line per event. The first leg streams to the park:

```
started run da120de5-...
streaming until the run rests:
  seq  0  RunStarted
  ...
  seq 42  BudgetExceeded
  -- stream closed; run is budget_exceeded

run parked: budget_exceeded on steps (limit 8, observed 8)
resuming with budget extension {"extend": {"steps": 40}}
resume outcome: driving
streaming the continuation:
  seq 43  Resumed
  ...
  seq 101  RunCompleted
  -- stream closed; run is completed

final state: completed
summary: Research complete: 9 findings saved.
```

The continuation stream starts at the sequence after the park and never
repeats an event: the SDK tracks one cursor, and the server replays the log
from there before tailing live. The Python and TypeScript output is identical
line for line, confirming the two SDKs behave the same.

## Files

- `agent.toml`: the research agent with a low step budget so a run parks.
- `input.json`: the run input.
- `python/service.py`: the Python app, using the `salvor` SDK.
- `typescript/service.ts`: the TypeScript app, using `@salvor/client`.
- `run.sh`: brings the offline stack up, runs both apps, tears it down.

The SDK sources under `sdks/` are used unmodified. See
`crates/salvor-server/API.md` for the full HTTP and server-sent-events
contract these clients speak.
