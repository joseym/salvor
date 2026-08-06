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

# Default to a checkout's build, but let an installed CLI drive instead.
SALVOR="${SALVOR_BIN:-$ROOT/target/debug/salvor}"
DEMO_MODEL="${SALVOR_DEMO_MODEL_BIN:-$ROOT/target/debug/salvor-demo-model}"

for bin in "$SALVOR" "$DEMO_MODEL"; do
  if [[ ! -x "$bin" ]]; then
    echo "missing $bin; build it first with:  cargo build" >&2
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

RUN_PID=""
MODEL_PID=""
cleanup() {
  # Only ever by the exact pid this script recorded, never by pattern.
  [[ -n "$RUN_PID" ]] && kill -9 "$RUN_PID" 2>/dev/null || true
  [[ -n "$MODEL_PID" ]] && kill "$MODEL_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "############################################"
echo "# 1. The static check: a typoed reference never reaches a run"
echo "############################################"
"$SALVOR" graph validate "$GRAPH"
# The same document with one letter moved in the `stop_when` predicate. The
# validator reads a fold's expressions against its BODY NODE's declared
# output_schema, so this is caught at submit rather than becoming a loop that
# silently never stops.
sed 's/"score >= 0.8"/"scoer >= 0.8"/' "$GRAPH" >"$TYPO_GRAPH"
if "$SALVOR" graph validate "$TYPO_GRAPH" >"$SCRATCH/salvor-refine-typo.out" 2>&1; then
  echo "FAILED: the validator accepted a stop_when the body schema does not describe"; FAIL=1
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
head -2 "$MODEL_LOG"

echo
echo "############################################"
echo "# 2. The control run: uninterrupted, start to finish"
echo "############################################"
BEFORE=$(model_requests)
"$SALVOR" --store "$CONTROL_STORE" graph run "$GRAPH" \
  --input @examples/refine/input.json "${AGENTS[@]}" \
  --label desk=payroll >"$SCRATCH/salvor-refine-control.out" 2>&1
cat "$SCRATCH/salvor-refine-control.out"
CONTROL_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-refine-control.out" | head -1 | awk '{print $2}')
[[ -n "$CONTROL_ID" ]] || { echo "FAILED: no run id"; exit 1; }
CONTROL_CALLS=$(( $(model_requests) - BEFORE ))
CONTROL_EVENTS=$(event_kinds "$CONTROL_STORE" "$CONTROL_ID" | grep -c .)
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
    break
  fi
  sleep 0.05
done
[[ -n "$CRASH_ID" ]] || { echo "FAILED: no run id"; exit 1; }
kill -9 "$RUN_PID"
wait "$RUN_PID" 2>/dev/null || true
echo "killed pid $RUN_PID inside pass 1"
RUN_PID=""
CRASH_CALLS=$(( $(model_requests) - BEFORE ))

echo
echo "== the recorded log stops at an unanswered model call =="
"$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | tail -4

echo
echo "############################################"
echo "# 4. Resume: pass 0 replays from the log, pass 1 is re-driven"
echo "############################################"
BEFORE=$(model_requests)
"$SALVOR" --store "$CRASH_STORE" resume "$CRASH_ID" --graph "$GRAPH" "${AGENTS[@]}" \
  >"$SCRATCH/salvor-refine-resume.out" 2>&1
# The last four lines are the value the run produced, printed whole.
tail -4 "$SCRATCH/salvor-refine-resume.out"
RESUME_CALLS=$(( $(model_requests) - BEFORE ))

echo
echo "== the recovered walk, in full =="
"$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID"

# --- The proof, assertion by assertion. ---
echo
CONVERGED=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | grep 'FoldConverged' || true)
if grep -q 'fold refine converged on \[1\]: stop_when `score >= 0.8` held after pass 1' <<<"$CONVERGED"; then
  echo "PROOF: the recorded convergence names the winner and the cause:"
  sed 's/^ */       /' <<<"$CONVERGED"
else
  echo "FAILED: the log does not carry the expected FoldConverged line: $CONVERGED"; FAIL=1
fi

# `best_by: score` is an argmax over EVERY pass, and the winning pass number is
# RECORDED, not left to a reader to work out. Pass 0's 0.55 draft is still in
# the log beside it, so the choice can be checked rather than trusted.
LOG_JSON=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" --json)
if grep -q '"winner_index": 1' <<<"$LOG_JSON" \
   && grep -q 'rev A1' <<<"$LOG_JSON" && grep -q '"score": 0.55' <<<"$LOG_JSON"; then
  echo "PROOF: winner_index 1 is recorded, with the losing pass ([rev A1], 0.55) still in the log to check it against."
