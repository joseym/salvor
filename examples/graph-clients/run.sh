#!/usr/bin/env bash
# Bring up the offline Salvor stack and run the three client apps.
#
# A stock `salvor serve`, no flags, hosts the dispute-refund graph for the two
# HTTP apps; the Rust app embeds the engine and gets a store path of its own.
# Everything runs offline: a scripted model server stands in for a real
# endpoint, so no API key and no network are needed, and the tools are the
# sibling example's three-tool Python MCP server, reached through the agent
# definitions rather than through any `tool` node. Every side effect lands in a
# scratch ledger this script owns.
#
# Usage, from anywhere:
#     examples/graph-clients/run.sh
set -euo pipefail

# Repository root, two levels up from this script. The agent definitions name
# their MCP server by a path relative to the directory salvor is invoked from,
# and that server lives under `examples/graph-service/`, so everything below
# runs from the root.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. Both
# defaults sit high and adjacent, deliberately far from 8080. That is salvor's
# own default bind, so a default of 8080 here would aim this example's client
# traffic at whatever real control plane is already listening.
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-18961}"
BIND="${SALVOR_EXAMPLE_BIND:-127.0.0.1:18962}"
BASE_URL="http://$BIND"
MODEL_DELAY_MS="${SALVOR_EXAMPLE_MODEL_DELAY_MS:-20}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
STORE="${SALVOR_EXAMPLE_STORE:-$SCRATCH/salvor-graph-clients.db}"
LEDGER="${SALVOR_EXAMPLE_LEDGER:-$SCRATCH/salvor-graph-clients-ledger.txt}"
NOTICES="${SALVOR_EXAMPLE_NOTICES:-$SCRATCH/salvor-graph-clients-notices.json}"
MODEL_LOG="${SALVOR_EXAMPLE_MODEL_LOG:-$SCRATCH/salvor-graph-clients-model.log}"
SERVE_LOG="${SALVOR_EXAMPLE_SERVE_LOG:-$SCRATCH/salvor-graph-clients-serve.log}"

# A checkout's build by default. SALVOR_BIN and SALVOR_DEMO_MODEL_BIN override,
# which is how an installed CLI drives this.
SALVOR="${SALVOR_BIN:-$ROOT/target/debug/salvor}"
DEMO_MODEL="${SALVOR_DEMO_MODEL_BIN:-$ROOT/target/debug/salvor-demo-model}"

for bin in "$SALVOR" "$DEMO_MODEL"; do
  if [[ ! -x "$bin" ]]; then
    echo "missing $bin; build it first with:  cargo build" >&2
    exit 1
  fi
done

GRAPH="examples/graph-clients/dispute-refund.json"
SETTLE_AGENT="examples/graph-clients/agents/settle-and-notify.toml"
CLAIMS_AGENT="examples/graph-clients/agents/small-claims.toml"

# Every run starts from a clean ledger and a clean store, so the line counts the
# apps print mean what they say. The previous store is moved aside, never
# deleted; a run that failed halfway is still worth reading afterwards. The
# ledger is a fresh running record per invocation and the append-only tool never
# parses it, so truncating it in place is fine. Notices are the case that needs
# care: the tool must meet an ABSENT file rather than an EMPTY one, so a
# previous notices file is moved aside the same way the store is.
: >"$LEDGER"
for suffix in "" "-wal" "-shm"; do
  [[ -e "$STORE$suffix" ]] && mv -f "$STORE$suffix" "$STORE$suffix.prev"
done
[[ -e "$NOTICES" ]] && mv -f "$NOTICES" "$NOTICES.prev"

# The MCP server writes here; the agents pass these through to their child
# process by ordinary environment inheritance from `salvor serve`, so no
# checked-in file changes.
export SALVOR_DISPUTES_LEDGER="$LEDGER"
export SALVOR_DISPUTES_NOTICES="$NOTICES"

# Quiet the MCP client library's handshake chatter and leave everything else at
# info. Only when the caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-info,rmcp=warn}"

