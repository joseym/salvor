# follow-up: a run that sleeps for real, and what that proves

An accounts desk chasing an unpaid invoice does not decide anything on the day
it sends the reminder. It sends the reminder, waits out a cool-off, and then
looks: paid, and the invoice closes; still unpaid, and it goes to collections.
The whole desk is one wait with a look on the far side of it, and the wait is
days long, so nothing can be holding a process open across it.

That is this example. One graph document
([`invoice-follow-up.json`](invoice-follow-up.json)) chases `INV-2031` through a
`delay` node. The run sends its reminder, parks `sleeping` with a deadline in
its log, and the CLI exits. Nothing at all is running while it waits. Later,
`salvor wake` finds it due and drives it to the end.

Everything runs offline, and this example has no model in it anywhere: the graph
is four tool nodes, a durable wait, and a branch, so no API key, no network, and
no scripted model server are involved. The one thing it does spend is wall time,
about a minute, because the deadline it waits out is a real one on a real clock.

## The document, node by node

```
send_reminder (tool, Write)
      |
      v
cool_off (delay, 20 seconds)          <-- the run parks HERE, holding nothing
      |
      v
check_payment (tool, Read)            <-- reads the world AFTER the wait
      |
      v
route (branch on structuredContent.paid)
      |                        |
   paid|                       |unpaid
      v                        v
close_invoice (tool, Write)  escalate (tool, Write)
```

- **`send_reminder`** (`tool`, Write). The entry node. Its input is the graph
  input, the invoice to chase, and it appends one line to the reminders ledger.
  It happens BEFORE the wait, which is what makes the exactly-once claim below
  worth checking: the reminder must survive the nap without being sent twice.
- **`cool_off`** (`delay`, 20 seconds). The whole point. Entering it records a
  clock reading and a `SleepStarted` derived from that reading, and then the
  drive returns: the run is parked `sleeping` with an instant in its log, and no
  process is left holding it. A delay transforms nothing, so its output is its
  input verbatim; it moves the run in time, never in value.
- **`check_payment`** (`tool`, Read). Reads the payments file and answers whether
  the invoice is paid. It runs on the far side of the wait, so what it reads is
  the world at the wake.
- **`route`** (`branch`). Two cases over the reading, `structuredContent.paid ==
  true` and `structuredContent.paid == false`, each realized by a labelled edge.
  A branch is a pure router: whichever case fires, the value passes through
  unchanged.
- **`close_invoice`** and **`escalate`** (`tool`, Write). One arm each, one
  ledger each, so which arm ran is a fact on disk and not an inference from
  prose.

This document references no agent by content hash, because it has no `agent`
node to reference one from. It still needs `--agent` at every drive:
`salvor graph run` has no standalone tool inventory, so a `tool` node's name
resolves against the tools the supplied `--agent` files carry, and
[`agents/accounts-desk.toml`](agents/accounts-desk.toml) exists to carry the one
MCP server that answers all four. Its `model` is never called. The day somebody
adds an `agent` node to write the escalation letter, learn the hash the node has
to name with `salvor agent hash examples/follow-up/agents/accounts-desk.toml`
and put that in the node, because a graph references an agent by hash and never
by path.

The document validates:

```
$ salvor graph validate examples/follow-up/invoice-follow-up.json
graph ok: 6 node(s), 5 edge(s)
entry:    send_reminder
terminal: close_invoice, escalate
```

## Why the wait is a duration, and why you cannot pass it on the command line

The node says `"seconds": 20`. It does not say a date, and there is no flag to
override it, and both of those are on purpose.

A graph document is authored once and then content addressed: the hash IS the
identity, and the same document backs `salvor graph run`, the control plane's
`POST /v1/graph-runs`, and every fork of every run that ever referenced it. An
absolute wake instant baked into a document would make it a single-use artifact,
correct on the first run and already in the past on the second, where every
delay would fall through instantly and the document would silently mean
something other than it did the day it was written. A duration says the thing
that stays true across runs, and it is what every other author-time number in
this format already is: a `map`'s `concurrency`, a `fold`'s `max_iterations`.

That is also why `run.sh` cannot take the wait as a parameter. The wait is IN
the document, and changing it changes the hash, which makes it a different
document that an existing run could not be resumed against. To try a different
cool-off, edit `"seconds"` and start a new run.

