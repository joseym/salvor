#!/usr/bin/env bash
# Drive the invoice-follow-up graph end to end, offline, across a real durable
# wait on a real clock.
#
#   validate -> send the reminder -> park `sleeping` on the delay, with no
#       process holding the run -> refuse an early resume, recording nothing ->
#       find nothing to wake before the deadline -> wait it out -> `salvor wake`
#       drives the run to completion
#
# Then the same document again, with one difference: the payment lands WHILE the
# run is asleep. The run wakes, reads the world as it is at that moment, and
# takes the other arm of the branch. Nothing here needs an API key, a network, or
# a model: the graph is four tool nodes, a delay, and a branch, and every
# decision is the document's.
#
# The failure mode this kills: a wait that is really a held process. A run parked
# on a timer here is DATA. The CLI has already exited, the MCP server it spawned
# died with it, and the deadline lives in the log. Kill the machine and the wait
# survives; that is what makes it durable rather than a sleeping thread.
#
# This is also the one end-to-end case the CLI's own `wake` suite cannot stage
# (see the header of `crates/salvor-cli/tests/wake_cli.rs`): a run that really
# sleeps and is really driven to completion by `salvor wake`. It is staged here
# instead, against wall time, which is why this script takes about a minute.
#
# Usage, from anywhere:
#     bash examples/follow-up/run.sh
#
# It drives the binary a checkout builds, preferring `target/debug` and falling
# back to `target/release`, so either `cargo build` or `cargo build --release` is
# enough. `SALVOR_BIN` overrides that path outright, which is how an
# already-installed CLI drives this instead. Store and ledger paths are
# overridable too; see the block below.
set -euo pipefail

# Repository root, two levels up from this script. The agent definition names its
# MCP server by a path relative to the directory salvor is invoked from, so
# everything below runs from the root.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Paths are overridable so this runs on a busy machine and in CI. No port is
# bound anywhere: this example has no model server and no control plane, because
# `salvor graph run` and `salvor wake` drive the store directly.
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
UNPAID_STORE="${SALVOR_EXAMPLE_UNPAID_STORE:-$SCRATCH/salvor-follow-up-unpaid.db}"
PAID_STORE="${SALVOR_EXAMPLE_PAID_STORE:-$SCRATCH/salvor-follow-up-paid.db}"
PAYMENTS="${SALVOR_EXAMPLE_PAYMENTS:-$SCRATCH/salvor-follow-up-payments.json}"
# One set of ledgers per leg, so a line count is that leg's own answer and never
# the two legs added together.
A_REMINDERS="$SCRATCH/salvor-follow-up-a-reminders.txt"
A_CLOSED="$SCRATCH/salvor-follow-up-a-closed.txt"
A_ESCALATIONS="$SCRATCH/salvor-follow-up-a-escalations.txt"
B_REMINDERS="$SCRATCH/salvor-follow-up-b-reminders.txt"
B_CLOSED="$SCRATCH/salvor-follow-up-b-closed.txt"
B_ESCALATIONS="$SCRATCH/salvor-follow-up-b-escalations.txt"
ZERO_GRAPH="$SCRATCH/salvor-follow-up-zero.json"
OUT="$SCRATCH/salvor-follow-up"

mkdir -p "$SCRATCH"

