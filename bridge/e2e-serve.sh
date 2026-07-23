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
# Graph-seeds ADDITION, on top of everything above, unchanged: three real v0.4 graph-engine
# runs over ONE stored document (`research` agent -> `approve` gate -> `followup` agent), agent and
# gate nodes ONLY — the stock server's tool registry is empty and wiring a tool node is a Rust
# change out of scope here. Both agent nodes reference the SAME registered demo agent (DEMO_AGENT,
# step 4), so every agent-node walk genuinely drives demo/agent.toml's 20-turn research loop against
# the offline model and the salvor-demo-research MCP fixture already up from step 2 — no new
# process, no new Rust. One run is driven to completion (parks at the gate, resumed through the
# ordinary /resume endpoint), one is left parked at the gate on purpose, and one is a fork of the
# completed run from the gate boundary. See step 5.6 below, including a note on a discovery made
# while seeding: forking past `approve` is NOT hazard-free with a real MCP-backed agent (the
# `followup` node's own internal `save_finding` writes are real `Effect::Write` tool calls the fork
# engine's flat log scan sees), so that seed exercises the genuine refuse-then-record acknowledgement
# flow rather than a hazard-free fork.
#
# DEFECT A ADDITION, on top of everything above, unchanged: a SECOND stalled seed, the real-world
# shape (an owner's client-run that died mid model-call, not mid-open) — RunStarted, then a
# ModelCallRequested with no completion, folding to `awaiting_model` rather than `running`. See step
# 5.4b below, right after the original `running`-shaped stall (5.4). Both shapes must render as
# stalled; the second is what the derivation's old, narrower rule (`state !== 'running'`) missed.
#
# Demo-tool-registry ADDITION, on top of everything above, unchanged: `salvor serve` is now
# started with `--demo-tools`, so it registers the three deterministic demo tools
# (`salvor_cli::demo_tools`: lookup_invoice read, issue_refund write, send_email idempotent) the
# stock server refuses. That closes the gap noted above: a SECOND graph document, the
# prototype-shaped 8-node invoice-dispute graph (agent, tool, branch, gate, agent, tool, tool,
# agent, real tool nodes this time, not agent/gate only), is submitted and driven for real. One
# run completes the whole shape (lookup -> split over_500 -> approve -> refund -> notify -> close,
# with n_fast genuinely skipped as the road not taken), and one is left parked at the gate. See
# step 5.7 below.
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
# Demo-tools addition: the demo tool registry's own write ledger (issue_refund), separate from FINDINGS
# above (the MCP research fixture's own file) so the two effect-class stories never share a file.
TOOL_LEDGER="/tmp/salvor-bridge-e2e-tool-ledger.txt"
# EXACT-PID TEARDOWN: every background process this script starts records its own $! here, one pid
# per line, and stop() kills ONLY these recorded pids — never a name/argv pattern. A pattern kill
# (the old `pkill -f 'salvor .*serve'`) also matches the OWNER'S real server on :8080, whose argv is
# likewise `salvor … serve …`, and has taken it down as collateral. Exact pids can never match a
# process this script did not start, so :8080 is structurally safe from this teardown.
PIDFILE="${PIDFILE:-/tmp/salvor-bridge-e2e.pids}"
# Inbox addition: the reconciliation walkthrough's own offline model + write server, on ports that
# do not collide with MODEL_PORT/SERVE_ADDR/APP_PORT above. The report path is NOT overridable
# from here: examples/reconciliation/agent.toml declares RECON_REPORT_PATH literally in its own
# `[[mcp_servers]] env` table, which wins over anything exported before invoking `salvor run` (an
# explicit `Command::env` for a key beats whatever the child would otherwise inherit) — so this
# polls the example's own hardcoded path rather than trying to relocate it.
RECON_MODEL_PORT="${RECON_MODEL_PORT:-8892}"
RECON_REPORT="/tmp/salvor-reconciliation-report.txt"

# Record a background process's pid for exact-pid teardown. Called with $! right after every `… &`.
record_pid() {
  echo "$1" >> "$PIDFILE"
}

# Tear down ONLY the pids this script recorded — never a pattern. For each recorded pid: SIGTERM it,
# wait briefly for it to exit, then SIGKILL only that same pid if it is still alive. A pid that is
# already gone (or was never ours) is skipped. Nothing here consults a process name or argv, so it
# can never reach a process this script did not start — the owner's :8080 server included.
stop() {
  echo "[e2e-serve] stopping servers (exact recorded pids only)"
  [ -f "$PIDFILE" ] || return 0
  local pids=()
  while IFS= read -r pid; do
    [ -n "$pid" ] && pids+=("$pid")
  done < "$PIDFILE"
  # First pass: ask each recorded pid to exit.
  for pid in "${pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  # Bounded wait for them to go, polling only these pids.
  for _ in $(seq 1 20); do
    local alive=0
    for pid in "${pids[@]}"; do
      if kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    [ "$alive" -eq 0 ] && break
    sleep 0.1
  done
  # Second pass: SIGKILL only the recorded pids that are still alive.
  for pid in "${pids[@]}"; do
    kill -9 "$pid" 2>/dev/null || true
  done
  rm -f "$PIDFILE"
}
if [ "${1:-}" = "--stop" ]; then stop; exit 0; fi