The instant is resolved at execution instead. Entering the node observes the
clock into the log as a `NowObserved`, and `wake_at` is derived from that
recorded reading, so the deadline is a pure function of recorded data and every
later drive derives the identical one.

A wait of nothing is refused before it can reach a run at all:

```
$ salvor graph validate /tmp/salvor-follow-up-zero.json
/tmp/salvor-follow-up-zero.json: 1 validation error(s):
  - delay node `cool_off`: seconds must be at least 1, found 0
```

A zero-second delay would record a clock reading, a `SleepStarted`, and a
`SleepCompleted` in every log forever and mean exactly what deleting the node
means, so the validator says so at submit. `run.sh` produces that file and that
refusal as its first step.

## What a park looks like, and what holds it

Run verbatim from the repo root, this command inherits none of `run.sh`'s
`SALVOR_FOLLOWUP_*` exports, so the four ledgers `server.py` writes
(`SALVOR_FOLLOWUP_REMINDERS`, `SALVOR_FOLLOWUP_CLOSED`,
`SALVOR_FOLLOWUP_ESCALATIONS`, `SALVOR_FOLLOWUP_PAYMENTS`) fall back to their
defaults, plain filenames under the working directory, and land in the repo
root. Export those four first, pointed at a scratch directory, to keep them
out of it.

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db graph run \
    examples/follow-up/invoice-follow-up.json \
    --input @examples/follow-up/input.json \
    --agent examples/follow-up/agents/accounts-desk.toml
run c407d5ef-b20b-4ac0-b5e9-5e10af9945f8
graph run c407d5ef-b20b-4ac0-b5e9-5e10af9945f8 parked at node `cool_off`.
  sleeping until: 2026-08-15 19:42:50Z
it continues once the deadline passes:
  salvor wake --graph examples/follow-up/invoice-follow-up.json --agent examples/follow-up/agents/accounts-desk.toml
```

The command has returned. There is no daemon, no held connection, and no timer
thread: the MCP server `salvor` spawned as a child read stdin to EOF and exited
with it. The run is rows in a SQLite file, and `salvor list --status sleeping`
reads that status back by folding the log, not by consulting a scheduler:

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db list --status sleeping
RUN ID                                STATUS                EVENTS  STARTED               LAST ACTIVITY
c407d5ef-b20b-4ac0-b5e9-5e10af9945f8  sleeping                   8  2026-08-15 19:42:30Z  2026-08-15 19:42:30Z
```

