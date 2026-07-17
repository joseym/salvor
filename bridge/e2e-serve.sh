#!/usr/bin/env bash
#
# e2e-serve.sh — bring up the Bridge build for the Playwright suite.
#
# It seeds a real Salvor control plane (mirroring scripts/demo-live.sh: an OFFLINE demo model,
# a disposable SQLite store, two agents, two runs — one completed, one budget-exceeded so the
# waiting group is non-empty), builds the Angular app, and serves the app + the API on ONE origin
# from `salvor serve` itself: the ui-enabled server embeds the dashboard and answers both the
# static app and /v1/* on the same port, so there is no separate proxy and no CORS to configure.
# The debug binary reads the freshly built dist/ from the filesystem, so the app is built before
# the server is used.
#
# Inbox ADDITION, on top of the above, unchanged: one genuine `needs_reconciliation` run,
# seeded via the repo's own offline reconciliation walkthrough (examples/reconciliation/) — run,
# kill mid-write, leave the dangling ToolCallRequested behind. This is additive only: it runs the
# `salvor` CLI directly against the SAME store `salvor serve` already has open (a fold reads
# straight off the log, so no HTTP agent registration is needed for the run to appear or for
# /resolve to work on it — only /resume would need the agent registered, and the Inbox's
# reconciliation card never calls resume). The four runs above are untouched; this only adds a
# fifth. See step 5.5 below.
#
# Usage:
#     bridge/e2e-serve.sh                 # start everything, print TARGET_URL, stay up
#     bridge/e2e-serve.sh --stop          # tear down the servers this script started
#
# Then, from the e2e suite directory:
#     TARGET_URL=http://127.0.0.1:4300/ ./run.sh 01-boot.spec.js 05-routes-and-deeplinks.spec.js
#
# Everything is offline. ANTHROPIC_API_KEY is never read.

set -euo pipefail

BRIDGE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$BRIDGE/.." && pwd)"
cd "$ROOT"

# APP_PORT is the one origin the app and the API share, now that salvor serve
# hosts both. SERVE_ADDR is kept as an override alias so existing callers still
# work; when unset it defaults to APP_PORT.
APP_PORT="${APP_PORT:-4300}"
MODEL_PORT="${MODEL_PORT:-8899}"
SERVE_ADDR="${SERVE_ADDR:-127.0.0.1:${APP_PORT}}"
API="http://${SERVE_ADDR}"
STORE="/tmp/salvor-bridge-e2e-store.sqlite"
FINDINGS="/tmp/salvor-bridge-e2e-findings.txt"
# Inbox addition: the reconciliation walkthrough's own offline model + write server, on ports that
# do not collide with MODEL_PORT/SERVE_ADDR/APP_PORT above. The report path is NOT overridable
# from here: examples/reconciliation/agent.toml declares RECON_REPORT_PATH literally in its own
# `[[mcp_servers]] env` table, which wins over anything exported before invoking `salvor run` (an
# explicit `Command::env` for a key beats whatever the child would otherwise inherit) — so this
# polls the example's own hardcoded path rather than trying to relocate it.
RECON_MODEL_PORT="${RECON_MODEL_PORT:-8892}"
RECON_REPORT="/tmp/salvor-reconciliation-report.txt"

stop() {
  echo "[e2e-serve] stopping servers"
  pkill -f 'salvor-demo-model' 2>/dev/null || true
  pkill -f 'salvor .*serve' 2>/dev/null || true
  pkill -f 'examples/reconciliation/model_server.py' 2>/dev/null || true
  pkill -f 'examples/reconciliation/server.py' 2>/dev/null || true
  pkill -f 'examples/reconciliation/agent.toml' 2>/dev/null || true
}
if [ "${1:-}" = "--stop" ]; then stop; exit 0; fi

stop   # clean any prior run first
rm -f "$STORE" "$STORE"-wal "$STORE"-shm "$FINDINGS" "$RECON_REPORT"

# 1. Build the CLI + fixture binaries (fixture is a default feature). Cheap if already built.
echo "[e2e-serve] building salvor-cli"
cargo build -p salvor-cli

# 1b. Build the app. The ui-enabled debug server reads dist/ from the filesystem at request time,
#     so the dashboard must be built before the server is used. Building it up front also fails
#     fast on a token-gate or compile error, before any seeding work.
echo "[e2e-serve] building the Bridge app"
( cd "$BRIDGE" && npm run build )

# 2. Offline demo model.
echo "[e2e-serve] starting salvor-demo-model on 127.0.0.1:${MODEL_PORT}"
target/debug/salvor-demo-model --port "$MODEL_PORT" --delay-ms 120 \
  >/tmp/salvor-bridge-e2e-model.log 2>&1 &
until curl -s -o /dev/null -X POST "http://127.0.0.1:${MODEL_PORT}/v1/messages" -d '{"messages":[]}' 2>/dev/null; do sleep 0.2; done

