#!/usr/bin/env bash
# Drive the refine-notice graph end to end, offline, including a crash mid-pass.
#
#   validate -> a control run that converges on pass 1 -> the same run again,
#       killed -9 while pass 1's model call is in flight -> resume -> the same
#       log, event for event, with exactly one model call re-driven
#
# Then the other arm of the same document: a notice that never clears the desk
# threshold, where the fold's `on_bound: fail` refuses to call four failures a
# convergence. A scripted model server stands in for a real endpoint, so nothing
# here needs an API key or a network.
#
# The failure mode this kills: a loop that reports a winner nobody can check. A
# fold records which pass won and why it stopped, so the desk reads the verdict
# out of the log rather than re-deriving it from four drafts and a threshold.
#
# Usage, from anywhere:
#     examples/refine/run.sh
#
# It drives the binaries a checkout builds, preferring `target/debug` and
# falling back to `target/release`, so either `cargo build` or
# `cargo build --release` is enough. `SALVOR_BIN` and `SALVOR_DEMO_MODEL_BIN`
# override those paths outright, which is how an already-installed CLI drives
# this instead. Ports and store paths are overridable too; see the block below.
set -euo pipefail

# Repository root, two levels up from this script. Everything below runs from
# there, so the paths printed here are the paths the READMEs use.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. The
# model port default is deliberately far up the range and distinct from every
# other example's: this script must never collide with a control plane, a dev
# server, or another example's model. Nothing here binds a control-plane port at
# all, because `salvor graph run` drives the store directly rather than over
# HTTP.
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-18951}"
# Long enough that the kill below reliably lands INSIDE pass 1's model call,
# which is the recoverable window. `examples/reconciliation/` covers the other
# one, a kill mid-write.
MODEL_DELAY_MS="${SALVOR_EXAMPLE_MODEL_DELAY_MS:-2000}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
CONTROL_STORE="${SALVOR_EXAMPLE_CONTROL_STORE:-$SCRATCH/salvor-refine-control.db}"
CRASH_STORE="${SALVOR_EXAMPLE_CRASH_STORE:-$SCRATCH/salvor-refine-crash.db}"
BOUND_STORE="${SALVOR_EXAMPLE_BOUND_STORE:-$SCRATCH/salvor-refine-bound.db}"
MODEL_LOG="${SALVOR_EXAMPLE_MODEL_LOG:-$SCRATCH/salvor-refine-model.log}"
TYPO_GRAPH="$SCRATCH/salvor-refine-typo.json"

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
DEMO_MODEL="${SALVOR_DEMO_MODEL_BIN:-$(built_bin salvor-demo-model || true)}"

for pair in "salvor|$SALVOR|SALVOR_BIN" "salvor-demo-model|$DEMO_MODEL|SALVOR_DEMO_MODEL_BIN"; do
  bin_name="${pair%%|*}"
  bin_rest="${pair#*|}"
  bin_path="${bin_rest%%|*}"
  bin_var="${bin_rest#*|}"
  if [[ -z "$bin_path" ]]; then
    echo "missing the \`$bin_name\` binary: no target/debug/$bin_name and no target/release/$bin_name." >&2
    echo "Build it with:  cargo build      (or: cargo build --release)" >&2
    echo "Or point $bin_var at one you already have." >&2
    exit 1
  fi
  if [[ ! -x "$bin_path" ]]; then
    echo "$bin_var names \`$bin_path\`, which is not an executable file." >&2
    exit 1
  fi
done

GRAPH="examples/refine/refine-notice.json"
AGENT="examples/refine/agents/tailor.toml"
AGENTS=(--agent "$AGENT")

# Every run starts from a clean store, so the event counts below mean what they
# say. Only files this script owns are removed.
rm -f "$CONTROL_STORE" "$CONTROL_STORE-wal" "$CONTROL_STORE-shm" \
      "$CRASH_STORE" "$CRASH_STORE-wal" "$CRASH_STORE-shm" \
      "$BOUND_STORE" "$BOUND_STORE-wal" "$BOUND_STORE-shm" \
      "$MODEL_LOG" "$TYPO_GRAPH"

# Salvor's own event lines are the story; the MCP client library's handshake
# chatter is not. Quiet that one target and leave everything else at info, and
# only when the caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-info,rmcp=warn}"

