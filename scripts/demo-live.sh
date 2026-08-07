#!/usr/bin/env bash
#
# demo-live.sh: bring up a local Salvor control plane against an OFFLINE demo
# model (no API key, no cost) and seed a set of runs, so the v0.3 web dashboard
# can be driven in a browser against it.
#
# It starts two long-lived servers in the background and leaves them running:
#
#   1. salvor-demo-model  (127.0.0.1:8899)  a scripted HTTP model. It serves the
#      same twenty-turn conversation as the demo_run test, so every model call
#      is answered locally with no network and no key.
#   2. salvor serve       (127.0.0.1:8080)  the control plane, over a disposable
#      SQLite store that this script removes first so a re-run re-seeds cleanly.
#
# Then it registers two agents and starts two runs to reach:
#   - one COMPLETED run (the full twenty-step demo agent), and
#   - one BUDGET-EXCEEDED run (an identical agent with a one-step budget), which
#     parks in the waiting-on-human group so the dashboard Inbox is non-empty.
#
# Run it from anywhere; it cds to the repository root itself (the demo agent's
# MCP server path is target-relative, so the control plane must run from there).
#
# Stop the servers afterward with:  pkill -f 'salvor-demo-model'; pkill -f 'salvor .*serve'
#
# Everything is offline. ANTHROPIC_API_KEY is never read.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STORE="/tmp/salvor-demo-store.sqlite"
FINDINGS="/tmp/salvor-demo-findings.txt"
MODEL_PORT=8899
SERVE_ADDR="127.0.0.1:8080"
API="http://${SERVE_ADDR}"

# 1. Build the CLI plus the two fixture binaries (fixture is a default feature).
echo "[demo-live] building salvor-cli (salvor, salvor-demo-model, salvor-demo-research)"
cargo build -p salvor-cli

# 2. Clean disposable state so every bring-up re-seeds from scratch.
rm -f "$STORE" "$STORE"-wal "$STORE"-shm "$FINDINGS"

# 3. Start the offline demo model. A watchable per-turn delay so a freshly
#    started run is visible as in-progress in the dashboard for a moment.
echo "[demo-live] starting salvor-demo-model on 127.0.0.1:${MODEL_PORT}"
target/debug/salvor-demo-model --port "$MODEL_PORT" --delay-ms 300 \
  >/tmp/salvor-demo-model.log 2>&1 &
until curl -s -o /dev/null -X POST "http://127.0.0.1:${MODEL_PORT}/v1/messages" \
  -d '{"messages":[]}' 2>/dev/null; do sleep 0.2; done

# 4. Start the control plane. The demo agent's base_url_env hook routes every
#    model call to the demo model; SALVOR_DEMO_FINDINGS points the write ledger
#    at a temp file; SALVOR_RECORD_PROMPTS=1 records the model request body on
#    each call so the inspector can show the prompt.
echo "[demo-live] starting salvor serve on ${SERVE_ADDR} (store ${STORE})"
SALVOR_DEMO_BASE_URL="http://127.0.0.1:${MODEL_PORT}" \
SALVOR_DEMO_FINDINGS="$FINDINGS" \
SALVOR_RECORD_PROMPTS=1 \
RUST_LOG=warn \
target/debug/salvor --store "$STORE" serve --bind "$SERVE_ADDR" \
  >/tmp/salvor-serve.log 2>&1 &
until curl -s -o /dev/null "${API}/v1/runs" 2>/dev/null; do sleep 0.2; done

# 5. Register the demo agent (drives the completed run).
echo "[demo-live] registering demo agent"
DEMO_AGENT=$(curl -s -X POST "${API}/v1/agents" \
  -H 'Content-Type: application/toml' \
  --data-binary @demo/agent.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')

# 6. Register a one-step-budget variant of the same agent (parks at budget).
echo "[demo-live] registering tiny-budget agent"
sed 's/^steps = 24/steps = 1/' demo/agent.toml > /tmp/salvor-demo-agent-tinybudget.toml
TINY_AGENT=$(curl -s -X POST "${API}/v1/agents" \
  -H 'Content-Type: application/toml' \
  --data-binary @/tmp/salvor-demo-agent-tinybudget.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')

# 7. Start the two runs.
INPUT='{"topic":"durable execution for AI agents"}'
echo "[demo-live] starting completed run"
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${DEMO_AGENT}\",\"input\":${INPUT}}" >/dev/null
echo "[demo-live] starting budget-exceeded run"
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${TINY_AGENT}\",\"input\":${INPUT}}" >/dev/null

# 8. Let them settle and print the folded run states. The completed run drives
#    twenty model calls at the 300ms pace above, so give it enough time to land.
sleep 12
echo "[demo-live] seeded runs:"
curl -s "${API}/v1/runs" | python3 -m json.tool

cat <<EOF

[demo-live] control plane is up at ${API}
[demo-live] serve the dashboard same-origin (no CORS) with:
    cd dashboard && SALVOR_API_BASE="" trunk serve    # http://127.0.0.1:8081
[demo-live] stop the servers with:
    pkill -f 'salvor-demo-model'; pkill -f 'salvor .*serve'
EOF
