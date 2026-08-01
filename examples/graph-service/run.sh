#!/usr/bin/env bash
# Drive the dispute-refund graph end to end, offline, including a crash.
#
#   run -> park at the human gate -> approve -> kill -9 after the refund lands
#       -> recover -> complete, with the refund still issued exactly once
#
# Then the other arm of the same graph: a small dispute that settles with no
# human in the loop at all. A scripted model server stands in for a real
# endpoint, so nothing here needs an API key or a network. Every side effect
# lands in a scratch ledger this script owns.
#
# Usage, from anywhere:
#     examples/graph-service/run.sh
set -euo pipefail

# Repository root, two levels up from this script. The agent definitions name
# their MCP server by a path relative to the directory salvor is invoked from,
# so everything below runs from the root.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. The
# model port default is deliberately far up the range: this script must never
# collide with a control plane, a dev server, or anything else already holding a
# conventional port. Nothing here binds a control-plane port at all, because
# `salvor graph run` drives the store directly rather than over HTTP.
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-18942}"
# Long enough that the kill below reliably lands inside a model call, which is
# the recoverable window (the alternative, killing mid-write, is a dangling
# write intent, and `examples/reconciliation/` already walks through that one).
MODEL_DELAY_MS="${SALVOR_EXAMPLE_MODEL_DELAY_MS:-2000}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
STORE="${SALVOR_EXAMPLE_STORE:-$SCRATCH/salvor-graph-service.db}"
LEDGER="${SALVOR_EXAMPLE_LEDGER:-$SCRATCH/salvor-graph-service-ledger.txt}"
NOTICES="${SALVOR_EXAMPLE_NOTICES:-$SCRATCH/salvor-graph-service-notices.json}"
MODEL_LOG="${SALVOR_EXAMPLE_MODEL_LOG:-$SCRATCH/salvor-graph-service-model.log}"

# Default to a checkout's build, but let an installed CLI drive instead.
SALVOR="${SALVOR_BIN:-$ROOT/target/debug/salvor}"
DEMO_MODEL="${SALVOR_DEMO_MODEL_BIN:-$ROOT/target/debug/salvor-demo-model}"

for bin in "$SALVOR" "$DEMO_MODEL"; do
  if [[ ! -x "$bin" ]]; then
    echo "missing $bin; build it first with:  cargo build" >&2
    exit 1
  fi
done

GRAPH="examples/graph-service/dispute-refund.json"
NOTICE_AGENT="examples/graph-service/agents/customer-notice.toml"
CLAIMS_AGENT="examples/graph-service/agents/small-claims.toml"
AGENTS=(--agent "$NOTICE_AGENT" --agent "$CLAIMS_AGENT")

# Every run starts from a clean store and a clean ledger, so the line counts
# below mean what they say.
rm -f "$STORE" "$STORE-wal" "$STORE-shm" "$LEDGER" "$NOTICES"

# The MCP server writes here; the agents pass these through to their child
# process by ordinary environment inheritance, so no checked-in file changes.
export SALVOR_DISPUTES_LEDGER="$LEDGER"
export SALVOR_DISPUTES_NOTICES="$NOTICES"

# Salvor's own event lines are the story; the MCP client library's handshake
# chatter is not. Quiet that one target and leave everything else at info, and
# only when the caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-info,rmcp=warn}"

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
# The scripted model answers by name, so a request whose system prompt matches
# no conversation is a loud 500 rather than a wrong answer.
export SALVOR_DEMO_BASE_URL="http://127.0.0.1:$MODEL_PORT"
for _ in $(seq 1 50); do
  grep -q listening "$MODEL_LOG" 2>/dev/null && break
  sleep 0.1
done
head -2 "$MODEL_LOG"

echo
echo "############################################"
echo "# 1. The escalated arm: DSP-4471, \$512.00"
echo "############################################"
"$SALVOR" --store "$STORE" graph run "$GRAPH" \
  --input @examples/graph-service/input.json "${AGENTS[@]}" \
  --label desk=disputes >"$SCRATCH/salvor-graph-service-leg1.out" 2>&1 || true
cat "$SCRATCH/salvor-graph-service-leg1.out"
RUN_ID=$(grep -oE 'run [0-9a-f-]{36}' "$SCRATCH/salvor-graph-service-leg1.out" | head -1 | awk '{print $2}')
[[ -n "$RUN_ID" ]] || { echo "FAILED: no run id"; exit 1; }

echo
echo "############################################"
echo "# 2. Approve the refund, then kill -9 the process"
echo "############################################"
# The gate's answer IS the refund instruction: it flows straight into the
# `settle` tool node as its input, so what the approver said is what gets paid.
"$SALVOR" --store "$STORE" resume "$RUN_ID" --graph "$GRAPH" "${AGENTS[@]}" \
  --input '{"dispute_id":"DSP-4471","amount_usd":512.0,"approver":"j.okafor","note":"Duplicate charge confirmed against INV-90210."}' \
  >"$SCRATCH/salvor-graph-service-leg2.out" 2>&1 &
RESUME_PID=$!

# Wait until the log shows a model call in flight. Everything before it is
# already durably recorded, including the refund's completion, so the kill lands
# in the recoverable window rather than mid-write.
for _ in $(seq 1 400); do
  "$SALVOR" --store "$STORE" history "$RUN_ID" 2>/dev/null \
    | grep -q ModelCallRequested && break
  sleep 0.05
done
echo "refund ledger at the instant of the kill:"
cat -n "$LEDGER"
kill -9 "$RESUME_PID"
wait "$RESUME_PID" 2>/dev/null || true
echo "killed pid $RESUME_PID"
RESUME_PID=""

echo
echo "== the recorded log stops at an unanswered model call =="
"$SALVOR" --store "$STORE" history "$RUN_ID" | tail -8

echo
echo "############################################"
echo "# 3. Recover the crashed run"
echo "############################################"
"$SALVOR" --store "$STORE" resume "$RUN_ID" --graph "$GRAPH" "${AGENTS[@]}"

echo
echo "== refund ledger after the recovery =="
cat -n "$LEDGER"
LINES=$(grep -c . "$LEDGER" | tr -d ' ')
if [[ "$LINES" == "1" ]]; then
  echo "PROOF: the refund executed exactly once across the crash."
else
  echo "FAILED: expected 1 ledger line, found $LINES"
  exit 1
fi

echo
echo "== the full recorded walk =="
"$SALVOR" --store "$STORE" history "$RUN_ID"

echo
echo "############################################"
echo "# 4. The auto-settle arm: DSP-3312, \$47.25"
echo "############################################"
# Same document, same agents, a smaller amount: the branch routes past the gate
# entirely and one agent settles the dispute with no human in the loop.
"$SALVOR" --store "$STORE" graph run "$GRAPH" \
  --input @examples/graph-service/input-small.json "${AGENTS[@]}" \
  --label desk=disputes

echo
echo "== both refunds, one per real execution =="
cat -n "$LEDGER"
echo
echo "== customer notices =="
cat "$NOTICES"

echo
echo "== done; tearing down the model server =="
