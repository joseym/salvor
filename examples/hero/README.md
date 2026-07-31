# The hero fixture

The run behind the terminal on [salvor.run](https://salvor.run), checked in so
you can produce the same events on your own disk, in your own SQLite file.

It is deliberately the smallest agent that still shows the whole behaviour: one
model call decides to record a claim, one tool call records it, one more model
call closes the run out. Ten events, exactly one write.

```sh
# the CLI, however you like it:
npm install -g @salvor-run/cli      # or: cargo install salvor-cli
# or, from a checkout of this repository:
cargo build

salvor run --fixture examples/hero
```

The commands on this page call the binary as `salvor`, which is right once one
of the install routes above has put it on your `PATH`. `cargo build` alone
does not do that: from a checkout with no global install, run
`./target/debug/salvor` in place of every `salvor` below. See
[`examples/README.md`](../README.md) for how the other examples name this
same distinction with a `SALVOR_BIN` variable.

No API key and no network. `--fixture` reads this directory's `agent.toml` and
`input.json`, starts the recorded model in `model.json` on a local port, and
points the agent at it.

## What you should see

```
   0  RunStarted           agent sha256:… input {"item":"ss-waratah"}
   1  NowObserved          2026-07-27 20:55:44Z
   2  ModelCallRequested   request sha256:…
   3  ModelCallCompleted   usage in 24 out 41
   4  ToolCallRequested    save_claim [Write] input {"item":"ss-waratah"}
   5  ToolCallCompleted    output {"content":[{"text":"claim recorded: ss-waratah",…
   6  NowObserved          2026-07-27 20:55:44Z
   7  ModelCallRequested   request sha256:…
   8  ModelCallCompleted   usage in 96 out 38
   9  RunCompleted         output "1 claim recorded for ss-waratah."
```

Read it back at any time with `salvor history <run-id>`.

Two things to note. Event 4 records the tool call **before** the tool runs,
which is what the next section depends on. Events 1 and 6 are the driver's
clock observations, one per loop iteration: a replay has to see the same
`now()` the first run saw, so the reading is logged rather than read from the
ambient clock.

## Now kill it

`save_claim` appends one line to a claims file per real execution, so the file
itself is the proof. Count the lines before and after.

```sh
export SALVOR_HERO_CLAIMS=/tmp/claims.txt
rm -f /tmp/claims.txt

salvor run --fixture examples/hero &
kill -9 $!

wc -l /tmp/claims.txt                    # 0 or 1, depending where it died
salvor resume <run-id> --agent examples/hero/agent.toml
wc -l /tmp/claims.txt                    # still 1. never 2.
```

A resume replays the committed events as state and re-executes none of them.
If the kill landed between event 4 and event 5, where the write was recorded but
its result never was, the resume refuses rather than guessing, because the log
cannot tell it whether that write landed:

```
Run … needs reconciliation and cannot be resumed automatically.
A write tool call was recorded but never completed, so it may or may not have taken effect.

The recorded intent:
  seq:             4
  tool:            save_claim
  effect:          Write
  …
```

There is no `--force`. Both honest paths start with you checking
`/tmp/claims.txt` yourself. If the write landed, record what the tool returned
so replay never re-runs it:

```sh
salvor resolve <run-id> --output '{"content":[{"type":"text","text":"claim recorded: ss-waratah"}]}'
```

If it did not land and still needs to happen, perform it yourself first, then
record the result the same way. There is no automatic retry for a write.

This is the same rule the release gate asserts at every one of the run's event
boundaries, in
[`crates/salvor-runtime/tests/release_gate.rs`](../../crates/salvor-runtime/tests/release_gate.rs).

## Against a real model

The same `agent.toml` drives a real run. Leave `SALVOR_HERO_BASE_URL` unset so
the endpoint is the public one, and export a key:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
salvor run --agent examples/hero/agent.toml --input @examples/hero/input.json
```

The model will phrase its confirmation differently, and the token counts will
not match the recording. The event sequence, the commit-before-act ordering and
the write-ahead rule are all the same runtime doing the same thing.

## The files

| File | What it is |
|---|---|
| `agent.toml` | The agent. Two modes, switched by `SALVOR_HERO_BASE_URL`. |
| `input.json` | The run input. |
| `model.json` | The recorded model conversation, keyed by message count so it is replay-safe across a kill. |
