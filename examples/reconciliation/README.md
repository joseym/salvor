# Example: reconciling a dangling write

This example demonstrates the one state Salvor refuses to guess about: a Write
tool call whose intent was recorded but whose completion never was, because the
process died mid-write. Salvor does not silently retry it and does not silently
skip it. It parks the run as needing reconciliation, shows the recorded intent
as evidence, and waits for a human to record what really happened with
`salvor resolve`. The result is a write that is never duplicated and never
silently retried.

Everything here runs offline with no API key: a scripted model server and a
one-tool MCP write server, both pure Python standard library.

## The durability contract, in one tool call

A Salvor tool call is two events, in this order:

1. `ToolCallRequested` (the intent), persisted **before** the tool runs.
2. `ToolCallCompleted` (the outcome), persisted **after** it returns.

This ordering is write-ahead: the intent is on disk before any side effect can
happen. For a `Read` or `Idempotent` tool, a crash between the two events is
safe to recover automatically, because re-running the tool either changes
nothing or collapses under the same idempotency key. For a `Write` tool it is
not safe: the write may have reached its target, partially applied, or never
run. Salvor cannot tell from the log alone, so it will not guess. The intent
sitting in the log with no completion is the evidence a human uses to decide.

The hard part of showing this is timing. A real write is fast, so an arbitrary
kill almost never lands in the narrow window between the two events. This
example removes the timing problem on purpose (see the next section).

## What is here

- `server.py`: a one-tool MCP write server, pure Python standard library, no
  `mcp` package and no venv. Its single tool, `commit_report`, is a genuine
  Write: it appends one line to a report file. The engineered part is that it
  performs the real write, flushes it to disk, and then **blocks** (sleeps)
  before returning. That block is the controllable window: while it sleeps, the
  intent is recorded and the write has landed, but the completion is not yet
  recorded. A kill during the sleep leaves the dangling write every time.
- `model_server.py`: a tiny offline scripted model, pure Python standard
  library, in the style of the repository's `salvor-demo-model`. It serves the
  Messages API shape and selects a response by conversation length: turn 1 asks
  to call `commit_report` once, turn 2 (served on resume, after the write is
  resolved) is a short final summary. No key, no network.
- `agent.toml`: the agent. Model routed offline through
  `base_url_env = "SALVOR_DEMO_BASE_URL"`, and one MCP server whose
  `commit_report` tool is pinned to `write`.
- `input.json`: the run input.
- `run.sh`: the whole sequence end to end (run, kill, refuse, resolve,
  complete), with the report file inspected at each stage.

The report file (`/tmp/salvor-reconciliation-report.txt`) is the side-effect
ledger: one line per real `commit_report` execution. Counting its lines across
the whole sequence is the exactly-once proof.

## The mechanism that strands the write, deterministically

`server.py`'s `commit_report` does its append, `fsync`s it, and then sleeps for
an hour. `run.sh` does not race a stopwatch. It waits until the line actually
appears in the report file, and only then kills the run. Because write-ahead
ordering guarantees the intent was recorded before the tool executed, the moment
the line is on disk the state is fixed: intent recorded, write landed, completion
not yet recorded. The kill lands in that state every time. This is the reliable,
offline way to produce `NeedsReconciliation` on demand.

The block chosen here is a plain long sleep, which needs no coordination and is
enough because the write is never re-run: on resume the dangling intent is
refused, and after resolve the completion is replayed from the log rather than
executed. A file or FIFO gate would work too; the sleep is the simplest thing
that reliably holds the process in the window.

## Running it

From the repository root:

```sh
./examples/reconciliation/run.sh
```

It builds `salvor`, starts the offline model on port 8891, and walks the four
stages below. What follows is the real output, stage by stage.

### Stage 1: run, then kill mid-write

The run performs its one write and blocks. `run.sh` waits for the line to land,
then `kill -9`s the process. The recorded log ends at the write intent:

```
   0  RunStarted           agent sha256:9dcabbe… input {"task":"Commit a one-line report…
   1  NowObserved          2026-…Z
   2  ModelCallRequested   request sha256:de8bd88…
   3  ModelCallCompleted   usage in 120 out 30
   4  ToolCallRequested    commit_report [Write] input {"content":"Write-ahead intents make a crash mid-write reconcilable…
```

Sequence 4 is a `Write` intent with no matching completion. That is the dangling
write. The report file already holds exactly one line, because the tool wrote it
before it blocked:

```
     1  Write-ahead intents make a crash mid-write reconcilable: the intent is recorded before the write, so resume refuses to guess and a human resolves.
```

### Stage 2: resume refuses, with the intent as evidence

`salvor resume` reads the log, sees the dangling write, and refuses. It exits 1
and prints the recorded intent so a human has something concrete to verify
against:

```
Run 7c4cfa0b-… needs reconciliation and cannot be resumed automatically.
A write tool call was recorded but never completed, so it may or may not have taken effect.

The recorded intent:
  seq:             4
  recorded at:     2026-…Z
  tool:            commit_report
  effect:          Write
  idempotency key: <none>
  input:
    {
      "content": "Write-ahead intents make a crash mid-write reconcilable: the intent is recorded before the write, so resume refuses to guess and a human resolves."
    }

Because the intent was durably recorded before the tool ran, the write may have
reached its target, partially applied, or never run at all. Salvor will not guess.

There are two honest outcomes. Both begin by verifying externally whether the write
took effect, and both end by recording the completion so replay never re-runs it:
  1. The write took effect. Record what the tool returned:
       salvor resolve 7c4cfa0b-… --output '<json the tool returned>'
  2. The write did not take effect and still needs to happen. Perform it yourself
     first, then record its result the same way. There is no automatic retry for a write.
```

Salvor neither retries the write behind your back (which would duplicate it if
it had landed) nor assumes it landed (which would lose it if it had not). It
hands you the evidence and the two honest paths.

### The two honest paths

Both start the same way: verify, outside Salvor, whether the write actually took
effect. Here that means looking at the report file.

- **The write took effect** (this example's case: the file holds the line).
  Record what the tool returned, so replay reproduces that completion and never
  re-runs the write:

  ```sh
  salvor resolve <run-id> --output '{"content":[{"type":"text","text":"committed report to /tmp/salvor-reconciliation-report.txt"}],"isError":false}'
  ```

- **The write did not take effect** (the file is missing the line). Perform the
  write yourself first, by hand, then record the same completion with the same
  `resolve` call. There is no automatic retry, because a blind retry of a write
  whose fate is unknown is exactly the duplicate Salvor exists to prevent.

Either way, `resolve` appends exactly one event (the missing `ToolCallCompleted`)
and executes nothing itself.

### Stage 3: resolve the dangling write

```
Run 7c4cfa0b-… resolved: recorded the missing write completion by hand.
The run no longer needs reconciliation. Continue it with:
  salvor resume 7c4cfa0b-… --agent <agent.toml>
```

### Stage 4: resume completes the run

Now `salvor resume` recovers cleanly. It replays the log, including the write
completion just recorded, without executing the tool again, then drives the one
remaining model turn and finishes:

```
"Report committed: one durable write, resolved by hand after the crash."
```

The report file still holds exactly one line:

```
     1  Write-ahead intents make a crash mid-write reconcilable: the intent is recorded before the write, so resume refuses to guess and a human resolves.
line count: 1
PROOF: the write executed exactly once.
```

One line, across the whole run, kill, resolve, and resume. The write happened
once (the tool did it before the kill), it was reconciled by a human rather than
guessed, and resume never repeated it.

## Why this is the safe behavior

The recorded intent records that a side effect was attempted and its outcome is
unknown. For a write, an unknown outcome is the case a durable system must
surface rather than hide. A silent retry risks a
duplicate; a silent skip risks a lost write. Salvor takes neither guess. It
surfaces the intent, refuses to continue, and lets a human record reality, which
is the only source of truth about whether a write landed. `salvor resolve` is
how that human answer becomes a durable part of the log, so every later replay
agrees with it and nothing runs twice.