else
  echo "FAILED: the log does not carry winner_index 1 beside the losing pass"; FAIL=1
fi

# And the value the node produced is that winner's draft, not the last thing
# that happened to be said.
if grep -q 'rev A2' "$SCRATCH/salvor-refine-resume.out" \
   && grep -q '"score": 0.85' "$SCRATCH/salvor-refine-resume.out"; then
  echo "PROOF: the fold produced the winning pass's draft, [rev A2] at 0.85."
else
  echo "FAILED: the produced value is not the winning draft"; FAIL=1
fi

STARTS=$("$SALVOR" --store "$CRASH_STORE" history "$CRASH_ID" | grep -c 'FoldIterationStarted' || true)
if [[ "$STARTS" == "2" ]]; then
  echo "PROOF: 2 passes started under a bound of 4, so passes 2 and 3 were never asked for."
else
  echo "FAILED: expected 2 FoldIterationStarted events, found $STARTS"; FAIL=1
fi

CRASH_EVENTS=$(event_kinds "$CRASH_STORE" "$CRASH_ID" | grep -c .)
if [[ "$CRASH_EVENTS" == "$CONTROL_EVENTS" ]] \
   && diff <(event_kinds "$CONTROL_STORE" "$CONTROL_ID") \
           <(event_kinds "$CRASH_STORE" "$CRASH_ID") >/dev/null; then
  echo "PROOF: the crashed and resumed run records the same $CRASH_EVENTS events, in the same order, as the uninterrupted control run."
else
  echo "FAILED: the recovered log differs from the control run ($CRASH_EVENTS events vs $CONTROL_EVENTS)"; FAIL=1
fi

# The crash leg spent both of the control's calls (pass 0, and pass 1 in flight
# when the process died); the resume spent one, re-driving that interrupted call
# and nothing else. Pass 0 came back off the log for free.
TOTAL_CALLS=$(( CRASH_CALLS + RESUME_CALLS ))
if [[ "$CRASH_CALLS" == "$CONTROL_CALLS" ]] && [[ "$RESUME_CALLS" == "1" ]] \
   && [[ "$TOTAL_CALLS" == "$(( CONTROL_CALLS + 1 ))" ]]; then
  echo "PROOF: $TOTAL_CALLS model calls across the crash against $CONTROL_CALLS uninterrupted: exactly the one interrupted call was re-driven, and pass 0 replayed free."
else
  echo "FAILED: expected $CONTROL_CALLS calls before the kill and 1 after, found $CRASH_CALLS and $RESUME_CALLS"; FAIL=1
fi

echo
echo "############################################"
echo "# 5. The other arm: a notice that never clears the threshold"
echo "############################################"
# Same document, same agent, a correction nobody can put in one plain paragraph.
# Four passes, none of them at 0.8, and `on_bound: fail` says what that means:
# not a winner, a failure.
"$SALVOR" --store "$BOUND_STORE" graph run "$GRAPH" \
  --input @examples/refine/input-stubborn.json "${AGENTS[@]}" \
  --label desk=payroll >"$SCRATCH/salvor-refine-bound.out" 2>&1 || true
tail -3 "$SCRATCH/salvor-refine-bound.out"
BOUND_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-refine-bound.out" | head -1 | awk '{print $2}')
[[ -n "$BOUND_ID" ]] || { echo "FAILED: no run id"; exit 1; }
BOUND_HIST=$("$SALVOR" --store "$BOUND_STORE" history "$BOUND_ID")
echo
grep -E 'FoldIterationJoined|RunFailed' <<<"$BOUND_HIST"

JOINS=$(grep -c 'FoldIterationJoined' <<<"$BOUND_HIST" || true)
if [[ "$JOINS" == "4" ]] && grep -q 'RunFailed' <<<"$BOUND_HIST" \
   && ! grep -q 'FoldConverged' <<<"$BOUND_HIST"; then
  echo "PROOF: 4 passes are in the log and the run is RunFailed with no FoldConverged: the work is recorded, the convergence is refused."
else
  echo "FAILED: the bound arm did not record 4 joins and a failure with no convergence"; FAIL=1
fi

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held; tearing down the model server =="
else
  echo "== FAILURES above; tearing down the model server =="
  exit 1
fi