stop            # clean any prior run first (only its own recorded pids)
: > "$PIDFILE"  # start this run with a fresh, empty pid ledger
rm -f "$STORE" "$STORE"-wal "$STORE"-shm "$FINDINGS" "$RECON_REPORT" "$TOOL_LEDGER"

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
record_pid $!
until curl -s -o /dev/null -X POST "http://127.0.0.1:${MODEL_PORT}/v1/messages" -d '{"messages":[]}' 2>/dev/null; do sleep 0.2; done

# 3. Control plane over the disposable store. `--demo-tools` registers the three
#    deterministic demo tools (salvor_cli::demo_tools) the stock server otherwise refuses, so the
#    invoice graph seed below (step 5.7) can drive real tool nodes.
echo "[e2e-serve] starting salvor serve on ${SERVE_ADDR} (store ${STORE}, --demo-tools)"
# SALVOR_CLIENT_LEASE_TTL_SECS shortens the client-driven-run lease so the seeded STALLED run
# (step 5.4) reports no attached driver within seconds of its last append, rather than the 60s
# default. It affects only client-run leases; no other seed here opens a client-driven run.
SALVOR_DEMO_BASE_URL="http://127.0.0.1:${MODEL_PORT}" \
SALVOR_DEMO_FINDINGS="$FINDINGS" \
SALVOR_DEMO_TOOL_LEDGER="$TOOL_LEDGER" \
SALVOR_RECORD_PROMPTS=1 \
SALVOR_CLIENT_LEASE_TTL_SECS=2 \
RUST_LOG=warn \
target/debug/salvor --store "$STORE" serve --bind "$SERVE_ADDR" --demo-tools \
  >/tmp/salvor-bridge-e2e-serve.log 2>&1 &
record_pid $!
until curl -s -o /dev/null "${API}/v1/runs" 2>/dev/null; do sleep 0.2; done

# 4. Register the demo agent + a one-step-budget variant (parks in the waiting group).
echo "[e2e-serve] registering agents"
DEMO_AGENT=$(curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/toml' \
  --data-binary @demo/agent.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')
sed 's/^steps = 24/steps = 1/' demo/agent.toml > /tmp/salvor-bridge-e2e-tinybudget.toml
TINY_AGENT=$(curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/toml' \
  --data-binary @/tmp/salvor-bridge-e2e-tinybudget.toml | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')

# 4b. Agent-name addition (additive only): register one more agent via JSON with a top-level
#     `name`, so the registry has a resolvable name for a hash to demonstrate the Agent column's
#     registry-resolved-NAME rendering end to end. The Bridge's lookup
#     (bridge/src/app/core/api/agent-registry.ts) deliberately reads `name` only off a JSON-format
#     definition's own top-level key, never a TOML body (to avoid mistaking a [[wasm_tools]]
#     entry's nested `name` for the agent's own identity), so this registers as JSON specifically —
#     a TOML registration would carry the same schema field but stay invisible to the column, by
#     design. No run is started against it: registration alone makes the hash resolvable, which is
#     all the column's lookup (GET /v1/agents/{hash}) needs.
echo "[e2e-serve] registering a named agent (JSON) so the Agent column has a resolvable name to render"
curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/json' \
  -d '{"model":"claude-opus-4-8","name":"demo-writer"}' >/dev/null

# 5. Start the runs — TWO completed + TWO budget-exceeded. The suite's row-channel tests need
#    both a second WAITING row (besides the cold-seeded selection) and a zebra (odd-index)
#    NON-waiting row, which needs two terminal rows below the two waiting ones.
#
#    Labels addition (additive only, same four runs, labels only): the two
#    completed DEMO_AGENT runs share one build_id (bld_e2e_alpha) — a real 2-run, non-degenerate
#    build group, both terminal so it also supplies the zebra (odd-index) non-waiting row item 10's
#    suite already depends on. The first budget-exceeded run gets its OWN build_id (bld_e2e_beta,
#    a singleton build); the second is left unlabeled, falling back to grouping by its own agent
#    identity (never guessed into a build it wasn't tagged with) — plus the reconciliation run
#    below, also genuinely unlabeled. Two distinct build_id values, a multi-run group, and honest
#    unlabelled runs, all from the same four-run seed the row-channel tests already rely on.
INPUT='{"topic":"durable execution for AI agents"}'
echo "[e2e-serve] starting 2 completed + 2 budget-exceeded runs (the two completed runs share a build_id; one budget-exceeded run gets its own; one stays unlabelled)"
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${DEMO_AGENT}\",\"input\":${INPUT},\"labels\":{\"build_id\":\"bld_e2e_alpha\"}}" >/dev/null
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${TINY_AGENT}\",\"input\":${INPUT},\"labels\":{\"build_id\":\"bld_e2e_beta\"}}" >/dev/null
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${DEMO_AGENT}\",\"input\":${INPUT},\"labels\":{\"build_id\":\"bld_e2e_alpha\"}}" >/dev/null
curl -s -X POST "${API}/v1/runs" -H 'Content-Type: application/json' \
  -d "{\"agent\":\"${TINY_AGENT}\",\"input\":${INPUT}}" >/dev/null