FAIL=0
RUN_PID=""
MODEL_PID=""

cleanup() {
  # Only ever by the exact pid this script recorded, never by pattern, and never
  # by process group: a negative pid here would reach the model server, this
  # shell, and anything else sharing the group.
  [[ -n "$RUN_PID" ]] && kill -9 "$RUN_PID" 2>/dev/null || true
  [[ -n "$MODEL_PID" ]] && kill "$MODEL_PID" 2>/dev/null || true
}
trap cleanup EXIT

# --- Saying what failed, every time ---
#
# Every check below names itself when it fails. A bare `grep` or `[[ ... ]]`
# under `set -euo pipefail` exits 1 having printed NOTHING, and a proof script
# that can die silently is worse than one that proves less: a reader cannot tell
# a run that checked everything from one that stopped before it got there. So
# each check goes through `fail`, `die`, or `check`, and each says what was
# expected and what was actually there.

# What to add to any failure when the scripted model is no longer running.
# Named separately because a leg that fails just after that server died is the
# SERVER's failure wearing the product's clothes: the CLI reports an `HTTP
# transport failure`, which reads as a Salvor bug and is not one.
model_report() {
  [[ -n "$MODEL_PID" ]] || return 0
  if kill -0 "$MODEL_PID" 2>/dev/null; then
    return 0
  fi
  echo "       NOTE: the scripted model server (salvor-demo-model, pid $MODEL_PID) is NOT running."
  echo "       It died before or during this step, so any transport failure above is that dead"
  echo "       server and not the product. Its log is $MODEL_LOG; its last lines:"
  tail -5 "$MODEL_LOG" 2>/dev/null | sed 's/^/         /' || true
}

# fail <what was expected> [<what was actually there>]
fail() {
  echo "FAILED: expected $1"
  if [[ -n "${2:-}" ]]; then
    printf '%s\n' "$2" | sed 's/^/       actual: /'
  fi
  model_report
  FAIL=1
}

# die <what was expected> [<what was actually there>]: a failure nothing after
# it can be read past, so the script stops here rather than cascading.
die() {
  fail "$@"
  exit 1
}

# How many model requests the scripted server has answered so far. Every leg
# reads this before and after itself, so a leg's cost is a delta rather than a
# total, and the re-drive proof is arithmetic on two numbers.
model_requests() {
  local count
  # A log with no request lines yet is zero, not an error: the assignment
  # failing is what carries that, so the count is always one bare number.
  count=$(grep -c 'request #' "$MODEL_LOG" 2>/dev/null) || count=0
  printf '%s' "$count"
}

# The kind column of a recorded log, one event per line. Two runs of the same
# document must produce the same sequence of kinds, whatever their run ids and
# timestamps are, so this is what a crashed run gets compared against.
event_kinds() {
  "$SALVOR" --store "$1" history "$2" | awk '{print $4}'
}

# The number of events in a recorded log, as one bare number. An empty log is
# zero rather than an error, which is what the `|| true` after `grep -c` carries.
count_events() {
  local kinds
  kinds=$(event_kinds "$1" "$2") \
    || die "run $2's log in $1 to read back" "\`salvor history\` exited nonzero"
  grep -c . <<<"$kinds" || true
}

echo "############################################"
echo "# 1. The static check: a typoed reference never reaches a run"
echo "############################################"
"$SALVOR" graph validate "$GRAPH" \
  || die "the committed document to validate" "\`salvor graph validate $GRAPH\` exited nonzero; its output is above"
# The same document with one letter moved in the `stop_when` predicate. The
# validator reads a fold's expressions against its BODY NODE's declared
# output_schema, so this is caught at submit rather than becoming a loop that
# silently never stops.
sed 's/"score >= 0.8"/"scoer >= 0.8"/' "$GRAPH" >"$TYPO_GRAPH" \
  || die "the typoed copy of the document to be written" "sed into $TYPO_GRAPH failed"
if "$SALVOR" graph validate "$TYPO_GRAPH" >"$SCRATCH/salvor-refine-typo.out" 2>&1; then
  fail "the validator to refuse a stop_when the body schema does not describe" \
       "$(cat "$SCRATCH/salvor-refine-typo.out" 2>/dev/null)"
