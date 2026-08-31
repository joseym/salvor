#!/usr/bin/env bash
# The reconciliation walkthrough, end to end and offline.
#
#   run -> kill mid-write -> resume refuses (dangling write) -> resolve -> resume completes
#
# It starts an offline scripted model, runs the agent, waits until the write has
# landed on disk (the tool then blocks), kills the process in that window, shows
# the reconciliation refusal on resume, resolves the dangling write by hand, and
# resumes to completion. The report file is inspected at each stage to prove the
# write executes exactly once. No API key, no network.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

SALVOR=./target/debug/salvor
STORE=/tmp/salvor-reconciliation.db
REPORT=/tmp/salvor-reconciliation-report.txt
PORT=8891
MODEL_URL="http://127.0.0.1:$PORT"

rm -f "$STORE" "$REPORT" "$STORE"-wal "$STORE"-shm

cleanup() {
  # Only ever by the exact pid this script recorded, never by pattern.
  [[ -n "${SERVER_PID:-}" ]] && kill -9 "$SERVER_PID" 2>/dev/null || true
  [[ -n "${RUN_PID:-}" ]] && kill -9 "$RUN_PID" 2>/dev/null || true
  [[ -n "${MODEL_PID:-}" ]] && kill "$MODEL_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Kill an exact pid and wait out its death, polling `kill -0` rather than the
# `wait` builtin: `wait` only works on a direct child of this shell, and the
# pid this is used for (the MCP tool server) is a grandchild, spawned by the
# salvor process rather than by this script.
kill_and_confirm() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  kill -9 "$pid" 2>/dev/null || true
  for _ in $(seq 1 50); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
}

echo "== Building salvor =="
cargo build -q

echo "== Starting the offline model on port $PORT =="
RECON_MODEL_PORT="$PORT" python3 "$HERE/model_server.py" &
MODEL_PID=$!
for _ in $(seq 1 50); do
  curl -s -o /dev/null "$MODEL_URL/" && break || sleep 0.1
done
export SALVOR_DEMO_BASE_URL="$MODEL_URL"

echo
echo "== Stage 1: run the agent, kill it mid-write =="
"$SALVOR" --store "$STORE" run \
  --agent examples/reconciliation/agent.toml \
  --input @examples/reconciliation/input.json >/tmp/recon-run.out 2>/tmp/recon-run.err &
RUN_PID=$!

RUN_ID=""
for _ in $(seq 1 100); do
  RUN_ID=$(grep -oE 'run [0-9a-f-]{36}' /tmp/recon-run.out 2>/dev/null | head -1 | awk '{print $2}' || true)
  [[ -n "$RUN_ID" ]] && break
  sleep 0.1
done
[[ -n "$RUN_ID" ]] || { echo "FAILED: no run id"; cat /tmp/recon-run.err; exit 1; }
echo "run id: $RUN_ID"

# The agent's one MCP tool server (examples/reconciliation/server.py) is a
# grandchild: the salvor process (RUN_PID) spawns it over stdio, not this
# script, so there is no `$!` for it here. It is already running by the time a
# run id is printed (the tool list is fetched from it before the run starts),
# so its pid is looked up from the OS's own process table by parentage of
# RUN_PID, which names exactly this run's server and nothing else.
SERVER_PID=$(pgrep -P "$RUN_PID" | head -1 || true)

# Wait until the write lands on disk. Write-ahead ordering guarantees the intent
# was recorded before the tool executed, so once the line appears the tool is
# blocking with the completion not yet recorded: the exact dangling-write window.
for _ in $(seq 1 200); do
  [[ -s "$REPORT" ]] && break
  sleep 0.1
done
[[ -s "$REPORT" ]] || { echo "FAILED: write never landed"; cat /tmp/recon-run.err; exit 1; }
echo "the write has landed; killing the process mid-write"
kill -9 "$RUN_PID"; wait "$RUN_PID" 2>/dev/null || true
RUN_PID=""
# The server is blocking mid-tool-call (RECON_BLOCK_SECONDS), so it will not
# notice the closed stdin pipe on its own for a long time; kill it by its own
# pid rather than leaving it to run out the clock.
kill_and_confirm "$SERVER_PID"
SERVER_PID=""

echo
echo "== The recorded log ends at a dangling write intent =="
"$SALVOR" --store "$STORE" history "$RUN_ID"

echo
echo "== Report file after the kill (the write happened once) =="
cat -n "$REPORT"

echo
echo "== Stage 2: resume refuses, needing reconciliation =="
set +e
"$SALVOR" --store "$STORE" resume "$RUN_ID" --agent examples/reconciliation/agent.toml
echo "(resume exit code: $?)"
set -e

echo
echo "== Stage 3: resolve the dangling write by hand =="
"$SALVOR" --store "$STORE" resolve "$RUN_ID" \
  --output '{"content":[{"type":"text","text":"committed report to /tmp/salvor-reconciliation-report.txt"}],"isError":false}'

echo
echo "== Stage 4: resume completes the run =="
"$SALVOR" --store "$STORE" resume "$RUN_ID" --agent examples/reconciliation/agent.toml

echo
echo "== Report file after resolve + resume (still exactly one line) =="
cat -n "$REPORT"
LINES=$(wc -l <"$REPORT" | tr -d ' ')
echo "line count: $LINES"
[[ "$LINES" == "1" ]] && echo "PROOF: the write executed exactly once." || { echo "FAILED: expected 1 line"; exit 1; }