# 5.4. LIVENESS addition (additive only): one genuinely STALLED run. A client-driven run is opened,
#      a single RunStarted is appended (so its log folds to `running` — mid-flight and drivable),
#      and then its drive-token lease is left to lapse: nothing drives it again. With
#      SALVOR_CLIENT_LEASE_TTL_SECS=2 (set on serve, step 3), the lease lapses within seconds and is
#      never refreshed, so GET /v1/runs reports this run as `running` with `driver: "none"` — the
#      exact evidence the dashboard folds into a `stalled` verdict (running + driverless + stale).
#      Seeded EARLY, before the slow reconciliation and graph seeds below, so its last event is
#      comfortably older than the client-side stall grace by the time the suite runs.
#
#      Its `agent_def_hash` is a READABLE LABEL (`stalled_demo_v1`), not a `sha256:` hash, so the
#      Agent column renders it as-is and never issues a GET /v1/agents/{hash} — a hash there would
#      404 (this run is registered under no agent) and trip the suite's zero-console-errors gate.
echo "[e2e-serve] seeding a STALLED client-driven run (opened, RunStarted appended, lease left to lapse)"
STALL_OPEN=$(curl -s -X POST "${API}/v1/client-runs" -H 'Content-Type: application/json' -d '{}')
STALL_RUN=$(echo "$STALL_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"])')
STALL_TOKEN=$(echo "$STALL_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["drive_token"])')
python3 - "$STALL_RUN" <<'PYEOF'
import json, sys
run = sys.argv[1]
events = [{
    "run_id": run, "seq": 0, "schema_version": 1, "recorded_at": "1970-01-01T00:00:00Z",
    "event": {"kind": "RunStarted", "payload": {
        "agent_def_hash": "stalled_demo_v1",
        "input": {"topic": "durable execution for AI agents"},
    }},
}]
json.dump({"events": events}, open("/tmp/salvor-bridge-e2e-stall-events.json", "w"))
PYEOF
curl -s -X POST "${API}/v1/client-runs/${STALL_RUN}/events" \
  -H 'Content-Type: application/json' -H "x-drive-token: ${STALL_TOKEN}" \
  --data-binary @/tmp/salvor-bridge-e2e-stall-events.json > /tmp/salvor-bridge-e2e-stall-append.json
STALL_STATE=$(curl -s "${API}/v1/runs/${STALL_RUN}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"]["state"])')
if [ "$STALL_STATE" != "running" ]; then
  echo "[e2e-serve] FATAL: stalled seed did not fold to running (got '${STALL_STATE}'): $(cat /tmp/salvor-bridge-e2e-stall-append.json)" >&2
  exit 1
fi
echo "[e2e-serve] stalled run seeded: ${STALL_RUN} (folds to running; lease lapses in ~2s and is never refreshed)"

# 5.4b. LIVENESS addition, second shape (additive only): the REAL-WORLD stall. The owner's actual
#       abandoned client-runs did not die between steps (the shape step 5.4 seeds) — they died
#       MID MODEL-CALL: a RunStarted, then a ModelCallRequested with no completion, so they fold to
#       `awaiting_model`, never literal `running`. This is exactly the shape defect A's fix widens
#       the stalled derivation to cover (the in-progress family, not just `running`).
#
#       A client-run is opened, RunStarted appended, then one model-step is requested with a
#       request body that cannot match any scripted turn in demo_script (an empty `messages` array —
#       the script's shortest scripted turn carries 1 message). The server's write-ahead rule
#       (client_runs.rs#model_step) durably records the ModelCallRequested intent BEFORE contacting
#       the provider; the provider then answers with its own scripted 500 ("no scripted response"),
#       the executor call fails, and model_step returns an error with the intent already on the log
#       and no completion ever written. No process is killed and nothing is retried, so the intent
#       is genuinely, permanently dangling — the log folds to `awaiting_model` for good. Its lease
#       then lapses exactly as step 5.4's does (same SALVOR_CLIENT_LEASE_TTL_SECS=2).
echo "[e2e-serve] seeding a second STALLED client-driven run, the real-world shape (died mid model-call, folds to awaiting_model)"
STALL2_OPEN=$(curl -s -X POST "${API}/v1/client-runs" -H 'Content-Type: application/json' -d '{}')
STALL2_RUN=$(echo "$STALL2_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"])')
STALL2_TOKEN=$(echo "$STALL2_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["drive_token"])')
python3 - "$STALL2_RUN" <<'PYEOF'
import json, sys
run = sys.argv[1]
events = [{
    "run_id": run, "seq": 0, "schema_version": 1, "recorded_at": "1970-01-01T00:00:00Z",
    "event": {"kind": "RunStarted", "payload": {
        "agent_def_hash": "stalled_awaiting_model_v1",
        "input": {"topic": "durable execution for AI agents"},
    }},
}]
json.dump({"events": events}, open("/tmp/salvor-bridge-e2e-stall2-events.json", "w"))
PYEOF
curl -s -X POST "${API}/v1/client-runs/${STALL2_RUN}/events" \
  -H 'Content-Type: application/json' -H "x-drive-token: ${STALL2_TOKEN}" \
  --data-binary @/tmp/salvor-bridge-e2e-stall2-events.json > /tmp/salvor-bridge-e2e-stall2-append.json
# An unscripted model-step: demo-model has no turn scripted for 0 messages, so it answers 500 and
# the executor call fails — but only AFTER the intent was durably written write-ahead. The step
# itself is expected to error; only the resulting log state matters below.
curl -s -X POST "${API}/v1/client-runs/${STALL2_RUN}/model-step" \
  -H 'Content-Type: application/json' -H "x-drive-token: ${STALL2_TOKEN}" \
  -d '{"seq":1,"request":{"messages":[]}}' > /tmp/salvor-bridge-e2e-stall2-modelstep.json
STALL2_STATE=$(curl -s "${API}/v1/runs/${STALL2_RUN}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"]["state"])')
if [ "$STALL2_STATE" != "awaiting_model" ]; then
  echo "[e2e-serve] FATAL: second stalled seed did not fold to awaiting_model (got '${STALL2_STATE}'): $(cat /tmp/salvor-bridge-e2e-stall2-modelstep.json)" >&2
  exit 1
