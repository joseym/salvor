# refine: a draft loop that converges, and the two things it records

A payroll desk does not send the first draft of a correction notice. Somebody
writes it, reads it against the desk checklist, and writes it again, and the
notice goes out when it is good enough or goes to a person when it never gets
there. That loop is a `fold`: a bounded number of passes over one agent, each
pass folding over the one before it, stopping on a condition the document
states out loud.

That is this example. One graph document
([`refine-notice.json`](refine-notice.json)) refines the notice that goes out
with a corrected payslip. The `tailor` agent writes a draft and grades it
against the desk checklist; the `refine` fold runs it up to four times, stops as
soon as a draft clears 0.8, and produces the best-scoring pass. The run script
kills the process in the middle of a pass and recovers it, and the log settles
the two arguments a loop like this always starts: which pass won, and what the
crash cost.

Everything runs offline. A scripted model server (`salvor-demo-model`) stands in
for a real endpoint, so no API key and no network are needed, and this example
has no tools at all: the fold is the whole mechanism.

## The document, node by node

```
refine (fold, bound 4, stop_when score >= 0.8, best_by score, on_bound fail)
   └── body ──▶ tailor (agent, output_schema {draft, score})
```

- **`refine`** (`fold`). The entry node, and the only node on the walk. Its input
  is the graph input, the correction to write up. Pass 0 folds over that; every
  later pass folds over the previous pass's output, which IS the accumulated
  value. After each pass joins, `stop_when "score >= 0.8"` is evaluated against
  the pass that just joined, and the loop stops the moment it holds. The `join`
  is `best_by: score`, an argmax over ALL the passes that ran, so the node
  produces the best draft rather than the last one. `max_iterations` is 4 and
  `on_bound` is `fail`, which is [what the desk means](#what-on_bound-fail-means)
  by a bound.
- **`tailor`** (`agent`, [`agents/tailor.toml`](agents/tailor.toml)). The
  per-pass worker. One pass is one whole run of this agent: a fresh conversation
  whose single user message is that pass's input. It is never walked as a node of
  its own, so its model call is recorded inline between the fold's iteration
  markers.

The node declares the `output_schema` (`{draft: string, score: number}`), not the
agent file. That declaration does two jobs at once. At runtime it puts the agent
on the structured loop: the model is offered a `salvor_answer` tool carrying that
exact schema, must call it, and the pass output is the validated object, so
`score` is a number the fold reads rather than a figure somebody has to find in a
sentence. At submit it is what the validator reads the fold's own expressions
against. An agent file may declare a schema too, and the node's wins where both
appear; here only the node does, because the shape is a fact about this position
in this document.

The `agent_hash` in the document is the real content hash of the file checked in
beside it. Ask for it yourself:

```
$ salvor agent hash examples/refine/agents/tailor.toml
sha256:6f92dceb3f50d6f86a8cdbe947a7a5c9fbd3944ff72b1afa02fe01228fd36714
```

The document validates:

```
$ salvor graph validate examples/refine/refine-notice.json
graph ok: 2 node(s), 0 edge(s)
entry:    refine, tailor
terminal: refine, tailor
```

(`tailor` shows as an entry and a terminal because it is the fold's body: it has
no edges of its own and is walked only inside the loop, never on the main path.)

## The two truths this proves

**The winning pass is recorded, not inferred.** The scripted desk scores its
first draft 0.55 and its second 0.85. The loop stops after pass 1, and the log
says which pass won and why it stopped, in one line:

```
FoldConverged   fold refine converged on [1]: stop_when held after pass 1: `score >= 0.8`
```

`winner_index` is the argmax the `best_by: score` join computed over every pass
that ran, and it is durable. Nobody has to re-read two drafts and re-apply a
threshold to find out which one the desk actually shipped; the losing pass is
still in the log beside it, at 0.55, so the choice can be checked rather than
trusted. Passes 2 and 3 were never asked for, and the log shows that too: two
`FoldIterationStarted` events under a bound of four.

**A kill mid-pass re-drives only the interrupted call.** The run script runs the
same graph twice: once clean, and once with `kill -9` landing while pass 1's
model call is in flight. After a `salvor resume`, the recovered log holds the
same 15 events in the same order as the uninterrupted control run, and the model
server counted exactly one extra request across the whole crash. Pass 0 had
already joined, so it came back off the log for free; only the call that was in
flight was made again.

## What `on_bound: fail` means

`on_bound` says what reaching `max_iterations` without `stop_when` holding MEANS.
Two words are legal: `join` folds the passes anyway and produces a winner, and
`fail` refuses.

This desk declares `fail`, because a notice that never cleared the checklist is
not a notice to send. Under `join` the fold would hand back the least bad of four
failures and record a convergence, and the next person down the line would have
no way to tell that from a draft that actually cleared. Under `fail` the run is
recorded `RunFailed` with the bound named in the error, the four passes and their
joins stay in the log because that work really happened, and no `FoldConverged`
and no `NodeExited` are written. So the word buys a guarantee on the happy path
too: **a `FoldConverged` in this run's log means the threshold was met**, never
just that the loop ran out of passes.

`run.sh` shows both. The second arm feeds the same document a correction nobody
can put in one plain paragraph, four passes score 0.41, 0.52, 0.58, and 0.61, and
the run fails with all four in the log.

## The static check

A fold's accumulated value is what its body produced, so the body node's declared
`output_schema` is the shape `stop_when` and `best_by` read, path for path. The
validator reads them against it at submit. Move one letter in the predicate:

```
$ salvor graph validate salvor-refine-typo.json
salvor-refine-typo.json: 1 validation error(s):
  - fold node `refine`: `stop_when` reads `scoer`, which body node `tailor`'s declared output schema does not describe
```

That is the difference between a typo you are told about now and a loop that runs
its whole bound, never stops, and fails on the bound for a reason that has
nothing to do with the drafts. `run.sh` produces that file and that refusal as
its first step.

## Run it

```
cargo build
bash examples/refine/run.sh
```

It validates the document, refuses the typoed copy, runs the graph clean, runs it
again into a `kill -9`, resumes, checks every proof above, and then runs the
bound arm. It exits 0 only if all of them hold, and every check that does not
hold prints a `FAILED: expected ...` line naming what it wanted and what it
found, so a run that stops early can never be mistaken for one that passed.

`cargo build --release` works just as well: the script takes `target/debug`
when it is there and `target/release` otherwise, so there is no need to build the
same code twice.

Ports and paths are overridable, so it runs on a busy machine and in CI:
`SALVOR_EXAMPLE_MODEL_PORT` (default 18951), `SALVOR_EXAMPLE_MODEL_DELAY_MS`
(default 2000, wide enough to aim a kill at), `SALVOR_EXAMPLE_SCRATCH`, and the
three store paths. `SALVOR_BIN` and `SALVOR_DEMO_MODEL_BIN` override the binary
paths outright, which is how an already-installed CLI drives this instead of a
checkout's build.

## How the offline model answers a loop

[`model-script.json`](model-script.json) is a `salvor-demo-model` script, and a
fold is the one shape that script's usual selection rules cannot serve. Every
pass is a fresh conversation carrying exactly one message under the same system
prompt, so neither the message count nor the system prompt tells pass 0 from pass
1. What does is the pass input, which reaches the model verbatim: pass 0 carries
the notice id `ADJ-7741`, and pass 1 carries the draft pass 0 wrote, tagged
`[rev A1]`. So each conversation in the script declares a `when` needle:

```json
"adj-7741-pass-1": {
  "when": "rev A1",
  "turns": [ ... one salvor_answer call, score 0.85 ... ]
}
```

A needle must appear in exactly one pass's request, which is why the agent's
system prompt names no notice id and no revision tag: anything written there
would appear in every request and match every pass at once, and the script server
refuses an ambiguous match rather than guessing. The same rule is what makes the
crash proof work: selection is stateless, so the resume's re-driven call carries
the same body and gets the same answer the killed call was going to get.

Each turn is a full Messages API response whose one content block is a `tool_use`
call to `salvor_answer`, which is exactly what a real model sends under a
declared output schema.