else
  cat "$SCRATCH/salvor-refine-typo.out"
  echo "PROOF: a fold reference the body's output_schema does not describe is refused at validate."
fi

echo
echo "== starting salvor-demo-model on 127.0.0.1:$MODEL_PORT =="
"$DEMO_MODEL" --port "$MODEL_PORT" --delay-ms "$MODEL_DELAY_MS" \
  --script "$HERE/model-script.json" >"$MODEL_LOG" 2>&1 &
MODEL_PID=$!
# The scripted model answers by needle, so a request carrying a pass input the
# script does not cover is a loud 500 rather than a wrong answer.
export SALVOR_DEMO_BASE_URL="http://127.0.0.1:$MODEL_PORT"
for _ in $(seq 1 50); do
  grep -q listening "$MODEL_LOG" 2>/dev/null && break
  sleep 0.1
done
head -2 "$MODEL_LOG" 2>/dev/null || true
# Nothing past this point means anything if the server never came up, and the
# usual reason it does not is a port already taken, which its own log says.
grep -q listening "$MODEL_LOG" 2>/dev/null \
  || die "salvor-demo-model to be listening on 127.0.0.1:$MODEL_PORT within 5s (SALVOR_EXAMPLE_MODEL_PORT moves it)" \
         "$(cat "$MODEL_LOG" 2>/dev/null)"

echo
echo "############################################"
echo "# 2. The control run: uninterrupted, start to finish"
echo "############################################"
BEFORE=$(model_requests)
if ! "$SALVOR" --store "$CONTROL_STORE" graph run "$GRAPH" \
     --input @examples/refine/input.json "${AGENTS[@]}" \
     --label desk=payroll >"$SCRATCH/salvor-refine-control.out" 2>&1; then
  cat "$SCRATCH/salvor-refine-control.out"
  die "the control run to complete" "\`graph run\` exited nonzero; its output is above"
fi
cat "$SCRATCH/salvor-refine-control.out"
CONTROL_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-refine-control.out" | head -1 | awk '{print $2}')
[[ -n "$CONTROL_ID" ]] \
  || die "the control run to print a \`run <id>\` line" "$(cat "$SCRATCH/salvor-refine-control.out" 2>/dev/null)"
CONTROL_CALLS=$(( $(model_requests) - BEFORE ))
CONTROL_EVENTS=$(count_events "$CONTROL_STORE" "$CONTROL_ID")
echo "control run: $CONTROL_EVENTS events, $CONTROL_CALLS model calls"

echo
echo "############################################"
echo "# 3. The same run again, killed -9 inside pass 1"
echo "############################################"
BEFORE=$(model_requests)
"$SALVOR" --store "$CRASH_STORE" graph run "$GRAPH" \
  --input @examples/refine/input.json "${AGENTS[@]}" \
  --label desk=payroll >"$SCRATCH/salvor-refine-crash.out" 2>&1 &
RUN_PID=$!

# Wait until pass 1's model call is in flight: pass 0 is durably joined, pass 1
# has started, and its request is recorded with no completion after it. That is
# a deterministic point in the log, not a wall-clock guess.
CRASH_ID=""
WINDOW=""
for _ in $(seq 1 600); do
  if [[ -z "$CRASH_ID" ]]; then
    # The run id is printed first; until it is there, there is nothing to read
    # a log for, and a grep that found nothing yet is not an error.
    CRASH_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-refine-crash.out" 2>/dev/null \
      | head -1 | awk '{print $2}' || true)
    sleep 0.05
    continue
  fi
  HIST=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" 2>/dev/null || true)
  REQUESTS=$(grep -c 'ModelCallRequested' <<<"$HIST" || true)
  if grep -q 'fold refine\[1\] started' <<<"$HIST" && [[ "$REQUESTS" == "2" ]]; then
    WINDOW="reached"
    break
  fi
  sleep 0.05
done
[[ -n "$CRASH_ID" ]] \
  || die "the crash leg to print a \`run <id>\` line within 30s" \
         "$(cat "$SCRATCH/salvor-refine-crash.out" 2>/dev/null)"
[[ -n "$WINDOW" ]] \
  || die "pass 1's model call to be in flight within 30s (pass 0 joined, pass 1 started, 2 model requests recorded)" \
         "$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" 2>/dev/null || true)"
