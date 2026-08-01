#!/usr/bin/env bash
# Bring up the offline stack and run the refund desk.
#
# The point of this example is what `salvor serve` is NOT given. It gets two
# declaration files and nothing else: no tool registry, no MCP server, no
# `--demo-tools`, and no path to `provider.py`. The refunds happen in the
# desk's own process, against a provider script this example owns, under a
# credential that is exported for the desk's command and for nothing else.
#
# Everything is offline: `salvor-demo-model --script` stands in for a real
# endpoint, so no API key and no network are needed.
#
# Usage, from anywhere:
#     examples/client-tools/run.sh
set -euo pipefail

# Repository root, two levels up. The declaration paths below are written
# relative to the root so the printed `salvor serve` command line is one a
# reader can copy.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. The
# defaults sit high and adjacent, deliberately far from 8080: that is salvor's
# own default bind, so a default of 8080 here would aim this example's traffic
# at whatever real control plane is already listening.
MODEL_PORT="${SALVOR_EXAMPLE_MODEL_PORT:-18963}"
BIND="${SALVOR_EXAMPLE_BIND:-127.0.0.1:18964}"
BASE_URL="http://$BIND"
MODEL_DELAY_MS="${SALVOR_EXAMPLE_MODEL_DELAY_MS:-20}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
STORE="${SALVOR_EXAMPLE_STORE:-$SCRATCH/salvor-client-tools.db}"
PROVIDER_LEDGER="${SALVOR_EXAMPLE_PROVIDER_LEDGER:-$SCRATCH/salvor-client-tools-provider.jsonl}"
MODEL_LOG="${SALVOR_EXAMPLE_MODEL_LOG:-$SCRATCH/salvor-client-tools-model.log}"
SERVE_LOG="${SALVOR_EXAMPLE_SERVE_LOG:-$SCRATCH/salvor-client-tools-serve.log}"

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

REFUND_DECL="examples/client-tools/refund-card.toml"
PAYOUT_DECL="examples/client-tools/wire-payout.toml"

# The provider's ledger is a fresh running record per invocation, and the
# provider only ever appends to it, so truncating it in place is right: the
# `created` and `replayed` lines printed at the end then belong to this run.
# The previous store is moved aside rather than removed; a run that failed
# halfway is still worth reading afterwards.
: >"$PROVIDER_LEDGER"
for suffix in "" "-wal" "-shm"; do
  [[ -e "$STORE$suffix" ]] && mv -f "$STORE$suffix" "$STORE$suffix.prev"
done

export SALVOR_EXAMPLE_PROVIDER_LEDGER="$PROVIDER_LEDGER"
export RUST_LOG="${RUST_LOG:-info}"

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
for _ in $(seq 1 100); do
  grep -q listening "$MODEL_LOG" 2>/dev/null && break
  sleep 0.1
done
head -2 "$MODEL_LOG"

echo
echo "== starting salvor serve on $BIND (store $STORE) =="
# Everything this server is given, in full. Two declarations; no tool registry,
# no MCP server, no `--demo-tools`, and nothing that names `provider.py`. The
# declaration format could not name it anyway: it has no command, path, or URL
# field, and an unknown key is a parse error.
SERVE_ARGV=(--store "$STORE" serve --bind "$BIND" --client-tool "$REFUND_DECL" --client-tool "$PAYOUT_DECL")
echo "  salvor ${SERVE_ARGV[*]}"
if printf '%s\n' "${SERVE_ARGV[@]}" | grep -q provider; then
  echo "run.sh: the serve command names the provider script; that is the mistake" >&2
  echo "this example exists to avoid. Refusing to start." >&2
  exit 1
fi
# SALVOR_MODEL_BASE_URL points the client-driven model step's executor at the
# scripted model, so the desk's model calls are offline and keyless.
#
# REFUND_PROVIDER_API_KEY is deliberately NOT exported here. It is set on the
# desk's command line further down, so it exists in the desk's process and in
# the provider subprocess the desk spawns, and never in this server's.
SALVOR_MODEL_BASE_URL="http://127.0.0.1:$MODEL_PORT" \
  "$SALVOR" "${SERVE_ARGV[@]}" >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!

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
echo "  the running process, as the operating system sees it:"
ps -o args= -p "$SERVE_PID" | sed 's/^/    /'

echo
echo "############################################"
echo "# the refund desk"
echo "############################################"
DESK="$HERE/desk.py"
if [[ ! -f "$DESK" ]]; then
  echo "SKIPPED: $DESK is missing."
else
  # DELIBERATE DIVERGENCE from `examples/polyglot-service/`, which installs the
  # PUBLISHED `salvor` package from PyPI on purpose. This example installs THIS
  # CHECKOUT's SDK instead, in editable form, because the client-performed tool
  # methods it needs (`list_client_tools`, `client_tool_intent`,
  # `client_tool_completion`) are not in any published release yet. Installing
  # from PyPI would fail on a missing attribute rather than run.
  #
  # REVERT THIS once a release carrying those methods ships: change the editable
  # install back to `pip install --quiet salvor` and drop the check below.
  PYVENV="${SALVOR_EXAMPLE_PYVENV:-$HERE/.venv}"
  if [[ ! -x "$PYVENV/bin/python" ]]; then
    echo "== creating a venv for the desk =="
    python3 -m venv "$PYVENV"
    "$PYVENV/bin/pip" install --quiet --upgrade pip
  fi
  echo "== installing this checkout's Python SDK (editable) =="
  "$PYVENV/bin/pip" install --quiet -e "$ROOT/sdks/python"
  if ! "$PYVENV/bin/python" -c 'from salvor import Client; raise SystemExit(0 if hasattr(Client, "list_client_tools") else 1)'; then
    echo "the installed salvor package has no Client.list_client_tools, so the" >&2
    echo "client-performed tool methods this desk needs are missing. Something" >&2
    echo "other than $ROOT/sdks/python is being imported; inspect $PYVENV." >&2
    exit 1
  fi
  echo
  # The credential exists for this command and its children. The control plane
  # started above never had it in its environment.
  REFUND_PROVIDER_API_KEY="${REFUND_PROVIDER_API_KEY:-sk_test_northwind_9f2c}" \
    "$PYVENV/bin/python" "$DESK" "$BASE_URL"
fi

echo
echo "== the provider's ledger, one line per call the desk made =="
echo "   (created = money moved; replayed = the same key came back and it did not)"
cat -n "$PROVIDER_LEDGER"

echo
echo "== done; tearing down the model server and the control plane =="
