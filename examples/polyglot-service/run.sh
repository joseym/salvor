#!/usr/bin/env bash
# Bring up the offline Salvor stack and run both language apps against it.
#
# One durable Rust process (`salvor serve`) backs two thin clients. Everything
# runs offline: the scripted demo model stands in for a real endpoint, so no
# API key is needed. This script starts the model server and the control plane,
# runs the Python app and then the TypeScript app (each registers an agent,
# starts a run, streams it, handles a budget park by resuming, and streams to
# completion), then tears the servers down.
#
# Usage, from anywhere:
#     examples/polyglot-service/run.sh
set -euo pipefail

# Repository root, two levels up from this script.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

BIND="127.0.0.1:8080"
BASE_URL="http://$BIND"
MODEL_PORT="8893"
STORE="/tmp/salvor-polyglot.db"
FINDINGS="/tmp/salvor-polyglot-findings.txt"

SALVOR="$ROOT/target/debug/salvor"
DEMO_MODEL="$ROOT/target/debug/salvor-demo-model"

for bin in "$SALVOR" "$DEMO_MODEL" "$ROOT/target/debug/salvor-demo-research"; do
  if [[ ! -x "$bin" ]]; then
    echo "missing $bin; build it first with:  cargo build" >&2
    exit 1
  fi
done

# The demo research MCP server writes findings here; use a scratch file and a
# fresh store so each run starts clean.
export SALVOR_DEMO_FINDINGS="$FINDINGS"
rm -f "$STORE" "$FINDINGS"

MODEL_PID=""
SERVE_PID=""
cleanup() {
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
  [[ -n "$MODEL_PID" ]] && kill "$MODEL_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "== starting salvor-demo-model on 127.0.0.1:$MODEL_PORT =="
"$DEMO_MODEL" --port "$MODEL_PORT" --delay-ms 50 >/tmp/salvor-polyglot-model.log 2>&1 &
MODEL_PID=$!

echo "== starting salvor serve on $BIND (store $STORE) =="
SALVOR_DEMO_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$SALVOR" --store "$STORE" serve --bind "$BIND" \
  >/tmp/salvor-polyglot-serve.log 2>&1 &
SERVE_PID=$!

# Wait for the control plane to answer.
for _ in $(seq 1 50); do
  if curl -sf "$BASE_URL/v1/agents" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

echo
echo "############################################"
echo "# Python app"
echo "############################################"
PYTHON="$ROOT/sdks/python/.venv/bin/python"
[[ -x "$PYTHON" ]] || PYTHON="python3"
PYTHONPATH="$ROOT/sdks/python" "$PYTHON" "$HERE/python/service.py" "$BASE_URL"

# The TypeScript app imports the SDK's built output; build it if needed. Node
# lives under nvm here; add it to PATH so this runs under a plain bash.
export PATH="$HOME/.nvm/versions/node/v24.18.0/bin:$PATH"
if [[ ! -f "$ROOT/sdks/typescript/dist/index.js" ]]; then
  echo "== building @salvor/client =="
  (cd "$ROOT/sdks/typescript" && npm install --silent && npm run build --silent)
fi

echo
echo "############################################"
echo "# TypeScript app"
echo "############################################"
node --experimental-strip-types "$HERE/typescript/service.ts" "$BASE_URL"

echo
echo "== done; tearing down servers =="