fi
echo "[e2e-serve] second stalled run seeded: ${STALL2_RUN} (folds to awaiting_model, dangling model intent; lease lapses in ~2s and is never refreshed)"

# 5.4c. ABANDON addition (additive only): one genuinely ABANDONED run, so every abandoned treatment
#       is demonstrable end to end — the muted `abandoned` pill in the Runs ledger, `status:abandoned`
#       in the filter, the run counting as TERMINAL (never attention) in the health strip, and the
#       Inspector's abandoned terminal banner with its recorded reason. It is seeded as a stalled-
#       shaped run (a client-run with a single RunStarted, folding to `running`) that is then RETIRED
#       through the real operator endpoint POST /v1/runs/{id}/abandon — exactly the "we do not care
#       about this run anymore" path the feature exists for. No lease and no drive token: abandon is
#       an operator action over the store, not a driver step. Kept separate from the two live stalled
#       seeds above (5.4/5.4b), which must STAY stalled for the Inbox's stalled-card demos — this one
#       leaves the stalled family the instant it is abandoned, which is the whole point.
echo "[e2e-serve] seeding an ABANDONED run (opened, RunStarted appended, then retired via POST /abandon)"
ABANDON_OPEN=$(curl -s -X POST "${API}/v1/client-runs" -H 'Content-Type: application/json' -d '{}')
ABANDON_RUN=$(echo "$ABANDON_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"])')
ABANDON_TOKEN=$(echo "$ABANDON_OPEN" | python3 -c 'import sys,json;print(json.load(sys.stdin)["drive_token"])')
python3 - "$ABANDON_RUN" <<'PYEOF'
import json, sys
run = sys.argv[1]
events = [{
    "run_id": run, "seq": 0, "schema_version": 1, "recorded_at": "1970-01-01T00:00:00Z",
    "event": {"kind": "RunStarted", "payload": {
        "agent_def_hash": "abandoned_demo_v1",
        "input": {"topic": "durable execution for AI agents"},
    }},
}]
json.dump({"events": events}, open("/tmp/salvor-bridge-e2e-abandon-events.json", "w"))
PYEOF
curl -s -X POST "${API}/v1/client-runs/${ABANDON_RUN}/events" \
  -H 'Content-Type: application/json' -H "x-drive-token: ${ABANDON_TOKEN}" \
  --data-binary @/tmp/salvor-bridge-e2e-abandon-events.json >/dev/null
curl -s -X POST "${API}/v1/runs/${ABANDON_RUN}/abandon" -H 'Content-Type: application/json' \
  -d '{"reason":"husk is dead forever"}' > /tmp/salvor-bridge-e2e-abandon-resp.json
ABANDON_STATE=$(curl -s "${API}/v1/runs/${ABANDON_RUN}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"]["state"])')
if [ "$ABANDON_STATE" != "abandoned" ]; then
  echo "[e2e-serve] FATAL: abandon seed did not fold to abandoned (got '${ABANDON_STATE}'): $(cat /tmp/salvor-bridge-e2e-abandon-resp.json)" >&2
  exit 1
fi
echo "[e2e-serve] abandoned run seeded: ${ABANDON_RUN} (retired via /abandon; folds to abandoned, reason recorded)"

