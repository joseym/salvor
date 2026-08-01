#!/usr/bin/env bash
# Drive the payroll-run graph end to end, offline, including a crash mid-batch.
#
#   run -> flag exceptions -> park at the human gate -> approve the amended
#       roster -> pay each employee, and kill -9 part way through the batch
#       -> recover -> finish, with every employee paid exactly once
#
# Then the other arm of the same graph: a clean roster that routes past the gate
# with no human in the loop at all. A scripted model server stands in for a real
# endpoint, so nothing here needs an API key or a network. Every payment lands in
# a scratch ledger this script owns.
#
# The two failure modes this kills: paid-twice (a crash that re-charges an
# employee the batch already paid) and never-paid (a crash that drops an employee
# the batch had not reached yet). Neither happens: the map replays its finished
# iterations from the log and re-drives only the unfinished ones, and the pay tool
# is exactly-once per employee besides.
#
# Usage, from anywhere:
#     examples/payroll/run.sh
set -euo pipefail

# Repository root, two levels up from this script. The agent definition names its
# MCP server by a path relative to the directory salvor is invoked from, so
# everything below runs from the root.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. The
# model port default is deliberately far up the range: this script must never
# collide with a control plane, a dev server, or anything else already holding a
# conventional port. Nothing here binds a control-plane port at all, because
# `salvor graph run` drives the store directly rather than over HTTP.
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-18946}"
MODEL_DELAY_MS="${SALVOR_EXAMPLE_MODEL_DELAY_MS:-50}"
# A per-payment delay so the kill below reliably lands INSIDE the batch rather
# than after it. The pay tool is idempotent, so a kill anywhere in the batch
# (even mid-write) recovers; the delay just makes the recoverable window wide
# enough to aim at.
PAY_DELAY_MS="${SALVOR_EXAMPLE_PAY_DELAY_MS:-300}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
STORE="${SALVOR_EXAMPLE_STORE:-$SCRATCH/salvor-payroll.db}"
LEDGER="${SALVOR_EXAMPLE_LEDGER:-$SCRATCH/salvor-payroll-ledger.txt}"
NOTICES="${SALVOR_EXAMPLE_NOTICES:-$SCRATCH/salvor-payroll-notices.txt}"
MODEL_LOG="${SALVOR_EXAMPLE_MODEL_LOG:-$SCRATCH/salvor-payroll-model.log}"

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

GRAPH="examples/payroll/payroll-run.json"
AGENT="examples/payroll/agents/notify-summary.toml"
AGENTS=(--agent "$AGENT")

# Every run starts from a clean store and a clean ledger, so the line counts below
# mean what they say. Only files this script owns are removed.
rm -f "$STORE" "$STORE-wal" "$STORE-shm" "$LEDGER" "$NOTICES"

# The MCP server writes here; the agent passes these through to its child process
# by ordinary environment inheritance, so no checked-in file changes.
export SALVOR_PAYROLL_LEDGER="$LEDGER"
export SALVOR_PAYROLL_NOTICES="$NOTICES"
export SALVOR_PAYROLL_PAY_DELAY_MS="$PAY_DELAY_MS"

# Salvor's own event lines are the story; the MCP client library's handshake
# chatter is not. Quiet that one target and leave everything else at info, and
# only when the caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-info,rmcp=warn}"