A sleeping run holds no lock, no idempotency claim, and no process, and
`SleepStarted` is recorded only after whatever came before it has already
settled. Backing up the store or restarting the machine while a run sleeps is
exactly as safe as doing either while a run sits idle between steps. See
[`docs/OPERATIONS.md#waking-sleeping-runs`](../../docs/OPERATIONS.md#waking-sleeping-runs).

## Driving it early is refused, and records nothing

A person can reach a sleeping run directly, and the honest answer is a refusal
rather than a silent no-op:

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db resume c407d5ef-b20b-4ac0-b5e9-5e10af9945f8 \
    --graph examples/follow-up/invoice-follow-up.json \
    --agent examples/follow-up/agents/accounts-desk.toml
Run c407d5ef-b20b-4ac0-b5e9-5e10af9945f8 is sleeping until 2026-08-15 19:42:50Z
and will not resume for another 19s. It is not parked on you: a sleeping run
takes no input, and driving it early records nothing.
Wait for the deadline, then drive whatever is due:
  salvor wake --graph examples/follow-up/invoice-follow-up.json --agent examples/follow-up/agents/accounts-desk.toml
$ echo $?
1
```

The event count is identical before and after that attempt: `run.sh` asserts
both the nonzero exit and the unchanged count, because "refused" and "did
nothing" are two different claims and a timer needs both.

A sweep run early is not refused, because nothing is wrong with it. It simply
finds nothing:

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db wake --dry-run \
    --graph examples/follow-up/invoice-follow-up.json \
    --agent examples/follow-up/agents/accounts-desk.toml
nothing to wake: no run in /tmp/salvor-follow-up-unpaid.db is sleeping past its deadline
```

Sweeps select on the recorded deadline, not on when they happen to run, so a
cron line firing every minute against a run due in an hour wakes nothing early
and costs nothing. Once the deadline passes, the same command lists it:

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db wake --dry-run \
    --graph examples/follow-up/invoice-follow-up.json \
    --agent examples/follow-up/agents/accounts-desk.toml
1 run(s) due to wake at 2026-08-15 19:42:51Z (dry run):
  c407d5ef-b20b-4ac0-b5e9-5e10af9945f8 due 2026-08-15 19:42:50Z, overdue by 1s
nothing was driven. Drop --dry-run to wake these, passing the --agent (and --graph) files they need.
```

and dropping `--dry-run` drives it to the end.

## The recorded walk

The whole of leg A's log, reminder to escalation, across the nap:

```
$ salvor --store /tmp/salvor-follow-up-unpaid.db history c407d5ef-b20b-4ac0-b5e9-5e10af9945f8
   0  2026-08-15 19:42:30Z  GraphRunStarted      graph sha256:ce67487… input {"amount_cents":128400,"customer":"Alder and Finch Joinery","due_date":"2026-07-…
   1  2026-08-15 19:42:30Z  NodeEntered          enter send_reminder
   2  2026-08-15 19:42:30Z  ToolCallRequested    send_reminder [Write] input {"amount_cents":128400,"customer":"Alder and Finch Joinery","due_date":"2026-07-…
   3  2026-08-15 19:42:30Z  ToolCallCompleted    output {"content":[{"text":"reminder sent to Alder and Finch Joinery on INV-2031","type…
   4  2026-08-15 19:42:30Z  NodeExited           exit send_reminder
   5  2026-08-15 19:42:30Z  NodeEntered          enter cool_off
   6  2026-08-15 19:42:30Z  NowObserved          2026-08-15 19:42:30Z
   7  2026-08-15 19:42:30Z  SleepStarted         until 2026-08-15 19:42:50Z
   8  2026-08-15 19:42:51Z  SleepCompleted       woke
   9  2026-08-15 19:42:51Z  NodeExited           exit cool_off
  10  2026-08-15 19:42:51Z  NodeEntered          enter check_payment
  11  2026-08-15 19:42:51Z  ToolCallRequested    check_payment [Read] input {"content":[{"text":"reminder sent to Alder and Finch Joinery on INV-2031","type…
  12  2026-08-15 19:42:51Z  ToolCallCompleted    output {"content":[{"text":"INV-2031 is still unpaid as of this reading","type":"text"}…
  13  2026-08-15 19:42:51Z  NodeExited           exit check_payment
  14  2026-08-15 19:42:51Z  NodeEntered          enter route
  15  2026-08-15 19:42:51Z  BranchTaken          branch route -> unpaid
  16  2026-08-15 19:42:51Z  NodeExited           exit route
  17  2026-08-15 19:42:51Z  NodeSkipped          skip close_invoice: no live inbound edge: an upstream branch routed to another case
  18  2026-08-15 19:42:51Z  NodeEntered          enter escalate
  19  2026-08-15 19:42:51Z  ToolCallRequested    escalate [Write] input {"content":[{"text":"INV-2031 is still unpaid as of this reading","type":"text"}…
  20  2026-08-15 19:42:51Z  ToolCallCompleted    output {"content":[{"text":"INV-2031 escalated to collections: no payment after the coo…
  21  2026-08-15 19:42:51Z  NodeExited           exit escalate
  22  2026-08-15 19:42:51Z  RunCompleted         output {"content":[{"text":"INV-2031 escalated to collections: no payment after the coo…
```

Read the middle of that. `NodeExited exit send_reminder` comes first: the
reminder was out the door before anything slept. Then `NodeEntered enter
cool_off`, then `NowObserved`, then `SleepStarted until ...` derived from it.
Everything above that line was written by the first drive; everything from
`SleepCompleted` down was written by `salvor wake`, minutes or days later, in a
different process. The log does not distinguish them, and nothing needs it to:
the events are the run.

There is no delay-specific event kind. `SleepStarted` and `SleepCompleted` are
the whole vocabulary, exactly as `Suspended` and `Resumed` are the gate's, and a
delay parks the same way a gate does: enter the node, park, leave no
`NodeExited` behind, so the next drive re-enters that node and continues from
the recorded sleep. That is also why an early drive can record nothing at all
and still be correct.

## The two legs

**Leg A, nobody pays.** The payments file is empty at the start and stays empty.
The run wakes, reads it, routes `unpaid`, and the escalations ledger gets its one
line while the closed ledger gets none. The reminders ledger still holds exactly
one line: `send_reminder`'s completion was already in the log, so the wake
replayed it for free rather than re-executing it.

**Leg B, the payment lands during the nap.** Same document, same input, a fresh
store, and the same empty payments file at the start. After the run parks, the
script writes the payment in. Nothing tells the run; nothing can. It is not
watching the file, it is not subscribed to anything, and it holds no process
that could be notified. It has an instant in its log. When `salvor wake` drives
it, `check_payment` reads what is there NOW, the branch records `route -> paid`,
and the closed ledger gets the line the escalations ledger got in leg A:

```
   7  2026-08-15 19:42:51Z  SleepStarted         until 2026-08-15 19:43:11Z
   8  2026-08-15 19:43:13Z  SleepCompleted       woke
  15  2026-08-15 19:43:13Z  BranchTaken          branch route -> paid
  22  2026-08-15 19:43:13Z  RunCompleted         output {"content":[{"text":"INV-2031 closed: payment received","type":"text"}],"isError…
```

That is the property worth naming, because it is the one a scheduler-shaped
mental model gets wrong: **a delay records an instant and nothing else.** It
carries no snapshot of the world across the wait, and it does not re-deliver the
value the desk had when it fell asleep. The value that flows through the node is
the upstream one, verbatim, and every fact about the world on the far side is
read on the far side.

## Run it

```
cargo build
bash examples/follow-up/run.sh
```

It validates the document, refuses a zero-second copy, runs leg A, refuses an
early resume and checks that the refusal recorded nothing, sweeps before the
deadline and finds nothing, waits the deadline out, wakes the run and checks the
recorded order of the reminder against the sleep, then runs leg B with the
payment landing mid-nap. It exits 0 only if every proof holds, and every check
that does not hold prints a `FAILED: expected ...` line naming what it wanted and
what it found, so a run that stops early can never be mistaken for one that
passed. It takes about a minute of wall time, most of it the two real waits.

`cargo build --release` works just as well: the script takes `target/debug` when
it is there and `target/release` otherwise, so there is no need to build the same
code twice.

Paths are overridable, so it runs on a busy machine and in CI:
`SALVOR_EXAMPLE_SCRATCH`, `SALVOR_EXAMPLE_UNPAID_STORE`,
`SALVOR_EXAMPLE_PAID_STORE`, and `SALVOR_EXAMPLE_PAYMENTS`. `SALVOR_BIN`
overrides the binary path outright, which is how an already-installed CLI drives
this instead of a checkout's build. No port is bound anywhere: there is no model
server and no control plane here, because `salvor graph run` and `salvor wake`
drive the store directly. Every ledger, store, and payments file `run.sh`
produces lives under the scratch directory; running `run.sh` itself, nothing
runtime is written into the repository. The commands earlier in this README,
run by hand rather than through the script, do not carry that guarantee; see
[above](#what-a-park-looks-like-and-what-holds-it).

## What an operator uses instead of the script's `sleep`

`run.sh` calls `sleep` because it has to prove the deadline really arrives inside
one shell script. Production has two better answers, and a store gets exactly one
of them:

- **`salvor serve`** sweeps for due timers every 60 seconds by default.
  `--wake-interval SECS` changes the cadence and `--wake-interval 0` turns the
  sweep off entirely, no task spawned.
- **cron**, for a store no server is watching:

  ```
  * * * * * salvor wake --store /var/lib/salvor/salvor.db --graph /etc/salvor/graphs/invoice-follow-up.json --agent /etc/salvor/agents/accounts-desk.toml
  ```

Not both at once: the server's sweep only skips runs it is already driving
itself, and it has no way to know about a second drive cron started on the same
run. `wake` takes the same `--graph` and repeatable `--agent` a `resume` would,
and for the same reason: the log records a graph run by the document's hash and
never the document itself, so waking a run rebuilds it from the same files its
author last ran it with. A due run this sweep cannot rebuild is reported and left
asleep, still due for the next one, and every other due run still gets its turn.

## Why this example exists at all

`crates/salvor-cli/tests/wake_cli.rs` seeds its sleeping runs by hand, which is
right for what it tests: selection, routing, reporting, and exit codes are exact
that way and wait on no wall clock. Its own header names the one case it
therefore cannot stage, a run whose re-drive genuinely continues to completion.
That case is what this example is. The run here really sleeps, `salvor wake`
really finds it, and the walk really finishes on the far side.