# 5.5. Inbox addition: one needs_reconciliation run, via the CLI directly against $STORE (additive —
#      the four runs above are untouched). Mirrors examples/reconciliation/run.sh's own stages 1-2
#      (run, kill mid-write) without its later resolve/resume stages: this build's Inbox performs
#      those itself, live, in the suite trial and in the browser.
#
#      Labels addition: also register this agent against the SAME serve process
#      over HTTP (idempotent by content hash — `created` comes back false since the CLI run below
#      already built it once into $STORE; this registers it into `salvor serve`'s own in-memory
#      registry too, which is separate and process-local). Without this, the reconciliation run's
#      agent_def_hash is registered nowhere the browser can ask about, so the Runs ledger's agent
#      column (GET /v1/agents/{hash}) gets a genuine 404 for it on every load — and Chromium logs
#      that 404 to the console regardless of how gracefully the app's own JS handles the rejection,
#      which trips this suite's zero-console-errors gate on every test that touches Runs. The run
#      itself is unaffected: it already finished driving via the CLI before this registers.
echo "[e2e-serve] registering the reconciliation agent against the serve API too (so its hash resolves, no 404 noise)"
curl -s -X POST "${API}/v1/agents" -H 'Content-Type: application/toml' \
  --data-binary @examples/reconciliation/agent.toml >/dev/null

echo "[e2e-serve] starting the reconciliation model+write server on 127.0.0.1:${RECON_MODEL_PORT}"
# Captured for the exact-pid inline kill below (step end). NOT recorded in $PIDFILE: it is torn down
# here mid-script by this exact pid, so it is never alive at teardown and its pid must not linger in
# the ledger where a later stop() could reach a recycled pid.
RECON_MODEL_PORT="$RECON_MODEL_PORT" python3 examples/reconciliation/model_server.py \
  >/tmp/salvor-bridge-e2e-recon-model.log 2>&1 &
RECON_MODEL_PID=$!
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
# Tear down the reconciliation model server by its EXACT captured pid, never a pattern.
kill "$RECON_MODEL_PID" 2>/dev/null || true
wait "$RECON_MODEL_PID" 2>/dev/null || true

# 5.6. Gate-graph addition (additive only): three real graph-engine seeds, agent + gate nodes only.
#
#      Document: research (agent, DEMO_AGENT) -> approve (gate) -> followup (agent, DEMO_AGENT).
#      Submitted once via POST /v1/graphs; every seed run below starts a fresh /v1/graph-runs of
#      the SAME stored hash, so GET /v1/graphs lists one document with three runs against it.
#
#      Optional node-name addition (additive only): each node payload also carries its new,
#      optional display `name` ("Research the topic" / "Approve the draft" / "Follow up"), so the
#      served target's canvas and node list render real names instead of falling back to the ids
#      "research" / "approve" / "followup" used above for the topology narrative.
#
#      Seed A (graph-run-completed): started, awaited to the gate, resumed with {"approved":true}
#      through the ORDINARY /v1/runs/{id}/resume endpoint (no graph-specific verb), awaited to
#      completion.
#      Seed B (graph-run-parked): started, awaited to the gate, left suspended on purpose — a
#      second, independent parked run.
#      Seed C (graph-run-forked): forks seed A from the "approve" gate boundary. A dry run first
#      discovers the exact hazard seqs (found empirically while writing this seed: forking past
#      "approve" is NOT hazard-free here, because "followup" is the SAME demo agent and its own
#      internal save_finding MCP calls are real Effect::Write tool calls sitting in the origin's
#      log — the fork engine's hazard scan is a flat per-run log scan with no node-kind filter, so
#      it sees them same as it would a graph tool node's write), then the real fork acknowledges
#      exactly those seqs. The child re-enters "approve" fresh (a fork's prefix ends BELOW the fork
#      node's NodeEntered, so the origin's Resumed event is excluded) and is left parked there,
#      proving forked_from without paying for a second full MCP walk to re-drive it to completion.
echo "[e2e-serve] submitting the gate graph document (research -agent-> approve -gate-> followup -agent->)"
cat > /tmp/salvor-bridge-e2e-graph-build.py <<'PYEOF'
import json, sys
agent_hash = sys.argv[1]
doc = {
    "schema_version": 1,
    "nodes": [
        {"kind": "agent", "payload": {
            "id": "research", "agent_hash": agent_hash, "name": "Research the topic",
        }},
        {"kind": "gate", "payload": {
            "id": "approve",
            "name": "Approve the draft",
            "prompt": "Approve this research pass before the follow-up round?",
            "approval_schema": {
                "type": "object",
                "properties": {"approved": {"type": "boolean"}},
                "required": ["approved"],
            },
        }},
        {"kind": "agent", "payload": {
            "id": "followup", "agent_hash": agent_hash, "name": "Follow up",
        }},
    ],
    "edges": [
        {"from": "research", "to": "approve"},
        {"from": "approve", "to": "followup"},
    ],
}
with open("/tmp/salvor-bridge-e2e-graph.json", "w") as f:
    json.dump(doc, f)
PYEOF
python3 /tmp/salvor-bridge-e2e-graph-build.py "$DEMO_AGENT"
curl -s -X POST "${API}/v1/graphs" -H 'Content-Type: application/json' \
  --data-binary @/tmp/salvor-bridge-e2e-graph.json > /tmp/salvor-bridge-e2e-graph-submit.json