# The first build of `$1` this checkout has, debug before release. A release
# build is a real build: telling somebody who ran `cargo build --release` to run
# `cargo build` is asking them to compile the same code a second time.
built_bin() {
  local candidate
  for candidate in "$ROOT/target/debug/$1" "$ROOT/target/release/$1"; do
    if [[ -x "$candidate" ]]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

SALVOR="${SALVOR_BIN:-$(built_bin salvor || true)}"
if [[ -z "$SALVOR" ]]; then
  echo "missing the \`salvor\` binary: no target/debug/salvor and no target/release/salvor." >&2
  echo "Build it with:  cargo build      (or: cargo build --release)" >&2
  echo "Or point SALVOR_BIN at one you already have." >&2
  exit 1
fi
if [[ ! -x "$SALVOR" ]]; then
  echo "SALVOR_BIN names \`$SALVOR\`, which is not an executable file." >&2
  exit 1
fi

GRAPH="examples/follow-up/invoice-follow-up.json"
AGENT="examples/follow-up/agents/accounts-desk.toml"
AGENTS=(--agent "$AGENT")
INPUT="@examples/follow-up/input.json"
INVOICE="INV-2031"

# Every leg starts from a clean store and clean ledgers, so the line counts below
# mean what they say. Only files this script owns, by exact path, are removed.
rm -f "$UNPAID_STORE" "$UNPAID_STORE-wal" "$UNPAID_STORE-shm" \
      "$PAID_STORE" "$PAID_STORE-wal" "$PAID_STORE-shm" \
      "$PAYMENTS" "$ZERO_GRAPH" \
      "$A_REMINDERS" "$A_CLOSED" "$A_ESCALATIONS" \
      "$B_REMINDERS" "$B_CLOSED" "$B_ESCALATIONS" \
      "$OUT"-*.out

# The MCP server writes here; the agent definition passes these through to its
# child process by ordinary environment inheritance, so no checked-in file
# changes and nothing runtime lands in the repository. The per-leg ledger paths
# are re-exported at the top of each leg.
export SALVOR_FOLLOWUP_PAYMENTS="$PAYMENTS"

# Salvor's own event lines are the story; the MCP client library's handshake
# chatter is not. Quiet that one target and leave everything else at info, and
# only when the caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-info,rmcp=warn}"

# NOTHING IS SPAWNED IN THE BACKGROUND BY THIS SCRIPT, so there is no pid to
# record and no teardown to run: `salvor graph run` and `salvor wake` spawn the
# MCP server themselves as a child and take it with them when they exit. That is
# the point of the example, and it is why there is no `trap cleanup EXIT` here
# where the other example scripts have one.

FAIL=0

# --- Saying what failed, every time ---
#
# Every check below names itself when it fails. A bare `grep` or `[[ ... ]]`
# under `set -euo pipefail` exits 1 having printed NOTHING, and a proof script
# that can die silently is worse than one that proves less: a reader cannot tell
# a run that checked everything from one that stopped before it got there. So
# each check goes through `fail`, `die`, or `check`, and each says what was
# expected and what was actually there.

# fail <what was expected> [<what was actually there>]
fail() {
  echo "FAILED: expected $1"
  if [[ -n "${2:-}" ]]; then
    printf '%s\n' "$2" | sed 's/^/       actual: /'
  fi
  FAIL=1
}

# die <what was expected> [<what was actually there>]: a failure nothing after
# it can be read past, so the script stops here rather than cascading.
die() {
  fail "$@"
  exit 1
}

# check <actual> <expected> <the proof this states> <what was expected>
check() {
  if [[ "$1" == "$2" ]]; then
    echo "PROOF: $3"
  else
    fail "$4" "$1"
  fi
}

# The kind column of a recorded log, one event per line.
event_kinds() {
  "$SALVOR" --store "$1" history "$2" | awk '{print $4}'
}

# The number of events in a recorded log, as one bare number. An empty log is
# zero rather than an error, which is what the `|| count=0` carries.
count_events() {
  local kinds count
  kinds=$(event_kinds "$1" "$2") \
    || die "run $2's log in $1 to read back" "\`salvor history\` exited nonzero"
  count=$(grep -c . <<<"$kinds") || count=0
  printf '%s' "$count"
}

# The number of non-empty lines in a ledger, as one bare number. A ledger that
# was never written is zero lines, not an error: that IS the answer for an arm
# the run never took.
count_lines() {
  local count
  count=$(grep -c . "$1" 2>/dev/null) || count=0
  printf '%s' "$count"
}

# The recorded seq number of the first line of <log> holding <text>, the
# same number `salvor history` prints in its own first column, or the empty
# string. Fixed-string matching, so a node id is a node id and not a pattern.
seq_of() {
  grep -F -- "$2" <<<"$1" | head -1 | awk '{print $1}'
}

# Multi-line CLI prose flattened to one line, so a `grep` for a sentence cannot
# be defeated by the 80-column wrap the reports use.
flatten() {
  tr '\n' ' ' <"$1" | tr -s ' '
}

# Whole seconds from now until the recorded wake instant <1>, formatted as
# `salvor` prints it (`2026-08-15 18:04:31Z`, always UTC), plus two seconds of
# margin. The margin covers the sub-second part the printed form drops: a
# deadline recorded at 18:04:31.7 prints as 18:04:31, and sleeping to exactly
# that would land before it. python3 does the parsing because it is already a
# hard dependency here (the MCP server is a Python program) and `date` spells
# this differently on BSD and GNU.
seconds_until() {
  python3 - "$1" <<'PY'
import datetime
import sys

target = datetime.datetime.strptime(sys.argv[1], "%Y-%m-%d %H:%M:%SZ").replace(
    tzinfo=datetime.timezone.utc
)
now = datetime.datetime.now(datetime.timezone.utc)
print(max(0, int((target - now).total_seconds()) + 2))
PY
}

# Any live process whose command line names <1>. Read-only, by exact path, and
# never a kill: this script starts no background process and so has nothing to
# tear down. It is how the "no process holds a sleeping run" claim is checked
# rather than asserted.
holders() {
  pgrep -f "$1" 2>/dev/null || true
}

echo "############################################"
echo "# 1. The static check: a wait of nothing never reaches a run"
echo "############################################"
"$SALVOR" graph validate "$GRAPH" \
  || die "the committed document to validate" \
         "\`salvor graph validate $GRAPH\` exited nonzero; its output is above"
# The same document with the wait set to zero. A `delay` whose `seconds` is zero
# is a node that would record a clock reading, a SleepStarted and a
# SleepCompleted forever and mean exactly what deleting the node means, so the
# validator refuses it at submit rather than leaving it to be noticed in a log.
sed 's/"seconds": [0-9][0-9]*/"seconds": 0/' "$GRAPH" >"$ZERO_GRAPH" \
  || die "the zero-wait copy of the document to be written" "sed into $ZERO_GRAPH failed"
grep -q '"seconds": 0' "$ZERO_GRAPH" \
  || die "the zero-wait copy to actually carry \`\"seconds\": 0\`" "$(cat "$ZERO_GRAPH")"
if "$SALVOR" graph validate "$ZERO_GRAPH" >"$OUT-zero.out" 2>&1; then
  fail "the validator to refuse a delay node declaring a zero wait" "$(cat "$OUT-zero.out")"
else
  cat "$OUT-zero.out"
  if grep -q 'seconds must be at least 1' "$OUT-zero.out"; then
    echo "PROOF: a delay of zero seconds is refused at validate, naming the node and the rule."
  else
    fail "the refusal to say \`seconds must be at least 1\`" "$(cat "$OUT-zero.out")"
  fi
fi

echo
echo "############################################"
echo "# 2. Leg A, nobody pays: the run sends the reminder and parks on the timer"
echo "############################################"
export SALVOR_FOLLOWUP_REMINDERS="$A_REMINDERS"
export SALVOR_FOLLOWUP_CLOSED="$A_CLOSED"
export SALVOR_FOLLOWUP_ESCALATIONS="$A_ESCALATIONS"
# The world at the moment the run starts: no payment on file for anything.
printf '%s\n' '{}' >"$PAYMENTS"

if ! "$SALVOR" --store "$UNPAID_STORE" graph run "$GRAPH" \
     --input "$INPUT" "${AGENTS[@]}" --label desk=accounts >"$OUT-a-run.out" 2>&1; then
  cat "$OUT-a-run.out"
  die "the run to reach the delay and park (exit 0)" "\`graph run\` exited nonzero; its output is above"
fi
cat "$OUT-a-run.out"
A_ID=$(grep -oE 'run [0-9a-f-]{36}' "$OUT-a-run.out" | head -1 | awk '{print $2}')
[[ -n "$A_ID" ]] \
  || die "the run to print a \`run <id>\` line" "$(cat "$OUT-a-run.out")"

# The park line names the node, the wake instant, and the command that continues
# the run. The wake instant is read back out of it here and never hardcoded: the
# document declares a DURATION, and the instant is the engine's, derived from the
# clock reading it recorded.
A_WAKE_AT=$(sed -n 's/^ *sleeping until: *//p' "$OUT-a-run.out" | head -1)
if grep -q "parked at node \`cool_off\`" "$OUT-a-run.out" && [[ -n "$A_WAKE_AT" ]]; then
  echo "PROOF: the run parked at \`cool_off\` sleeping until $A_WAKE_AT."
else
  fail "a park at node \`cool_off\` with a \`sleeping until:\` instant" "$(cat "$OUT-a-run.out")"
fi
if grep -qF "salvor wake --store $UNPAID_STORE --graph $GRAPH --agent $AGENT" "$OUT-a-run.out"; then
  echo "PROOF: the park printed the exact command that continues it: salvor wake --store ... --graph ... --agent ..."
else
  fail "the park to print \`salvor wake --store $UNPAID_STORE --graph $GRAPH --agent $AGENT\`" "$(cat "$OUT-a-run.out")"
fi

# NOTHING HOLDS THE RUN. `graph run` has returned, so its process is gone, and
# the MCP server it spawned as a child went with it (that server reads stdin to
# EOF). There is nothing to kill here, which is the claim: the wait is recorded
# data, not a sleeping thread.
STORE_HOLDERS=$(holders "$UNPAID_STORE")
SERVER_HOLDERS=$(holders "examples/follow-up/server.py")
if [[ -z "$STORE_HOLDERS" && -z "$SERVER_HOLDERS" ]]; then
  echo "PROOF: no process holds the sleeping run: none has $UNPAID_STORE open, and no MCP server survived the park."
else
  fail "no live process naming the store or the MCP server while the run sleeps" \
       "store: ${STORE_HOLDERS:-none}; server: ${SERVER_HOLDERS:-none}"
fi

check "$(count_lines "$A_REMINDERS")" "1" \
  "the reminder went out exactly once, before the nap: 1 line in the reminders ledger." \
  "1 line in $A_REMINDERS"

A_LIST=$("$SALVOR" --store "$UNPAID_STORE" list --status sleeping) \
  || die "\`salvor list --status sleeping\` to succeed" "it exited nonzero"
printf '%s\n' "$A_LIST"
if grep -q "$A_ID" <<<"$A_LIST" && grep -q 'sleeping' <<<"$A_LIST"; then
  echo "PROOF: \`salvor list --status sleeping\` shows this run, folded out of its log rather than out of a scheduler."
else
  fail "run $A_ID to appear under \`list --status sleeping\`" "$A_LIST"
fi

# The world has not been read yet: `check_payment` is on the far side of the
# delay and the log has not entered it. That is what makes leg B's payment
# genuinely late rather than merely unnoticed.
A_PARKED_HIST=$("$SALVOR" --store "$UNPAID_STORE" history "$A_ID") \
  || die "the parked run's log to read back" "\`salvor history $A_ID\` exited nonzero"
echo
printf '%s\n' "$A_PARKED_HIST"
if ! grep -qF 'enter check_payment' <<<"$A_PARKED_HIST"; then
  echo "PROOF: the sleeping run has not read the world yet: no \`enter check_payment\` in its log."
else
  fail "no \`enter check_payment\` in the log of a run still asleep" "$A_PARKED_HIST"
fi

echo
echo "############################################"
echo "# 3. Before the deadline: an early drive is refused and records nothing"
echo "############################################"
EVENTS_BEFORE=$(count_events "$UNPAID_STORE" "$A_ID")
if "$SALVOR" --store "$UNPAID_STORE" resume "$A_ID" --graph "$GRAPH" "${AGENTS[@]}" \
     >"$OUT-a-early.out" 2>&1; then
  cat "$OUT-a-early.out"
  fail "an early \`salvor resume\` to be refused and exit nonzero" "it exited 0; its output is above"
else
  cat "$OUT-a-early.out"
  EARLY_FLAT=$(flatten "$OUT-a-early.out")
  A_REMAINING=$(sed -n 's/.*will not resume for another \([^ ]*\)\..*/\1/p' <<<"$EARLY_FLAT" | head -1)
  if grep -qF "is sleeping until $A_WAKE_AT" <<<"$EARLY_FLAT" && [[ -n "$A_REMAINING" ]]; then
    echo "PROOF: the early resume exited nonzero and said the run is sleeping until $A_WAKE_AT, $A_REMAINING left."
  else
    fail "the refusal to name the deadline ($A_WAKE_AT) and the time remaining" "$EARLY_FLAT"
  fi
fi
EVENTS_AFTER=$(count_events "$UNPAID_STORE" "$A_ID")
check "$EVENTS_AFTER" "$EVENTS_BEFORE" \
  "the refused drive recorded nothing: $EVENTS_BEFORE events before it and $EVENTS_AFTER after." \
  "the log to still hold $EVENTS_BEFORE events after the refused resume"

"$SALVOR" --store "$UNPAID_STORE" wake --dry-run --graph "$GRAPH" "${AGENTS[@]}" \
  >"$OUT-a-dry-early.out" 2>&1 \
  || die "\`salvor wake --dry-run\` before the deadline to exit 0" "it exited nonzero: $(cat "$OUT-a-dry-early.out")"
cat "$OUT-a-dry-early.out"
if grep -q 'nothing to wake' "$OUT-a-dry-early.out"; then
  echo "PROOF: a sweep before the deadline finds nothing due, so a cron line that fires early wakes nothing early."
else
  fail "\`wake --dry-run\` before the deadline to report nothing to wake" "$(cat "$OUT-a-dry-early.out")"
fi

echo
echo "############################################"
echo "# 4. After the deadline: \`salvor wake\` finds the run and finishes it"
echo "############################################"
A_WAIT=$(seconds_until "$A_WAKE_AT")
echo "== waiting ${A_WAIT}s for the deadline at $A_WAKE_AT =="
sleep "$A_WAIT"

"$SALVOR" --store "$UNPAID_STORE" wake --dry-run --graph "$GRAPH" "${AGENTS[@]}" \
  >"$OUT-a-dry-due.out" 2>&1 \
  || die "\`salvor wake --dry-run\` after the deadline to exit 0" "it exited nonzero: $(cat "$OUT-a-dry-due.out")"
cat "$OUT-a-dry-due.out"
A_DUE=$(grep -c 'overdue by' "$OUT-a-dry-due.out") || A_DUE=0
if [[ "$A_DUE" == "1" ]] && grep -q "$A_ID" "$OUT-a-dry-due.out"; then
  echo "PROOF: the same sweep after the deadline lists exactly this one run as due, and drove nothing."
else
  fail "\`wake --dry-run\` to list exactly run $A_ID as due" "$(cat "$OUT-a-dry-due.out")"
fi
EVENTS_AFTER_DRY=$(count_events "$UNPAID_STORE" "$A_ID")
check "$EVENTS_AFTER_DRY" "$EVENTS_BEFORE" \
  "the dry run recorded nothing either: still $EVENTS_BEFORE events." \
  "the log to still hold $EVENTS_BEFORE events after \`wake --dry-run\`"

if ! "$SALVOR" --store "$UNPAID_STORE" wake --graph "$GRAPH" "${AGENTS[@]}" \
     >"$OUT-a-wake.out" 2>&1; then
  cat "$OUT-a-wake.out"
  die "\`salvor wake\` to drive the due run and exit 0" "it exited nonzero; its output is above"
fi
cat "$OUT-a-wake.out"
if grep -qF "$A_ID is now completed" "$OUT-a-wake.out"; then
  echo "PROOF: \`salvor wake\` drove the run all the way to completed, with no resume input and nobody in the loop."
else
  fail "\`salvor wake\` to report \`$A_ID is now completed\`" "$(cat "$OUT-a-wake.out")"
fi

echo
echo "== the whole recorded walk, reminder to escalation, across the nap =="
A_HIST=$("$SALVOR" --store "$UNPAID_STORE" history "$A_ID") \
  || die "the woken run's log to read back" "\`salvor history $A_ID\` exited nonzero"
printf '%s\n' "$A_HIST"

# --- The proof, assertion by assertion. ---
echo
A_EXIT_REMINDER=$(seq_of "$A_HIST" 'exit send_reminder')
A_SLEEP_START=$(seq_of "$A_HIST" 'SleepStarted')
A_SLEEP_DONE=$(seq_of "$A_HIST" 'SleepCompleted')
if [[ -n "$A_EXIT_REMINDER" && -n "$A_SLEEP_START" && -n "$A_SLEEP_DONE" ]] \
   && (( A_EXIT_REMINDER < A_SLEEP_START )) && (( A_SLEEP_START < A_SLEEP_DONE )); then
  echo "PROOF: the recorded order is send_reminder exited (seq $A_EXIT_REMINDER), then SleepStarted (seq $A_SLEEP_START), then SleepCompleted (seq $A_SLEEP_DONE): the reminder went out before the nap and the nap really ended."
else
  fail "the log to hold \`exit send_reminder\`, then SleepStarted, then SleepCompleted, in that order" \
       "exit send_reminder at seq ${A_EXIT_REMINDER:-nowhere}, SleepStarted at seq ${A_SLEEP_START:-nowhere}, SleepCompleted at seq ${A_SLEEP_DONE:-nowhere}"
fi

A_SLEEPS=$(grep -c 'SleepStarted' <<<"$A_HIST") || A_SLEEPS=0
A_WAKES=$(grep -c 'SleepCompleted' <<<"$A_HIST") || A_WAKES=0
if [[ "$A_SLEEPS" == "1" && "$A_WAKES" == "1" ]]; then
  echo "PROOF: exactly one SleepStarted and one SleepCompleted, so the refused drive and the dry run left no second nap behind."
else
  fail "exactly one SleepStarted and one SleepCompleted" "$A_SLEEPS started, $A_WAKES completed"
fi

if grep -qF 'branch route -> unpaid' <<<"$A_HIST"; then
  echo "PROOF: the branch recorded \`route -> unpaid\`: nothing had been paid when the run looked."
else
  fail "a \`branch route -> unpaid\` line in the log" "$(grep 'BranchTaken' <<<"$A_HIST" || printf 'no BranchTaken at all')"
fi

check "$(count_lines "$A_REMINDERS")" "1" \
  "the reminders ledger still holds exactly 1 line after the wake: the reminder was not sent twice." \
  "1 line in $A_REMINDERS after the wake"
check "$(count_lines "$A_ESCALATIONS")" "1" \
  "the escalations ledger holds 1 line: the unpaid arm ran." \
  "1 line in $A_ESCALATIONS"
check "$(count_lines "$A_CLOSED")" "0" \
  "the closed ledger holds 0 lines: the paid arm did not." \
  "0 lines in $A_CLOSED"

echo
echo "############################################"
echo "# 5. Leg B, the payment lands during the nap"
echo "############################################"
# Same document, same input, a fresh store, and the same starting world: nothing
# paid. The only difference is what happens WHILE the run is asleep.
export SALVOR_FOLLOWUP_REMINDERS="$B_REMINDERS"
export SALVOR_FOLLOWUP_CLOSED="$B_CLOSED"
export SALVOR_FOLLOWUP_ESCALATIONS="$B_ESCALATIONS"
printf '%s\n' '{}' >"$PAYMENTS"

if ! "$SALVOR" --store "$PAID_STORE" graph run "$GRAPH" \
     --input "$INPUT" "${AGENTS[@]}" --label desk=accounts >"$OUT-b-run.out" 2>&1; then
  cat "$OUT-b-run.out"
  die "leg B's run to reach the delay and park" "\`graph run\` exited nonzero; its output is above"
fi
tail -4 "$OUT-b-run.out"
B_ID=$(grep -oE 'run [0-9a-f-]{36}' "$OUT-b-run.out" | head -1 | awk '{print $2}')
B_WAKE_AT=$(sed -n 's/^ *sleeping until: *//p' "$OUT-b-run.out" | head -1)
[[ -n "$B_ID" && -n "$B_WAKE_AT" ]] \
  || die "leg B to print a run id and a \`sleeping until:\` instant" "$(cat "$OUT-b-run.out")"
check "$(count_lines "$B_REMINDERS")" "1" \
  "leg B sent its one reminder and parked." \
  "1 line in $B_REMINDERS"

# THE WORLD MOVES WHILE THE RUN SLEEPS. Nothing tells the run; nothing can. The
# run is not watching the payments file, it is not subscribed to anything, and it
# holds no process that could be notified. It has an instant in its log.
cat >"$PAYMENTS" <<JSON
{"$INVOICE": {"paid": true, "received": "2026-08-15"}}
JSON
echo "== the payments file, rewritten while run $B_ID sleeps =="
cat "$PAYMENTS"

B_WAIT=$(seconds_until "$B_WAKE_AT")
echo "== waiting ${B_WAIT}s for the deadline at $B_WAKE_AT =="
sleep "$B_WAIT"

if ! "$SALVOR" --store "$PAID_STORE" wake --graph "$GRAPH" "${AGENTS[@]}" \
     >"$OUT-b-wake.out" 2>&1; then
  cat "$OUT-b-wake.out"
  die "\`salvor wake\` to drive leg B's due run and exit 0" "it exited nonzero; its output is above"
fi
tail -6 "$OUT-b-wake.out"
if ! grep -qF "$B_ID is now completed" "$OUT-b-wake.out"; then
  fail "\`salvor wake\` to report \`$B_ID is now completed\`" "$(cat "$OUT-b-wake.out")"
fi

B_HIST=$("$SALVOR" --store "$PAID_STORE" history "$B_ID") \
  || die "leg B's log to read back" "\`salvor history $B_ID\` exited nonzero"
echo
grep -E 'SleepStarted|SleepCompleted|BranchTaken|RunCompleted' <<<"$B_HIST" \
  || fail "the sleep, branch and completion lines in leg B's log" "$B_HIST"

echo
if grep -qF 'branch route -> paid' <<<"$B_HIST"; then
  echo "PROOF: the branch recorded \`route -> paid\`: the run read the world as it was AT THE WAKE, not as it was when it fell asleep."
else
  fail "a \`branch route -> paid\` line in leg B's log" \
       "$(grep 'BranchTaken' <<<"$B_HIST" || printf 'no BranchTaken at all')"
fi
check "$(count_lines "$B_CLOSED")" "1" \
  "leg B's closed ledger holds 1 line: the paid arm ran." \
  "1 line in $B_CLOSED"
check "$(count_lines "$B_ESCALATIONS")" "0" \
  "leg B's escalations ledger holds 0 lines: the unpaid arm did not." \
  "0 lines in $B_ESCALATIONS"
check "$(count_lines "$B_REMINDERS")" "1" \
  "leg B's reminder still went out exactly once, across its own nap." \
  "1 line in $B_REMINDERS after the wake"
echo "PROOF: the same document and the same input took opposite arms, decided entirely by when the run looked: a delay records an instant and nothing else, so it carries no stale copy of the world across the wait."

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held =="
else
  echo "== FAILURES above =="
  exit 1
fi
