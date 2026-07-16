#!/usr/bin/env bash
#
# e2e-serve.sh — bring up the Bridge build for the Playwright suite.
#
# It seeds a real Salvor control plane (mirroring scripts/demo-live.sh: an OFFLINE demo model,
# a disposable SQLite store, two agents, two runs — one completed, one budget-exceeded so the
# waiting group is non-empty), builds the Angular app, and serves the app + the API on ONE origin
# through a static+proxy server (no CORS — the server ships none).
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

APP_PORT="${APP_PORT:-4300}"
MODEL_PORT="${MODEL_PORT:-8899}"
SERVE_ADDR="${SERVE_ADDR:-127.0.0.1:8080}"
API="http://${SERVE_ADDR}"
STORE="/tmp/salvor-bridge-e2e-store.sqlite"
FINDINGS="/tmp/salvor-bridge-e2e-findings.txt"

stop() {
  echo "[e2e-serve] stopping servers"
  pkill -f 'salvor-demo-model' 2>/dev/null || true
  pkill -f 'salvor .*serve' 2>/dev/null || true
  pkill -f 'e2e-serve-proxy.mjs' 2>/dev/null || true
}
if [ "${1:-}" = "--stop" ]; then stop; exit 0; fi

stop   # clean any prior run first
rm -f "$STORE" "$STORE"-wal "$STORE"-shm "$FINDINGS"

# 1. Build the CLI + fixture binaries (fixture is a default feature). Cheap if already built.
echo "[e2e-serve] building salvor-cli"
cargo build -p salvor-cli

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

# 6. Let them settle (the 24-step demo runs drive the offline model at the pace above).
sleep 14
echo "[e2e-serve] seeded runs:"
curl -s "${API}/v1/runs" | python3 -m json.tool | head -40

# 7. Build the app.
echo "[e2e-serve] building the Bridge app"
( cd "$BRIDGE" && npm run build )

# 8. Serve app + API on one origin.
DIST="$BRIDGE/dist/bridge/browser"
echo "[e2e-serve] starting static+proxy server on 127.0.0.1:${APP_PORT}"
node "$BRIDGE/e2e-serve-proxy.mjs" "$APP_PORT" "${SERVE_ADDR%:*}" "${SERVE_ADDR##*:}" "$DIST" \
  >/tmp/salvor-bridge-e2e-proxy.log 2>&1 &
until curl -s -o /dev/null "http://127.0.0.1:${APP_PORT}/" 2>/dev/null; do sleep 0.2; done

cat <<EOF

[e2e-serve] UP.
  App (same-origin app + API):  http://127.0.0.1:${APP_PORT}/
  Control plane (direct):       ${API}

Run the suite from the e2e suite directory:
  TARGET_URL=http://127.0.0.1:${APP_PORT}/ ./run.sh 01-boot.spec.js 05-routes-and-deeplinks.spec.js

Tear down:
  bridge/e2e-serve.sh --stop
EOF