# The kill proves something only if there is a live run for it to land on. A
# process that already exited means the window above was misjudged, which is a
# different failure and says so instead of dying on `kill`'s own exit status.
kill -0 "$RUN_PID" 2>/dev/null \
  || die "the crash leg's run to still be running when the kill lands" \
         "pid $RUN_PID had already exited, having printed: $(cat "$SCRATCH/salvor-refine-crash.out" 2>/dev/null)"
kill -9 "$RUN_PID" || die "the kill -9 to land on the crash leg's run" "\`kill -9 $RUN_PID\` failed"
wait "$RUN_PID" 2>/dev/null || true
echo "killed pid $RUN_PID inside pass 1"
RUN_PID=""
CRASH_CALLS=$(( $(model_requests) - BEFORE ))

echo
echo "== the recorded log stops at an unanswered model call =="
"$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | tail -4 \
  || die "the crashed run's log to read back" "\`salvor history $CRASH_ID\` exited nonzero"

echo
echo "############################################"
echo "# 4. Resume: pass 0 replays from the log, pass 1 is re-driven"
echo "############################################"
BEFORE=$(model_requests)
if ! "$SALVOR" --store "$CRASH_STORE" resume "$CRASH_ID" --graph "$GRAPH" "${AGENTS[@]}" \
     >"$SCRATCH/salvor-refine-resume.out" 2>&1; then
  cat "$SCRATCH/salvor-refine-resume.out"
  die "the resume to complete the crashed run" \
      "\`salvor resume $CRASH_ID\` exited nonzero; its output is above"
fi
# The last four lines are the value the run produced, printed whole.
tail -4 "$SCRATCH/salvor-refine-resume.out"
RESUME_CALLS=$(( $(model_requests) - BEFORE ))

echo
echo "== the recovered walk, in full =="
"$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" \
  || die "the recovered run's log to read back" "\`salvor history $CRASH_ID\` exited nonzero"

# --- The proof, assertion by assertion. ---
echo
CONVERGED=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | grep 'FoldConverged' || true)
if grep -q 'fold refine converged on \[1\]: stop_when held after pass 1: `score >= 0.8`' <<<"$CONVERGED"; then
  echo "PROOF: the recorded convergence names the winner and the cause:"
  sed 's/^ */       /' <<<"$CONVERGED"
else
  fail 'a FoldConverged line reading: fold refine converged on [1]: stop_when held after pass 1: `score >= 0.8`' \
       "$CONVERGED"
fi

# `best_by: score` is an argmax over EVERY pass, and the winning pass number is
# RECORDED, not left to a reader to work out. Pass 0's 0.55 draft is still in
# the log beside it, so the choice can be checked rather than trusted.
LOG_JSON=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" --json) \
  || die "the recovered run's log to read back as JSON" "\`salvor history $CRASH_ID --json\` exited nonzero"
if grep -q '"winner_index": 1' <<<"$LOG_JSON" \
   && grep -q 'rev A1' <<<"$LOG_JSON" && grep -q '"score": 0.55' <<<"$LOG_JSON"; then
  echo "PROOF: winner_index 1 is recorded, with the losing pass ([rev A1], 0.55) still in the log to check it against."
else
  fail "winner_index 1 in the log beside the losing pass ([rev A1] at 0.55)" \
       "$(grep -E 'winner_index|rev A1|"score"' <<<"$LOG_JSON" \
          || printf 'no winner_index, no [rev A1], and no score anywhere in the log')"
fi

# And the value the node produced is that winner's draft, not the last thing
# that happened to be said.
RESUME_OUT=$(cat "$SCRATCH/salvor-refine-resume.out")
if grep -q 'rev A2' <<<"$RESUME_OUT" && grep -q '"score": 0.85' <<<"$RESUME_OUT"; then
  echo "PROOF: the fold produced the winning pass's draft, [rev A2] at 0.85."
else
  fail "the produced value to be the winning draft, [rev A2] at 0.85" "$RESUME_OUT"
fi

STARTS=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | grep -c 'FoldIterationStarted' || true)
if [[ "$STARTS" == "2" ]]; then
  echo "PROOF: 2 passes started under a bound of 4, so passes 2 and 3 were never asked for."
else
  fail "2 FoldIterationStarted events under a bound of 4" "$STARTS of them"