MODEL_PID=""
SERVE_PID=""
cleanup() {
  # Only ever by the exact pid this script recorded, never by pattern.
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
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
for _ in $(seq 1 100); do
  grep -q listening "$MODEL_LOG" 2>/dev/null && break
  sleep 0.1
done
head -2 "$MODEL_LOG"

echo "== starting salvor serve on $BIND (store $STORE) =="
# No `--demo-tools`: this server's tool registry is EMPTY, which is the point of
# the example. The graph has no `tool` node, so nothing it references needs the
# registry. The side effects live inside agent nodes, whose MCP servers the
# control plane spawns from the registered definitions.
"$SALVOR" --store "$STORE" serve --bind "$BIND" >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!

# Poll a real endpoint until it answers.
READY=""
for _ in $(seq 1 100); do
  if curl -sf "$BASE_URL/v1/agents" >/dev/null 2>&1; then READY=1; break; fi
  sleep 0.1
done
if [[ -z "$READY" ]]; then
  echo "salvor serve never answered on $BASE_URL; last log lines:" >&2
  tail -5 "$SERVE_LOG" >&2
  exit 1
fi
echo "control plane ready on $BASE_URL"

echo
echo "== verifying the agent hashes the document claims =="
# The document names each agent by content hash, never by path, so an edit to a
# TOML silently desyncs it from the graph unless something checks. Recompute
# both hashes and compare them against what the document actually holds, before
# any app tries to register anything.
check_hash() {
  local node="$1" file="$2" claimed actual
  claimed="$(python3 -c '
import json, sys
doc = json.load(open(sys.argv[1]))
for node in doc["nodes"]:
    if node["payload"]["id"] == sys.argv[2]:
        print(node["payload"]["agent_hash"])
        break
else:
    raise SystemExit("no agent node `%s` in %s" % (sys.argv[2], sys.argv[1]))
' "$GRAPH" "$node")"
  actual="$("$SALVOR" agent hash "$file" 2>/dev/null)"
  if [[ "$claimed" != "$actual" ]]; then
    echo "HASH MISMATCH for agent node \`$node\`:" >&2
    echo "  $GRAPH claims: $claimed" >&2
    echo "  $file hashes:  $actual" >&2
    echo "The document and the definition have drifted apart. Update the" >&2
    echo "agent_hash in $GRAPH, or revert the edit to $file." >&2
    exit 1
  fi
  echo "  $node -> $actual (matches $file)"
}
check_hash settle_and_notify "$SETTLE_AGENT"
check_hash small_claims "$CLAIMS_AGENT"

# Each app tells the same story: run the escalated dispute to the gate, answer
# it, then run the small one straight through. Python and TypeScript do it over
# HTTP against the server started above, registering both agents and submitting
# the document first. The Rust app has no server to register with; it takes a
# store path and drives the engine itself. All three write into the one ledger,
# which is why it ends up the running record of every refund any of them made.
#
# A missing app is announced and skipped rather than failing the script.

echo
echo "############################################"
echo "# Python app"
echo "############################################"
PY_APP="$HERE/python/service.py"
if [[ ! -f "$PY_APP" ]]; then
  echo "SKIPPED: $PY_APP is missing."
else
  # DELIBERATE DIVERGENCE from `examples/polyglot-service/`, which installs the
  # PUBLISHED `salvor` package from PyPI on purpose. This example installs THIS
  # CHECKOUT's SDK instead, in editable form, because the graph methods it needs
  # (`submit_graph`, `start_graph_run`, `validate_graph`, `get_graph`,
  # `list_graphs`) are not in any published release yet. Installing 0.7.0 from
  # PyPI would fail on a missing attribute rather than run.
  #
  # REVERT THIS once a release carrying those methods ships: change the editable
  # install back to `pip install --quiet salvor` and drop the check below, so
  # this example demonstrates the published package like its sibling does.
  #
  # A venv beside the example keeps the install out of the caller's environment,
  # the same way the other Python examples do it.
  PYVENV="${SALVOR_EXAMPLE_PYVENV:-$HERE/.venv}"
  if [[ ! -x "$PYVENV/bin/python" ]]; then
    echo "== creating a venv for the Python app =="
    python3 -m venv "$PYVENV"
    "$PYVENV/bin/pip" install --quiet --upgrade pip
  fi
  # Re-installed on every invocation. A venv left over from an earlier version
  # of this script may hold the published package, and the editable install has
  # to win over it.
  echo "== installing this checkout's Python SDK (editable) =="
  "$PYVENV/bin/pip" install --quiet -e "$ROOT/sdks/python"
  if ! "$PYVENV/bin/python" -c 'from salvor import Client; raise SystemExit(0 if hasattr(Client, "start_graph_run") else 1)'; then
    echo "the installed salvor package has no Client.start_graph_run, so the" >&2
    echo "graph methods this app needs are missing. Something other than" >&2
    echo "$ROOT/sdks/python is being imported; inspect $PYVENV and reinstall." >&2
    exit 1
  fi
  "$PYVENV/bin/python" "$PY_APP" "$BASE_URL"
fi

echo
echo "############################################"
echo "# TypeScript app"
echo "############################################"
TS_APP="$HERE/typescript/service.ts"
if [[ ! -f "$TS_APP" ]]; then
  echo "SKIPPED: $TS_APP is missing."
else
  # Node lives under nvm on many machines, which a non-interactive shell does
  # not load. Fall back to the newest nvm install only when node is genuinely
  # absent, so a node the caller chose always wins.
  if ! command -v node >/dev/null 2>&1; then
    NVM_BIN=$(ls -d "$HOME"/.nvm/versions/node/*/bin 2>/dev/null | sort -V | tail -1)
    [[ -n "$NVM_BIN" ]] && export PATH="$NVM_BIN:$PATH"
  fi
  if ! command -v node >/dev/null 2>&1; then
    echo "run.sh: node is required for the TypeScript app; install Node 24 or newer" >&2
    exit 1
  fi
  # DELIBERATE DIVERGENCE from `examples/polyglot-service/`, which installs the
  # PUBLISHED `@salvor-run/client` from npm on purpose. This example imports THIS
  # CHECKOUT's SDK built output by relative path instead, because the graph
  # methods it needs (`submitGraph`, `startGraphRun`, `validateGraph`) are not in
  # any published release yet. Installing 0.7.0 from npm would fail on a missing
  # method rather than run. The app therefore imports
  # `../../../sdks/typescript/dist/index.js` rather than `@salvor-run/client`,
  # which is also why it needs no package.json and no npm install of its own.
  #
  # REVERT THIS once a release carrying those methods ships: give the app a
  # package.json depending on `@salvor-run/client`, restore the bare-specifier
  # import, and drop everything from here to the build check below.
  SDK_TS="$ROOT/sdks/typescript"
  SDK_ENTRY="$SDK_TS/dist/index.js"
  # `tsc` needs the SDK's own devDependencies before it can emit anything.
  if [[ ! -d "$SDK_TS/node_modules" ]]; then
    echo "== installing the TypeScript SDK's build dependencies =="
    (cd "$SDK_TS" && npm install --silent)
  fi
  echo "== building the TypeScript SDK from this checkout =="
  (cd "$SDK_TS" && npm run --silent build)
  # Built output that predates the graph methods is worse than none at all: the
  # import would succeed and the call would fail deep inside the app. So check
  # for the method itself.
  if [[ ! -f "$SDK_ENTRY" ]]; then
    echo "no built SDK at $SDK_ENTRY after 'npm run build' in $SDK_TS." >&2
    echo "Build it by hand and re-run; the app imports that path directly." >&2
    exit 1
  fi
  if ! grep -q startGraphRun "$SDK_TS/dist/client.js" 2>/dev/null; then
    echo "the built SDK at $SDK_TS/dist has no startGraphRun, so it is stale." >&2
    echo "Rebuild it (npm run build in $SDK_TS) and re-run; importing it as it" >&2
    echo "stands would fail inside the app instead of here." >&2
    exit 1
  fi
  node --experimental-strip-types "$TS_APP" "$BASE_URL"
fi

echo
echo "############################################"
echo "# Rust app"
echo "############################################"
RS_APP="$HERE/rust/main.rs"
if [[ ! -f "$RS_APP" ]]; then
  echo "SKIPPED: $RS_APP is missing."
else
  # The Rust app embeds the engine in its own process, so it takes a store path
  # where the other two take a base URL. Nothing it does touches the server
  # started above.
  #
  # It is an `[[example]]` target on `salvor-cli` whose `path` points at
  # `$RS_APP`. That is the pattern this repository already uses; see the
  # `todo_agent` example on `salvor-runtime`. The workspace build then
  # compile-gates it forever.
  #
  # cargo takes the workspace target lock here, so nothing else may be building.
  cargo run --quiet -p salvor-cli --example graph_clients -- \
    "${SALVOR_EXAMPLE_RUST_STORE:-$SCRATCH/salvor-graph-clients-rust.db}"
fi

echo
echo "== the refund ledger, one line per refund any app made =="
cat -n "$LEDGER"
echo
echo "== customer notices =="
cat "$NOTICES"

echo
echo "== done; tearing down the model server and the control plane =="
