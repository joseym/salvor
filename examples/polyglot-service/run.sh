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

# Every port and path is overridable, because the defaults collide with a real server: 8080 is
# salvor's own default bind, so anyone already running one cannot execute this example without
# them. Overriding is also how CI runs it on a port nothing else claims.
BIND="${SALVOR_EXAMPLE_BIND:-127.0.0.1:8080}"
BASE_URL="http://$BIND"
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-8893}"
STORE="${SALVOR_EXAMPLE_STORE:-/tmp/salvor-polyglot.db}"
FINDINGS="${SALVOR_EXAMPLE_FINDINGS:-/tmp/salvor-polyglot-findings.txt}"

# Default to a checkout's build, but let an installed CLI serve instead.
SALVOR="${SALVOR_BIN:-$ROOT/target/debug/salvor}"
DEMO_MODEL="${SALVOR_DEMO_MODEL_BIN:-$ROOT/target/debug/salvor-demo-model}"

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
# lives under nvm here, which a non-interactive shell does not load. Fall back to the newest nvm
# install only when node is genuinely absent, so this never overrides a node the caller chose, and
# never depends on one exact version being present.
if ! command -v node >/dev/null 2>&1; then
  NVM_BIN=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
  [[ -n "$NVM_BIN" ]] && export PATH="$NVM_BIN:$PATH"
fi
if ! command -v node >/dev/null 2>&1; then
  echo "run.sh: node is required for the TypeScript half; install Node 24 or newer" >&2
  exit 1
fi

# The TypeScript app depends on the PUBLISHED client, declared in its own package.json, so the
# setup is an ordinary install rather than a build of this repository's SDK sources.
if [[ ! -d "$HERE/typescript/node_modules" ]]; then
  echo "== installing @salvor-run/client for the TypeScript app =="
  (cd "$HERE/typescript" && npm install --silent)
fi

echo
echo "############################################"
echo "# TypeScript app"
echo "############################################"
node --experimental-strip-types "$HERE/typescript/service.ts" "$BASE_URL"

echo
echo "== done; tearing down servers =="
