# salvor (Python)

A thin Python client for the Salvor control plane.

## What the control plane is

Salvor is a durable execution runtime for AI agents. A run is an append-only
log of events: every model call and every tool call is recorded before the run
moves on, so a process that dies mid-flight is recovered from the log and
finished from exactly where it stopped, with no completed step run twice.

The control plane is a small HTTP and server-sent-events server that puts that
runtime on a network. It owns one event store and drives runs in the
background. You submit an agent definition and an input, then read the run's
events as they land. The full contract is in
`crates/salvor-server/API.md`.

## Why the SDK is thin

The durability guarantees stay in one Rust process. Exact replay, crash-safe
resume, and the write-ahead rule that parks a run whose write was recorded but
never completed all live server-side, enforced by the same runtime the CLI
uses. So this SDK is a few hundred lines: it submits data, reads events, and
maps the server's error envelope to exceptions. It holds no agent loop, no
run state, and no durability logic of its own. Because the server does all the work,
the SDK stays consistent with it by construction.

## Install

```sh
pip install -e sdks/python        # from the repository root
```

The one runtime dependency is `httpx`.

## The client surface

```python
from salvor import Client

client = Client("http://127.0.0.1:8080", token=None)

agent    = client.register_agent(toml_or_dict)      # -> agent hash
run_id   = client.start_run(agent, input=None)      # -> run id
state    = client.get_run(run_id)                   # -> RunState
runs     = client.list_runs()                       # -> list[RunSummary]
stream   = client.stream_events(run_id, from_seq=None)  # -> EventStream
result   = client.resume(run_id, input=None)        # -> ResumeResult
state    = client.resolve(run_id, output)           # record a dangling write
projected = client.replay(run_id)                   # -> ReplayState (dry run)
```

`register_agent` accepts a TOML string (sent as `application/toml`) or a dict
of the same fields (sent as `application/json`). An agent is data, so it has a
content hash; submit it once and reference it by that hash on every start.

## The streaming and cursor model

`stream_events` returns an `EventStream` you iterate for
[`Event`](salvor/models.py) objects in sequence order:

```python
stream = client.stream_events(run_id)
for event in stream:
    print(event.seq, event.kind)
print(stream.end.status.state)   # the resting status the end frame carried
```

On connect the server replays every recorded event at or after the cursor,
then tails new events as they land, then sends one terminal `end` frame and
closes. A run's log has contiguous, ascending sequence numbers, so the stream
is gap-free and duplicate-free by construction, and the client only has to
track one number: the next sequence to expect.

That same number is what makes a dropped connection recoverable. If the socket
drops mid-tail, the client reconnects with `?from_seq=<next>` and the server
resumes from there. Any event that arrived just before the drop is skipped by
sequence number, so the merged stream stays gap-free and duplicate-free across
the reconnect. Iteration stops at the `end` frame; its status (and a
`detached` flag, set when the run is mid-step with no driver in this server
process) is then on `stream.end`.

## Errors

Every server error is decoded from the one JSON envelope
(`{"error": {"code", "message", "details?}}`) into a `SalvorAPIError` carrying
the stable `code` and the `message`. The one refusal with structured evidence,
a resume blocked because a write was recorded but never completed, raises
`NeedsReconciliationError`, whose `.intent` is the recorded write. Verify what
that write did, then call `resolve(run_id, output)` to record its completion so
replay never re-runs it.

```python
from salvor import NeedsReconciliationError

try:
    client.resume(run_id)
except NeedsReconciliationError as e:
    print("stuck on write:", e.intent.get("tool"), e.intent.get("input"))
    client.resolve(run_id, output={"charged": True})
    client.resume(run_id)
```

## Runnable example

`example/agent.toml` is a model-only agent that answers one question. It is the
Python mirror of `examples/web-research`, driven over the control plane instead
of the CLI. Start a server with a key on its environment, then run the script:

```sh
# from the repository root
cargo build --bin salvor

ANTHROPIC_API_KEY=sk-ant-... \
    ./target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db &

pip install -e sdks/python
python sdks/python/example/answer.py http://127.0.0.1:8080
```

It registers the agent, starts a run, streams every event to completion, and
prints the final answer, the event count, and the token usage.