# The human's answer at the gate: the amended roster to pay. The two flagged rows
# are corrected here (E07's 10x bonus typo down to a normal month, E10's
# missing-digits amount back up), and these are the amounts that get paid.
AMENDED_ANSWER='{"structuredContent":{"roster":[
  {"id":"E01","name":"Ada Okonkwo","amount_cents":410000,"pay_period":"2025-11-B"},
  {"id":"E02","name":"Bhavna Rao","amount_cents":455000,"pay_period":"2025-11-B"},
  {"id":"E03","name":"Cyrus Alizadeh","amount_cents":390000,"pay_period":"2025-11-B"},
  {"id":"E04","name":"Deepa Menon","amount_cents":512000,"pay_period":"2025-11-B"},
  {"id":"E05","name":"Emil Novak","amount_cents":448000,"pay_period":"2025-11-B"},
  {"id":"E06","name":"Farida Haddad","amount_cents":476000,"pay_period":"2025-11-B"},
  {"id":"E07","name":"Gustavo Pinto","amount_cents":470000,"pay_period":"2025-11-B"},
  {"id":"E08","name":"Hana Sato","amount_cents":505000,"pay_period":"2025-11-B"},
  {"id":"E09","name":"Ivo Petrov","amount_cents":462000,"pay_period":"2025-11-B"},
  {"id":"E10","name":"June Adeyemi","amount_cents":420000,"pay_period":"2025-11-B"},
  {"id":"E11","name":"Kwame Boateng","amount_cents":433000,"pay_period":"2025-11-B"},
  {"id":"E12","name":"Lena Fischer","amount_cents":498000,"pay_period":"2025-11-B"}
]},"approver":"m.suarez","note":"E07 bonus typo corrected to 470000; E10 missing digits corrected to 420000."}'