# 3. Control plane over the disposable store.
echo "[e2e-serve] starting salvor serve on ${SERVE_ADDR} (store ${STORE})"
SALVOR_DEMO_BASE_URL="http://127.0.0.1:${MODEL_PORT}" \
SALVOR_DEMO_FINDINGS="$FINDINGS" \
SALVOR_RECORD_PROMPTS=1 \
RUST_LOG=warn \
target/debug/salvor --store "$STORE" serve --bind "$SERVE_ADDR" \
  >/tmp/salvor-bridge-e2e-serve.log 2>&1 &
until curl -s -o /dev/null "${API}/v1/runs" 2>/dev/null; do sleep 0.2; done

# 4. Register the demo agent + a one-step-budget variant (parks in the waiting group).
echo "[e2e-serve] registering agents"
DEMO_AGENT=$(curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/toml' \
  --data-binary @demo/agent.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')
sed 's/^steps = 24/steps = 1/' demo/agent.toml > /tmp/salvor-bridge-e2e-tinybudget.toml
TINY_AGENT=$(curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/toml' \
  --data-binary @/tmp/salvor-bridge-e2e-tinybudget.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')

# 5. Start the runs — TWO completed + TWO budget-exceeded. The suite's row-channel tests need
#    both a second WAITING row (besides the cold-seeded selection) and a zebra (odd-index)
#    NON-waiting row, which needs two terminal rows below the two waiting ones.
INPUT='{"topic":"durable execution for AI agents"}'
echo "[e2e-serve] starting 2 completed + 2 budget-exceeded runs"
for _ in 1 2; do
  curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
    -d "{\"agent\":\"${DEMO_AGENT}\",\"input\":${INPUT}}" >/dev/null
  curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
    -d "{\"agent\":\"${TINY_AGENT}\",\"input\":${INPUT}}" >/dev/null
done

# 5.5. Inbox addition: one needs_reconciliation run, via the CLI directly against $STORE (additive —
#      the four runs above are untouched). Mirrors examples/reconciliation/run.sh's own stages 1-2
#      (run, kill mid-write) without its later resolve/resume stages: this build's Inbox performs
#      those itself, live, in the suite trial and in the browser.
echo "[e2e-serve] starting the reconciliation model+write server on 127.0.0.1:${RECON_MODEL_PORT}"
RECON_MODEL_PORT="$RECON_MODEL_PORT" python3 examples/reconciliation/model_server.py \
  >/tmp/salvor-bridge-e2e-recon-model.log 2>&1 &
until curl -s -o /dev/null "http://127.0.0.1:${RECON_MODEL_PORT}/" 2>/dev/null; do sleep 0.2; done

echo "[e2e-serve] starting the reconciliation run, timed to strand a dangling write"
SALVOR_DEMO_BASE_URL="http://127.0.0.1:${RECON_MODEL_PORT}" \
target/debug/salvor --store "$STORE" run \
  --agent examples/reconciliation/agent.toml \
  --input @examples/reconciliation/input.json \
  >/tmp/salvor-bridge-e2e-recon-run.out 2>/tmp/salvor-bridge-e2e-recon-run.err &
RECON_RUN_PID=$!

RECON_RUN_ID=""
for _ in $(seq 1 100); do
  RECON_RUN_ID=$(grep -oE 'run [0-9a-f-]{36}' /tmp/salvor-bridge-e2e-recon-run.out 2>/dev/null | head -1 | awk '{print $2}' || true)
  [ -n "$RECON_RUN_ID" ] && break
  sleep 0.1
done
if [ -z "$RECON_RUN_ID" ]; then
  echo "[e2e-serve] WARNING: the reconciliation run never printed its id — no needs_reconciliation seed this run" >&2
  cat /tmp/salvor-bridge-e2e-recon-run.err >&2 || true
else
  # write-ahead ordering: once the report line is on disk, the intent is durably recorded and the
  # tool is blocking with no completion yet — the exact dangling-write window (see the example's
  # own README for the full argument).
  for _ in $(seq 1 200); do
    [ -s "$RECON_REPORT" ] && break
    sleep 0.1
  done
  if [ -s "$RECON_REPORT" ]; then
    echo "[e2e-serve] the write has landed for run ${RECON_RUN_ID}; killing mid-write"
    kill -9 "$RECON_RUN_PID" 2>/dev/null || true
    wait "$RECON_RUN_PID" 2>/dev/null || true
  else
    echo "[e2e-serve] WARNING: the reconciliation write never landed — no needs_reconciliation seed this run" >&2
    cat /tmp/salvor-bridge-e2e-recon-run.err >&2 || true
  fi
fi
pkill -f 'examples/reconciliation/model_server.py' 2>/dev/null || true

# 6. Let them settle (the 24-step demo runs drive the offline model at the pace above).
sleep 14
echo "[e2e-serve] seeded runs:"
curl -s "${API}/v1/runs" | python3 -m json.tool | head -60

# 7. Nothing more to start: salvor serve (step 3) is already answering both the app and the API
#    on ${SERVE_ADDR}. The app was built in step 1b, so the server has a dashboard to serve.

cat <<EOF

[e2e-serve] UP.
  App + API (one origin, salvor serve):  ${API}/

Run the suite from the e2e suite directory:
  TARGET_URL=${API}/ ./run.sh 01-boot.spec.js 05-routes-and-deeplinks.spec.js

Tear down:
  bridge/e2e-serve.sh --stop
EOF
