# The Salvor demo: kill -9 a research agent and resume it with no lost or repeated work

This directory holds the reference workload behind Salvor's headline claim:
a ~20-step research agent you can `kill -9` mid-run and resume with an
identical event history and zero repeated side effects.

## What is here

- `agent.toml`: the agent definition. Model, system prompt, budgets, and
  one MCP server (`salvor-demo-research`, built from this repository). The
  same file drives both a real Anthropic endpoint and a mock one; see the
  two modes below.
- `input.json`: the run input, passed with `--input @demo/input.json`.
- The MCP server itself lives at
  `crates/salvor-cli/src/bin/demo_research.rs`. It is hermetic and
  deterministic: `search_notes` (read-only) answers from a canned library,
  `save_finding` (write) appends one line to a findings file, and
  `get_finding_count` (read-only) reports the line count. The findings file
  is the side-effect ledger: one line per real write execution, so counting
  lines before the kill and after the resume is the zero-duplicate proof.

The run's shape: nine search/save subtopic pairs, one count check, one
final summary. Twenty model calls, nineteen tool calls, nine durable
writes.

## The walkthrough (real model)

From the repository root:

```sh
cargo build
export ANTHROPIC_API_KEY=sk-ant-...
export SALVOR_DEMO_FINDINGS=/tmp/salvor-demo-findings.txt
rm -f "$SALVOR_DEMO_FINDINGS"

./target/debug/salvor --store /tmp/salvor-demo.db \
    run --agent demo/agent.toml --input @demo/input.json &
SALVOR_PID=$!
```

The run prints its id first (`run <uuid>`), before any step executes, so
you can copy it even after the kill. Watch the findings accumulate, and
around step 11 (a few findings in), kill the process for real:

```sh
wc -l "$SALVOR_DEMO_FINDINGS"     # some findings landed, say 4
kill -9 $SALVOR_PID
```

Inspect what survived, then resume. Resume replays the recorded history
(completed calls are read from the log, never re-executed) and continues
live from the first unrecorded step:

```sh
./target/debug/salvor --store /tmp/salvor-demo.db history <run-id>
./target/debug/salvor --store /tmp/salvor-demo.db \
    resume <run-id> --agent demo/agent.toml
wc -l "$SALVOR_DEMO_FINDINGS"     # exactly 9: nothing was written twice
```

`salvor history <run-id>` after the resume shows one continuous event log:
everything up to the kill is byte-identical to what was recorded before it.
`salvor replay --dry-run <run-id>` re-derives the run's state from the log
without executing anything.

Uses model `claude-opus-4-8` (set in `agent.toml`). The API key is read
from `ANTHROPIC_API_KEY` and never appears in any file.

## Rehearsal mode (mock model, no key, no network)

For rehearsing the demo (or recording it repeatably), point the same
`agent.toml` at a scripted model instead: export `SALVOR_DEMO_BASE_URL` with
the URL of any server speaking the Messages API wire shape, and the agent
file's `base_url_env` hook routes requests there. No key is needed.

The integration test `crates/salvor-cli/tests/demo_run.rs` is exactly this
harness: it mounts a wiremock server scripted with all twenty turns (each
response selected by conversation length, so replays after a resume never
confuse it), points `SALVOR_DEMO_BASE_URL` at it, runs the real `salvor`
binary against this directory's unmodified `agent.toml` and `input.json`,
and asserts the findings file ends up with exactly the nine expected lines.
Run it with:

```sh
cargo test -p salvor-cli --test demo_run
```

## The property test behind the GIF

The demo is one kill at one boundary. The release gate behind it is
`crates/salvor-runtime/tests/release_gate.rs`: the same shape of run, killed
at every one of its event boundaries, resumed, and checked for an identical
final log and zero duplicate writes at each one. The process-level
complement, which SIGKILLs the real binary mid-run, is
`crates/salvor-cli/tests/kill_resume.rs`.