MODEL_PID=""
RESUME_PID=""
cleanup() {
  # Only ever by the exact pid this script recorded, never by pattern.
  [[ -n "$RESUME_PID" ]] && kill -9 "$RESUME_PID" 2>/dev/null || true
  [[ -n "$MODEL_PID" ]] && kill "$MODEL_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== starting salvor-demo-model on 127.0.0.1:$MODEL_PORT =="
"$DEMO_MODEL" --port "$MODEL_PORT" --delay-ms "$MODEL_DELAY_MS" \
  --script "$HERE/model-script.json" >"$MODEL_LOG" 2>&1 &
MODEL_PID=$!
export SALVOR_DEMO_BASE_URL="http://127.0.0.1:$MODEL_PORT"
for _ in $(seq 1 50); do
  grep -q listening "$MODEL_LOG" 2>/dev/null && break
  sleep 0.1
done
head -1 "$MODEL_LOG"

echo
echo "############################################"
echo "# 1. Pull the roster, flag exceptions, park at the gate"
echo "############################################"
"$SALVOR" --store "$STORE" graph run "$GRAPH" \
  --input '{"pay_period":"2025-11-B"}' "${AGENTS[@]}" \
  --label desk=payroll >"$SCRATCH/salvor-payroll-leg1.out" 2>&1 || true
cat "$SCRATCH/salvor-payroll-leg1.out"
RUN_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-payroll-leg1.out" | head -1 | awk '{print $2}')
[[ -n "$RUN_ID" ]] || { echo "FAILED: no run id"; exit 1; }

echo
echo "############################################"
echo "# 2. Approve the amended roster, then kill -9 mid-batch"
echo "############################################"
# The gate's answer IS the pay instruction: it flows straight into the `pay_each`
# map as the list to pay, so the amounts the approver sends are the amounts paid.
"$SALVOR" --store "$STORE" resume "$RUN_ID" --graph "$GRAPH" "${AGENTS[@]}" \
  --input "$AMENDED_ANSWER" \
  >"$SCRATCH/salvor-payroll-leg2.out" 2>&1 &
RESUME_PID=$!

# Wait until the ledger holds between 4 and 8 lines: the batch is under way but
# not finished, which is the window the kill has to land in to mean anything.
for _ in $(seq 1 800); do
  lines=$(grep -c . "$LEDGER" 2>/dev/null || echo 0)
  [[ "$lines" -ge 4 ]] && break
  sleep 0.02
done
echo "pay ledger at the instant of the kill:"
cat -n "$LEDGER"
kill -9 "$RESUME_PID"
wait "$RESUME_PID" 2>/dev/null || true
echo "killed pid $RESUME_PID"
RESUME_PID=""

echo
echo "== the recorded log stops part way through the batch =="
"$SALVOR" --store "$STORE" history "$RUN_ID" | tail -6

echo
echo "############################################"
echo "# 3. Recover the crashed run"
echo "############################################"
export SALVOR_PAYROLL_PAY_DELAY_MS=0
"$SALVOR" --store "$STORE" resume "$RUN_ID" --graph "$GRAPH" "${AGENTS[@]}" \
  >"$SCRATCH/salvor-payroll-leg3.out" 2>&1
tail -2 "$SCRATCH/salvor-payroll-leg3.out"
export SALVOR_PAYROLL_PAY_DELAY_MS="$PAY_DELAY_MS"

echo
echo "== pay ledger after the recovery =="
cat -n "$LEDGER"

# --- The proof, assertion by assertion. ---
FAIL=0

LINES=$(grep -c . "$LEDGER" | tr -d ' ')
if [[ "$LINES" == "12" ]]; then
  echo "PROOF: 12 ledger lines, one per employee, across the crash."
else
  echo "FAILED: expected 12 ledger lines, found $LINES"; FAIL=1
fi

UNIQUE=$(grep -oE '"id": "E[0-9]+"' "$LEDGER" | sort -u | wc -l | tr -d ' ')
if [[ "$UNIQUE" == "12" ]]; then
  echo "PROOF: 12 distinct employee ids, so no employee was paid twice and none was skipped."
else
  echo "FAILED: expected 12 distinct employee ids, found $UNIQUE"; FAIL=1
fi

# The amended amounts were paid, not the anomalous ones. E07 was flagged at
# 4500000 and corrected to 470000; E10 at 4200 and corrected to 420000.
if grep -q '"2025-11-B:E07".*470000\|470000.*"2025-11-B:E07"' "$LEDGER" \
   && grep -q '"2025-11-B:E10".*420000\|420000.*"2025-11-B:E10"' "$LEDGER" \
   && ! grep -q '4500000' "$LEDGER" && ! grep -q '"amount_cents": 4200,' "$LEDGER"; then
  echo "PROOF: the amended amounts were paid (E07 470000, E10 420000), never the flagged 4500000 or 4200."
else
  echo "FAILED: the ledger does not carry the amended amounts"; FAIL=1
fi

# Capture the recorded walk once, then read it, so a `grep -q` closing the pipe
# early cannot race the writer into a broken-pipe exit under `set -o pipefail`.
HIST="$("$SALVOR" --store "$STORE" history "$RUN_ID")"
if grep -q 'BranchTaken .* route -> review' <<<"$HIST"; then
  echo "PROOF: the branch recorded BranchTaken route -> review, so the run went through the human gate."
else
  echo "FAILED: no BranchTaken route -> review in the history"; FAIL=1
fi

JOINS=$(grep -c 'MapIterationJoined' <<<"$HIST" || true)
if [[ "$JOINS" == "12" ]]; then
  echo "PROOF: 12 MapIterationJoined events, the whole fan-out visible in the recorded walk."
else
  echo "FAILED: expected 12 MapIterationJoined events, found $JOINS"; FAIL=1
fi

echo
echo "== the summary notice, one per completed run =="
cat "$NOTICES"

echo
echo "############################################"
echo "# 4. The clean arm: a roster with no exceptions routes past the gate"
echo "############################################"
# Same document, same agent, a clean pay period: the branch routes to pay_all and
# the gate never fires, so the batch runs with no human in the loop.
CLEAN_STORE="$SCRATCH/salvor-payroll-clean.db"
rm -f "$CLEAN_STORE" "$CLEAN_STORE-wal" "$CLEAN_STORE-shm"
"$SALVOR" --store "$CLEAN_STORE" graph run "$GRAPH" \
  --input '{"pay_period":"2025-11-A"}' "${AGENTS[@]}" \
  --label desk=payroll >"$SCRATCH/salvor-payroll-leg4.out" 2>&1
CLEAN_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-payroll-leg4.out" | head -1 | awk '{print $2}')
CLEAN_HIST="$("$SALVOR" --store "$CLEAN_STORE" history "$CLEAN_ID")"
grep -E 'BranchTaken|NodeSkipped .* review_exceptions|RunCompleted' <<<"$CLEAN_HIST"
CLEAN_LINES=$(grep -c . "$LEDGER" | tr -d ' ')
if [[ "$CLEAN_LINES" == "24" ]]; then
  echo "PROOF: the clean arm paid its 12, for 24 ledger lines total, with no gate."
else
  echo "FAILED: expected 24 total ledger lines after the clean arm, found $CLEAN_LINES"; FAIL=1
fi

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held; tearing down the model server =="
else
  echo "== FAILURES above; tearing down the model server =="
  exit 1
fi