fi

CRASH_EVENTS=$(count_events "$CRASH_STORE" "$CRASH_ID")
if [[ "$CRASH_EVENTS" == "$CONTROL_EVENTS" ]] \
   && diff <(event_kinds "$CONTROL_STORE" "$CONTROL_ID") \
           <(event_kinds "$CRASH_STORE" "$CRASH_ID") >/dev/null; then
  echo "PROOF: the crashed and resumed run records the same $CRASH_EVENTS events, in the same order, as the uninterrupted control run."
else
  fail "the recovered log to match the control run's $CONTROL_EVENTS events, kind for kind" \
       "$CRASH_EVENTS events, differing from the control run like this:
$(diff <(event_kinds "$CONTROL_STORE" "$CONTROL_ID") <(event_kinds "$CRASH_STORE" "$CRASH_ID") || true)"
fi

# The crash leg spent both of the control's calls (pass 0, and pass 1 in flight
# when the process died); the resume spent one, re-driving that interrupted call
# and nothing else. Pass 0 came back off the log for free.
TOTAL_CALLS=$(( CRASH_CALLS + RESUME_CALLS ))
if [[ "$CRASH_CALLS" == "$CONTROL_CALLS" ]] && [[ "$RESUME_CALLS" == "1" ]] \
   && [[ "$TOTAL_CALLS" == "$(( CONTROL_CALLS + 1 ))" ]]; then
  echo "PROOF: $TOTAL_CALLS model calls across the crash against $CONTROL_CALLS uninterrupted: exactly the one interrupted call was re-driven, and pass 0 replayed free."
else
  fail "$CONTROL_CALLS model calls before the kill and exactly 1 after" \
       "$CRASH_CALLS before and $RESUME_CALLS after"
fi

echo
echo "############################################"
echo "# 5. The other arm: a notice that never clears the threshold"
echo "############################################"
# Same document, same agent, a correction nobody can put in one plain paragraph.
# Four passes, none of them at 0.8, and `on_bound: fail` says what that means:
# not a winner, a failure.
if "$SALVOR" --store "$BOUND_STORE" graph run "$GRAPH" \
   --input @examples/refine/input-stubborn.json "${AGENTS[@]}" \
   --label desk=payroll >"$SCRATCH/salvor-refine-bound.out" 2>&1; then
  cat "$SCRATCH/salvor-refine-bound.out"
  fail "the bound arm to refuse and exit nonzero (on_bound: fail)" "it exited 0; its output is above"
fi
tail -3 "$SCRATCH/salvor-refine-bound.out"
BOUND_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-refine-bound.out" | head -1 | awk '{print $2}')
[[ -n "$BOUND_ID" ]] \
  || die "the bound arm to print a \`run <id>\` line" "$(cat "$SCRATCH/salvor-refine-bound.out" 2>/dev/null)"
BOUND_HIST=$("$SALVOR" --store "$BOUND_STORE" history "$BOUND_ID") \
  || die "the bound arm's log to read back" "\`salvor history $BOUND_ID\` exited nonzero"
echo
BOUND_LINES=$(grep -E 'FoldIterationJoined|RunFailed' <<<"$BOUND_HIST" || true)
if [[ -n "$BOUND_LINES" ]]; then
  printf '%s\n' "$BOUND_LINES"
else
  fail "FoldIterationJoined and RunFailed lines in the bound arm's log" "$BOUND_HIST"
fi

JOINS=$(grep -c 'FoldIterationJoined' <<<"$BOUND_HIST" || true)
if [[ "$JOINS" == "4" ]] && grep -q 'RunFailed' <<<"$BOUND_HIST" \
   && ! grep -q 'FoldConverged' <<<"$BOUND_HIST"; then
  echo "PROOF: 4 passes are in the log and the run is RunFailed with no FoldConverged: the work is recorded, the convergence is refused."
else
  fail "4 FoldIterationJoined events and a RunFailed, with no FoldConverged" \
       "$JOINS joins, $(grep -c 'RunFailed' <<<"$BOUND_HIST" || true) RunFailed, $(grep -c 'FoldConverged' <<<"$BOUND_HIST" || true) FoldConverged"
fi

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held; tearing down the model server =="
else
  echo "== FAILURES above; tearing down the model server =="
  exit 1
fi