GRAPH_HASH=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-graph-submit.json"))["graph"])')
if [ -z "$GRAPH_HASH" ]; then
  echo "[e2e-serve] FATAL: graph submit failed: $(cat /tmp/salvor-bridge-e2e-graph-submit.json)" >&2
  exit 1
fi
echo "[e2e-serve] graph stored: ${GRAPH_HASH}"

# Polls GET /v1/runs/{run} until status.state == $2, or fails loudly after $3 tries (0.5s each).
wait_for_run_state() {
  local run="$1" want="$2" tries="${3:-160}" state=""
  for _ in $(seq 1 "$tries"); do
    state=$(curl -s "${API}/v1/runs/${run}" | python3 -c 'import sys,json
try:
    print(json.load(sys.stdin)["status"]["state"])
except Exception:
    print("")')
    if [ "$state" = "$want" ]; then return 0; fi
    sleep 0.5
  done
  echo "[e2e-serve] FATAL: run ${run} never reached state '${want}' (last saw '${state}')" >&2
  curl -s "${API}/v1/runs/${run}" >&2 || true
  exit 1
}

echo "[e2e-serve] seed A: graph run driven to completion"
curl -s -X POST "${API}/v1/graph-runs" -H 'Content-Type: application/json' \
  -d "{\"graph_hash\":\"${GRAPH_HASH}\",\"input\":${INPUT}}" \
  > /tmp/salvor-bridge-e2e-gruna.json
GRUN_A=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-gruna.json"))["run"])')
wait_for_run_state "$GRUN_A" "suspended"
curl -s -X POST "${API}/v1/runs/${GRUN_A}/resume" -H 'Content-Type: application/json' \
  -d '{"input":{"approved":true}}' >/dev/null
wait_for_run_state "$GRUN_A" "completed"
echo "[e2e-serve] seed A completed: ${GRUN_A}"

echo "[e2e-serve] seed B: a second graph run, parked at the gate on purpose"
curl -s -X POST "${API}/v1/graph-runs" -H 'Content-Type: application/json' \
  -d "{\"graph_hash\":\"${GRAPH_HASH}\",\"input\":${INPUT}}" \
  > /tmp/salvor-bridge-e2e-grunb.json
GRUN_B=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-grunb.json"))["run"])')
wait_for_run_state "$GRUN_B" "suspended"
echo "[e2e-serve] seed B parked: ${GRUN_B}"

echo "[e2e-serve] seed C: forking seed A from the 'approve' boundary"
curl -s -X POST "${API}/v1/runs/${GRUN_A}/fork" -H 'Content-Type: application/json' \
  -d '{"from_node":"approve","dry_run":true}' \
  > /tmp/salvor-bridge-e2e-fork-preview.json
python3 - <<'PYEOF'
import json
with open("/tmp/salvor-bridge-e2e-fork-preview.json") as f:
    preview = json.load(f)
writes = preview.get("unacknowledged_writes", [])
with open("/tmp/salvor-bridge-e2e-fork-body.json", "w") as f:
    json.dump({"from_node": "approve", "acknowledge_writes": writes}, f)
print(f"[e2e-serve] fork preview: {len(writes)} write hazard(s) to acknowledge: {writes}")
PYEOF
curl -s -X POST "${API}/v1/runs/${GRUN_A}/fork" -H 'Content-Type: application/json' \
  --data-binary @/tmp/salvor-bridge-e2e-fork-body.json \
  > /tmp/salvor-bridge-e2e-fork-resp.json
GRUN_C=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-fork-resp.json")).get("run",""))')
if [ -z "$GRUN_C" ]; then
  echo "[e2e-serve] FATAL: fork failed: $(cat /tmp/salvor-bridge-e2e-fork-resp.json)" >&2
  exit 1
fi
wait_for_run_state "$GRUN_C" "suspended"
echo "[e2e-serve] seed C forked child parked: ${GRUN_C} (forked from ${GRUN_A} at approve)"

echo "[e2e-serve] verifying the gate graph seeds"
curl -s "${API}/v1/graphs" > /tmp/salvor-bridge-e2e-graphs-list.json
python3 - "$GRAPH_HASH" <<'PYEOF'
import json, sys
h = sys.argv[1]
with open("/tmp/salvor-bridge-e2e-graphs-list.json") as f:
    d = json.load(f)
found = next((g for g in d["graphs"] if g["graph"] == h), None)
assert found, f"graph {h} not listed in GET /v1/graphs: {d}"
assert found["node_count"] == 3, found
assert found["edge_count"] == 2, found
assert found["entry_nodes"] == ["research"], found
assert found["terminal_nodes"] == ["followup"], found
print("[e2e-serve] GET /v1/graphs lists the gate document:", found)
PYEOF

for pair in "${GRUN_A}:completed" "${GRUN_B}:suspended" "${GRUN_C}:suspended"; do
  run="${pair%%:*}"
  want="${pair##*:}"
  curl -s "${API}/v1/runs/${run}" > /tmp/salvor-bridge-e2e-run-check.json
  got=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-run-check.json"))["status"]["state"])')
  if [ "$got" != "$want" ]; then
    echo "[e2e-serve] FATAL: run ${run} expected state '${want}', got '${got}'" >&2
    exit 1
  fi
done
echo "[e2e-serve] run states verified: A(completed)=${GRUN_A} B(suspended)=${GRUN_B} C(suspended,forked)=${GRUN_C}"

curl -s "${API}/v1/runs/${GRUN_A}/forks" > /tmp/salvor-bridge-e2e-forks.json
python3 - "$GRUN_C" <<'PYEOF'
import json, sys
child = sys.argv[1]
with open("/tmp/salvor-bridge-e2e-forks.json") as f:
    d = json.load(f)
assert d.get("derived") is True, f"forks index not labeled derived: {d}"
found = next((fork for fork in d["forks"] if fork["run"] == child), None)
assert found, f"child {child} missing from the forks index: {d}"
assert found["from_node"] == "approve", found
print("[e2e-serve] GET /v1/runs/{origin}/forks lists the child:", found)
PYEOF

curl -s "${API}/v1/runs/${GRUN_C}/graph" > /tmp/salvor-bridge-e2e-child-graph.json
python3 - "$GRUN_A" <<'PYEOF'
import json, sys
origin = sys.argv[1]
with open("/tmp/salvor-bridge-e2e-child-graph.json") as f:
    d = json.load(f)
fo = d.get("forked_from")
assert fo, f"child's GET /v1/runs/{{id}}/graph has no forked_from: {d}"
assert fo["run_id"] == origin, fo
assert fo["from_node"] == "approve", fo
print("[e2e-serve] child's GET /v1/runs/{id}/graph shows forked_from:", fo)
PYEOF
echo "[e2e-serve] gate graph seeds verified: graph=${GRAPH_HASH} A=${GRUN_A} B=${GRUN_B} C=${GRUN_C}"

# 5.7. Invoice-graph addition (additive only): a SECOND graph document, the prototype-shaped 8-node
#      invoice-dispute graph, this time with real `tool` nodes dispatched through the server's
#      `--demo-tools` registry (step 3): lookup_invoice (read), issue_refund (write), send_email
#      (idempotent), see crates/salvor-cli/src/demo_tools.rs. Same node ids and names as the
#      prototype's own invoice-dispute fixture graph, so a spec that keys on them reads the same
#      story on both targets; the one shape divergence is `n_notify`, a plain `tool` node here
#      rather than the prototype's inline `map` fan-out (the real graph format has no inline map
#      body, see salvor_graph::document::MapBody, and the build does not render map iterations
#      yet either, so a single-recipient tool call tells the identical idempotent-retry story with
#      nothing lost).
#
#      Two entry nodes (n_lookup, a tool; n_triage, an agent) so BOTH a tool-node walk and an
#      agent-node walk start the graph directly off the run's own input, with no fragile
#      agent-prose-into-typed-tool-input edge anywhere in the document: n_split routes on
#      n_lookup's structured output, n_fast (agent) is the untaken arm on this seed (genuinely
#      SKIPPED, never called), and n_approve's gate is resumed with a body this script controls
#      completely, carrying every field n_refund and n_notify need downstream.
#
#      Seed D (graph-run-completed): started over invoice inv_2002 (amount $512, so the branch
#      takes over_500), awaited to the gate, resumed with the full record (approval plus the
#      invoice/watcher fields n_refund and n_notify consume), awaited to completion, the whole
#      8-node shape walks: n_lookup, n_split, n_approve, n_refund, n_notify, n_close, and n_triage
#      all EXITED, n_fast SKIPPED.
#      Seed E (graph-run-parked): a second run over the same invoice, left suspended at the gate on
#      purpose, same shape as the gate graph's seed B, but this time the parked origin is a tool-bearing
#      graph run, not agent+gate only.
echo "[e2e-serve] submitting the invoice graph document (8-node invoice-dispute shape, real tool nodes)"
cat > /tmp/salvor-bridge-e2e-graph9-build.py <<'PYEOF'
import json, sys
agent_hash = sys.argv[1]
doc = {
    "schema_version": 1,
    "nodes": [
        {"kind": "tool", "payload": {
            "id": "n_lookup", "name": "Look up the invoice", "tool": "lookup_invoice",
        }},
        {"kind": "agent", "payload": {
            "id": "n_triage", "name": "Classify the dispute", "agent_hash": agent_hash,
        }},
        {"kind": "branch", "payload": {
            "id": "n_split", "name": "Refund threshold",
            "cases": [
                {"name": "under_500", "when": {"kind": "expression", "value": "amount_usd < 500"}},
                {"name": "over_500", "when": {"kind": "expression", "value": "amount_usd >= 500"}},
            ],
        }},
        {"kind": "agent", "payload": {
            "id": "n_fast", "name": "Fast-track the refund", "agent_hash": agent_hash,
        }},
        {"kind": "gate", "payload": {
            "id": "n_approve", "name": "Approve the refund",
            "prompt": "This refund exceeds $500. Approve?",
            "approval_schema": {
                "type": "object",
                "required": ["approved"],
                "properties": {"approved": {"type": "boolean"}},
            },
        }},
        {"kind": "tool", "payload": {
            "id": "n_refund", "name": "Issue the refund", "tool": "issue_refund",
        }},
        {"kind": "tool", "payload": {
            "id": "n_notify", "name": "Notify the watcher", "tool": "send_email",
        }},
        {"kind": "agent", "payload": {
            "id": "n_close", "name": "Draft the closing note", "agent_hash": agent_hash,
        }},
    ],
    "edges": [
        {"from": "n_lookup", "to": "n_split"},
        {"from": "n_split", "to": "n_fast", "label": "under_500"},
        {"from": "n_split", "to": "n_approve", "label": "over_500"},
        {"from": "n_approve", "to": "n_refund"},
        {"from": "n_refund", "to": "n_notify"},
        {"from": "n_notify", "to": "n_close"},
    ],
}
with open("/tmp/salvor-bridge-e2e-graph9.json", "w") as f:
    json.dump(doc, f)
PYEOF
python3 /tmp/salvor-bridge-e2e-graph9-build.py "$DEMO_AGENT"
curl -s -X POST "${API}/v1/graphs" -H 'Content-Type: application/json' \
  --data-binary @/tmp/salvor-bridge-e2e-graph9.json > /tmp/salvor-bridge-e2e-graph9-submit.json
GRAPH9_HASH=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-graph9-submit.json"))["graph"])')
if [ -z "$GRAPH9_HASH" ]; then
  echo "[e2e-serve] FATAL: invoice graph submit failed: $(cat /tmp/salvor-bridge-e2e-graph9-submit.json)" >&2
  exit 1
fi
echo "[e2e-serve] invoice graph stored: ${GRAPH9_HASH}"

INVOICE_INPUT='{"invoice_id":"inv_2002"}'
RESUME_BODY='{"input":{"approved":true,"invoice_id":"inv_2002","amount_usd":512.0,"watcher":"carol@example.com"}}'

echo "[e2e-serve] seed D: the 8-node graph, driven to completion"
curl -s -X POST "${API}/v1/graph-runs" -H 'Content-Type: application/json' \
  -d "{\"graph_hash\":\"${GRAPH9_HASH}\",\"input\":${INVOICE_INPUT}}" \
  > /tmp/salvor-bridge-e2e-grund.json
GRUN_D=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-grund.json"))["run"])')
wait_for_run_state "$GRUN_D" "suspended"
curl -s -X POST "${API}/v1/runs/${GRUN_D}/resume" -H 'Content-Type: application/json' \
  --data-binary "$RESUME_BODY" >/dev/null
wait_for_run_state "$GRUN_D" "completed" 400
echo "[e2e-serve] seed D completed: ${GRUN_D}"

echo "[e2e-serve] seed E: a second 8-node graph run, parked at the gate on purpose"
curl -s -X POST "${API}/v1/graph-runs" -H 'Content-Type: application/json' \
  -d "{\"graph_hash\":\"${GRAPH9_HASH}\",\"input\":${INVOICE_INPUT}}" \
  > /tmp/salvor-bridge-e2e-grune.json
GRUN_E=$(python3 -c 'import json;print(json.load(open("/tmp/salvor-bridge-e2e-grune.json"))["run"])')
wait_for_run_state "$GRUN_E" "suspended"
echo "[e2e-serve] seed E parked: ${GRUN_E}"

echo "[e2e-serve] verifying the invoice graph seeds"
curl -s "${API}/v1/runs/${GRUN_D}/graph" > /tmp/salvor-bridge-e2e-grund-graph.json
python3 - <<'PYEOF'
import json
with open("/tmp/salvor-bridge-e2e-grund-graph.json") as f:
    d = json.load(f)
nodes = {n["node"]: n for n in d["nodes"]}
assert len(nodes) == 8, f"expected 8 nodes in the projection, got {len(nodes)}: {nodes.keys()}"
exited = {"n_lookup", "n_triage", "n_split", "n_approve", "n_refund", "n_notify", "n_close"}
for node_id in exited:
    assert nodes[node_id]["state"] == "exited", f"{node_id}: {nodes[node_id]}"
assert nodes["n_fast"]["state"] == "skipped", nodes["n_fast"]
assert nodes["n_split"].get("branch_case") == "over_500", nodes["n_split"]
print("[e2e-serve] seed D's 8-node projection verified: 7 exited, n_fast skipped, over_500 taken")
PYEOF
ledger_lines=$(wc -l < "$TOOL_LEDGER" | tr -d ' ')
if [ "$ledger_lines" != "1" ]; then
  echo "[e2e-serve] FATAL: expected exactly 1 durable issue_refund line, found ${ledger_lines}: $(cat "$TOOL_LEDGER")" >&2
  exit 1
fi
echo "[e2e-serve] invoice graph seeds verified: graph=${GRAPH9_HASH} D=${GRUN_D} E=${GRUN_E} ledger=1 line"

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

Tear down (kills ONLY the exact pids this run recorded in ${PIDFILE} — never a name pattern,
so the owner's :8080 server is never matched):
  bridge/e2e-serve.sh --stop
EOF
