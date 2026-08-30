#!/usr/bin/env bash
# Drive the same LangChain support desk twice, once in TypeScript and once in
# Python, and prove eight things about what salvor recorded each time.
#
#   1. a first invoke records the desk's model calls and its one refund
#   2. a second invoke replays: no model call, no tool body, every marker `replayed`
#   3. a crash inside `refund_order` costs one refund, not two
#   4. a second copy of the desk on a held thread is refused before it runs anything
#   5. a new question down an old thread forks, once, and says where
#   6. a refund the operator will not let the desk close stops for a person, and
#      a resolution with the wrong amount is refused before the right one lands
#   7. a finished thread takes no more invokes
#   8. the same refund asked for twice in one turn settles from the first call
#
# Nothing here needs an API key or a network. The model is a scripted stand-in
# inside each app that reads the conversation so far, so every model call is
# free and every answer is the same on every machine. That is also why the
# proofs below can count model calls at all: under a real provider the apps
# print `unavailable` rather than a number.
#
# Usage, from anywhere:
#     bash examples/langchain/run.sh
#
# It exits 0 only if all sixteen proofs hold. Every check that does not hold
# prints a `FAILED: expected ...` line naming what it wanted and what it found,
# so a run that stopped early can never be mistaken for one that passed.
set -euo pipefail

# Repository root, two levels up from this script. The declaration paths below
# are written relative to the root so the `salvor serve` command line this
# prints is one a reader can copy.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

# Ports and paths are overridable so this runs on a busy machine and in CI. The
# defaults sit high and adjacent, deliberately far from 8080: that is salvor's
# own default bind, so a default of 8080 here would aim this example's traffic
# at whatever real control plane is already listening.
TS_PORT="${SALVOR_EXAMPLE_TS_PORT:-18401}"
PY_PORT="${SALVOR_EXAMPLE_PY_PORT:-18402}"
SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
mkdir -p "$SCRATCH"

# How long a lease survives a driver that says nothing. The server's default is
# 60 seconds, which is right in production and wrong for proof 3: a desk that
# was killed mid-refund leaves its lease behind, and this script would sit out a
# full minute of it. Eight seconds keeps the same shape (the retry loop below is
# what a real worker does) at a tenth of the wall time.
LEASE_TTL="${SALVOR_EXAMPLE_LEASE_TTL:-8}"

# A checkout's build by default. SALVOR_BIN overrides it outright, which is how
# an already-installed CLI drives this instead.
SALVOR="${SALVOR_BIN:-$ROOT/target/debug/salvor}"
if [[ ! -x "$SALVOR" ]]; then
  echo "missing the \`salvor\` binary at $SALVOR." >&2
  echo "Build it with:  cargo build" >&2
  echo "Or point SALVOR_BIN at one you already have." >&2
  exit 1
fi

NODE="${SALVOR_EXAMPLE_NODE:-node}"
NPM="${SALVOR_EXAMPLE_NPM:-npm}"

# The scripted model, always, whatever the shell that started this already had
# set. Both apps pick their scripted stand-in only when there is no key, and the
# proofs below assert exact model-call counts, which an app cannot report for a
# call a provider made on its behalf: under a key it prints `unavailable (real
# provider)` and those proofs could not be checked at all. Swapping a real model
# in is a thing to do by hand, one invoke at a time, not inside a proof script.
if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ANTHROPIC_API_KEY is set; unsetting it for this script, which proves things about counted model calls."
fi
unset ANTHROPIC_API_KEY

DECLS=(
  --client-tool examples/langchain/tools/lookup-order.toml
  --client-tool examples/langchain/tools/refund-order.toml
  --client-tool examples/langchain/tools/refund-large.toml
)

# The six questions, and the orders behind them. Each proof gets its own order
# so a ledger line count is that proof's own answer.
ASK_7781="Refund ORD-7781, the item arrived damaged."
ASK_8120="Refund ORD-8120, a duplicate charge on one renewal."
ASK_3050="Refund ORD-3050, the customer never received it."
ASK_9002="How is ORD-9002 doing?"
ASK_4400="Refund ORD-4400, the card expired before the refund window."
# The one ticket that names its own amount and says the refund is on it twice.
# The scripted model reads that as two calls in one turn, which is what proof 8
# needs; there is nothing to look up, so the only tool body in that invoke is
# the refund itself.
ASK_5150="Refund ORD-5150 for 3300 cents. The ticket lists it twice."

FAIL=0
SERVE_PID=""
BG_PID=""

# Only ever by the exact pids this script recorded, never by pattern.
cleanup() {
  [[ -n "$BG_PID" ]] && kill "$BG_PID" 2>/dev/null || true
  [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
}
trap cleanup EXIT

# --- Saying what failed, every time ---
#
# A bare `grep` or `[[ ... ]]` under `set -euo pipefail` exits 1 having printed
# NOTHING, and a proof script that can die silently is worse than one that
# proves less: a reader cannot tell a run that checked everything from one that
# stopped before it got there. So every check goes through `fail`, `die` or
# `want`, and each says what was expected and what was actually there.

# fail <what was expected> [<what was actually there>]
fail() {
  echo "FAILED: expected $1"
  if [[ -n "${2:-}" ]]; then
    printf '%s\n' "$2" | sed 's/^/       actual: /'
  fi
  FAIL=1
}

# die <what was expected> [<what was actually there>]: a failure nothing after
# it can be read past, so the script stops here rather than cascading.
die() {
  fail "$@"
  exit 1
}

# want <actual> <expected> <what was expected>: one condition of a proof, with
# the PROOF line printed by the caller once every condition has held.
want() {
  if [[ "$1" != "$2" ]]; then
    fail "$3" "$1"
    return 1
  fi
}

# The value of a `KEY: value` line the desk printed, or the empty string.
field() {
  sed -n "s/^$2: //p" "$1" | head -1
}

# The number of non-empty lines in a ledger, as one bare number. A ledger that
# was never written is zero lines, not an error.
count_lines() {
  local count
  count=$(grep -c . "$1" 2>/dev/null) || count=0
  printf '%s' "$count"
}

# The number of lines of <1> naming <2>, as one bare number.
count_matching() {
  local count
  count=$(grep -c -F -- "$2" "$1" 2>/dev/null) || count=0
  printf '%s' "$count"
}

# What a run's recorded log says about one tool, read out of the durable log
# itself rather than out of anything the desk printed:
#
#     intents=1 completions=1 dedup=0 distinct_keys=1 keys=sha256:07513bd5...
#
# `intents` counts the ToolCallRequested events naming the tool, `completions`
# counts the ToolCallCompleted events sitting on those intents, `dedup` counts
# how many of those completions salvor copied from an earlier call rather than
# the desk reporting them, `distinct_keys` is how many different idempotency
# keys the intents carry, and `keys` lists them.
tool_facts() { # <store> <run> <tool>
  local json="$SCRATCH/salvor-langchain-history.json"
  "$SALVOR" --store "$1" history "$2" --json >"$json"
  python3 -c '
import json, sys

events = json.load(open(sys.argv[1]))
tool = sys.argv[2]
intents = {}
for envelope in events:
    event = envelope["event"]
    payload = event.get("payload", {})
    if event["kind"] == "ToolCallRequested" and payload.get("tool") == tool:
        intents[payload["seq"]] = payload.get("idempotency_key", "")
completions = [
    envelope["event"].get("payload", {})
    for envelope in events
    if envelope["event"]["kind"] == "ToolCallCompleted"
    and envelope["event"].get("payload", {}).get("seq") in intents
]
dedup = sum(1 for payload in completions if payload.get("deduplicated_from"))
distinct = sorted(set(intents.values()))
print(
    "intents=%d completions=%d dedup=%d distinct_keys=%d keys=%s"
    % (len(intents), len(completions), dedup, len(distinct), ",".join(distinct) or "none")
)
' "$json" "$3"
}

# --- the desk, run once ---
#
# DESK_LANG, BASE and STORE are set by the per-language block below; everything from
# here down is written once and runs twice.

# desk <output file> <flags...>: run the desk in the current language, echo what
# it printed, and leave its exit status in APP_EXIT.
desk() {
  local out="$1"
  shift
  APP_EXIT=0
  if [[ "$DESK_LANG" == typescript ]]; then
    SALVOR_EXAMPLE_SCRATCH="$SCRATCH" "$NODE" "$HERE/app.ts" \
      --server "$BASE" "$@" >"$out" 2>&1 || APP_EXIT=$?
  else
    SALVOR_EXAMPLE_SCRATCH="$SCRATCH" "$PYTHON" "$HERE/app.py" \
      --server "$BASE" "$@" >"$out" 2>&1 || APP_EXIT=$?
  fi
  sed 's/^/  /' "$out"
}

# desk_bg <output file> <flags...>: the same, in the background, with its pid in
# BG_PID. Used once, to hold a thread while a second copy tries to take it.
desk_bg() {
  local out="$1"
  shift
  if [[ "$DESK_LANG" == typescript ]]; then
    SALVOR_EXAMPLE_SCRATCH="$SCRATCH" "$NODE" "$HERE/app.ts" \
      --server "$BASE" "$@" >"$out" 2>&1 &
  else
    SALVOR_EXAMPLE_SCRATCH="$SCRATCH" "$PYTHON" "$HERE/app.py" \
      --server "$BASE" "$@" >"$out" 2>&1 &
  fi
  BG_PID=$!
}

# desk_when_free <output file> <flags...>: invoke, and if the thread is still
# held by a driver that died without releasing, wait for that hold to lapse and
# try again. This is the `lease_held` handler both SDK READMEs write out: poll
# rather than sleep out the whole window, because a live driver usually finishes
# well before its hold does.
desk_when_free() {
  local out="$1"
  shift
  local waited=0
  while :; do
    desk "$out" "$@"
    if [[ "$APP_EXIT" != "3" ]] || ! grep -q 'REFUSED lease_held' "$out"; then
      return 0
    fi
    if (( waited >= 40 )); then
      return 0
    fi
    echo "  == the dead driver's lease has not lapsed yet; waiting 2s =="
    sleep 2
    waited=$((waited + 2))
  done
}

run_language() { # <typescript|python> <port> <ledger prefix>
  DESK_LANG="$1"
  local port="$2"
  local prefix="$3"
  BASE="http://127.0.0.1:$port"
  STORE="$SCRATCH/salvor-langchain-$prefix.db"
  local out="$SCRATCH/salvor-langchain-$prefix"
  local lookups="$SCRATCH/salvor-langchain-$prefix-lookups.jsonl"
  local refunds="$SCRATCH/salvor-langchain-$prefix-refunds.jsonl"
  local large="$SCRATCH/salvor-langchain-$prefix-large-refunds.jsonl"

  echo
  echo "############################################################"
  echo "# $DESK_LANG"
  echo "############################################################"

  # A clean store and clean ledgers, so every count below means what it says.
  # Only files this script owns, by exact path, are removed.
  rm -f "$STORE" "$STORE-wal" "$STORE-shm" "$lookups" "$refunds" "$large" \
     "$SCRATCH/salvor-langchain-history.json" "$out"-*.out "$out"-*.json

  echo "== starting salvor serve on 127.0.0.1:$port (store $STORE) =="
  echo "  salvor --store $STORE serve --bind 127.0.0.1:$port ${DECLS[*]}"
  echo "  (SALVOR_CLIENT_LEASE_TTL_SECS=$LEASE_TTL, so proof 3 does not sit out the 60s default)"
  SALVOR_CLIENT_LEASE_TTL_SECS="$LEASE_TTL" \
    "$SALVOR" --store "$STORE" serve --bind "127.0.0.1:$port" "${DECLS[@]}" \
    >"$out-serve.log" 2>&1 &
  SERVE_PID=$!
  local ready=""
  for _ in $(seq 1 100); do
    if curl -sf "$BASE/v1/client-tools" >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.1
  done
  if [[ -z "$ready" ]]; then
    echo "salvor serve never answered on $BASE; last log lines:" >&2
    tail -5 "$out-serve.log" >&2
    exit 1
  fi
  echo "  three declarations loaded; the desk's code is not among them"

  # -- 1. the first invoke --------------------------------------------------
  echo
  echo "-- 1. the first invoke: the desk answers, and salvor holds the receipt --"
  desk "$out-1.out" --thread orders-7781 --ask "$ASK_7781"
  local run_7781
  run_7781=$(field "$out-1.out" RUN)
  if want "$APP_EXIT" "0" "the first invoke to exit 0" \
     && want "$(field "$out-1.out" 'MODEL CALLS')" "3" "3 model calls on the first invoke" \
     && want "$(field "$out-1.out" 'TOOL BODIES')" "2" "2 tool bodies on the first invoke" \
     && want "$(field "$out-1.out" MARKERS)" "live@1,live@5,live@9" \
             "every answer marked live, at seqs 1, 5 and 9" \
     && want "$(count_lines "$refunds")" "1" "1 line in the refunds ledger" \
     && want "$(tool_facts "$STORE" "$run_7781" refund_order | sed 's/ keys=.*//')" \
             "intents=1 completions=1 dedup=0 distinct_keys=1" \
             "one refund intent and one completion in the log"; then
    echo "PROOF: $DESK_LANG, first invoke: 3 model calls, 2 tool bodies, one line in the refunds ledger, and a log holding one refund intent with one completion on it."
  fi

  # -- 2. the second invoke -------------------------------------------------
  echo
  echo "-- 2. the same thread again: nothing is called, everything is read back --"
  desk "$out-2.out" --thread orders-7781 --ask "$ASK_7781"
  if want "$APP_EXIT" "0" "the replay to exit 0" \
     && want "$(field "$out-2.out" 'MODEL CALLS')" "0" "0 model calls on the replay" \
     && want "$(field "$out-2.out" 'TOOL BODIES')" "0" "0 tool bodies on the replay" \
     && want "$(field "$out-2.out" MARKERS)" "replayed@1,replayed@5,replayed@9" \
             "every answer marked replayed" \
     && want "$(field "$out-2.out" ANSWER)" "$(field "$out-1.out" ANSWER)" \
             "the same final answer as the first invoke" \
     && want "$(count_lines "$refunds")" "1" "the refunds ledger still at 1 line" \
     && want "$(count_lines "$lookups")" "1" "the lookups ledger still at 1 line"; then
    echo "PROOF: $DESK_LANG, replay: 0 model calls, 0 tool bodies, every marker \`replayed\`, the same answer, and neither ledger grew."
  fi

  # -- 3. the crash ---------------------------------------------------------
  echo
  echo "-- 3. killed between the refund and the record: one refund, not two --"
  desk "$out-3a.out" --thread orders-8120 --ask "$ASK_8120" --crash-in refund_order
  local run_8120
  run_8120=$(field "$out-3a.out" RUN)
  if [[ "$APP_EXIT" != "9" ]]; then
    fail "the desk to die with exit 9 inside refund_order" "it exited $APP_EXIT"
  fi
  echo "  == the ledger says the money moved; the log ends at the intent =="
  echo "  == invoking again, which is all a worker picking this job up does =="
  desk_when_free "$out-3b.out" --thread orders-8120 --ask "$ASK_8120"
  if want "$APP_EXIT" "0" "the invoke after the crash to exit 0" \
     && want "$(field "$out-3b.out" 'TOOL BODIES')" "1" \
             "exactly the interrupted tool body to run again" \
     && want "$(count_matching "$refunds" ORD-8120)" "1" \
             "1 line for ORD-8120 in the refunds ledger, across both attempts" \
     && want "$(tool_facts "$STORE" "$run_8120" refund_order | sed 's/ keys=.*//')" \
             "intents=1 completions=1 dedup=0 distinct_keys=1" \
             "one refund intent and one completion in the log after the crash"; then
    echo "PROOF: $DESK_LANG, crash inside refund_order: the body ran twice under one key, the ledger holds 1 line for ORD-8120, and the log holds one intent with one completion. $(tool_facts "$STORE" "$run_8120" refund_order)"
  fi

  # -- 4. the lease ---------------------------------------------------------
  echo
  echo "-- 4. a second copy of the desk, while the first is inside a tool --"
  desk_bg "$out-4a.out" --thread orders-3050 --ask "$ASK_3050" --slow-tool lookup_order=5
  local held=""
  for _ in $(seq 1 100); do
    if grep -q 'SLOW TOOL' "$out-4a.out" 2>/dev/null; then held=1; break; fi
    sleep 0.1
  done
  if [[ -z "$held" ]]; then
    die "the first copy to reach its slow tool body" "$(cat "$out-4a.out" 2>/dev/null)"
  fi
  echo "  == the first copy is inside lookup_order; starting a second on the same thread =="
  desk "$out-4b.out" --thread orders-3050 --ask "$ASK_3050"
  local rival_ok=0
  if want "$APP_EXIT" "3" "the second copy to be refused and exit 3" \
     && want "$(field "$out-4b.out" 'MODEL CALLS')" "0" "the second copy to call no model" \
     && want "$(field "$out-4b.out" 'TOOL BODIES')" "0" "the second copy to run no tool body" \
     && want "$(count_matching "$out-4b.out" "REFUSED lease_held")" "1" \
             "the refusal to name lease_held"; then
    rival_ok=1
  fi
  local first_exit=0
  wait "$BG_PID" || first_exit=$?
  BG_PID=""
  echo "  == and what the first copy printed, now that it has finished =="
  sed 's/^/  /' "$out-4a.out"
  if (( rival_ok == 1 )) \
     && want "$first_exit" "0" "the first copy to finish its invoke" \
     && want "$(field "$out-4a.out" 'TOOL BODIES')" "2" "the first copy to run both its tools"; then
    echo "PROOF: $DESK_LANG, one driver per thread: the second copy was refused \`lease_held\` (lapsing in $(field "$out-4b.out" 'LAPSES IN')s) before it called a model or ran a tool, and the first copy finished."
  fi

  # -- 5. the fork ----------------------------------------------------------
  echo
  echo "-- 5. a new question down an old thread --"
  desk "$out-5.out" --thread orders-7781 --ask "$ASK_9002"
  if want "$APP_EXIT" "0" "the forking invoke to exit 0" \
     && want "$(field "$out-5.out" FORKS)" "1" "the fork callback to be told exactly once" \
     && want "$(field "$out-5.out" MARKERS)" "forked@1,forked@1" \
             "every message from the fork onward to carry the fork marker" \
     && want "$(count_matching "$out-5.out" "FORK at seq 1")" "1" \
             "one printed fork notice, naming the seq it forked at"; then
    echo "PROOF: $DESK_LANG, a genuinely new turn forks rather than replaying: one fork notice at seq 1, and every message marked \`forked\`."
  fi

  # -- 6. the refund a person has to confirm --------------------------------
  echo
  echo "-- 6. a refund too large for the desk to close on its own say-so --"
  desk "$out-6a.out" --thread orders-4400 --ask "$ASK_4400"
  local stopped
  stopped=$(field "$out-6a.out" 'NEEDS RESOLUTION')
  if [[ "$APP_EXIT" != "4" || -z "$stopped" ]]; then
    die "the invoke to stop with ToolNeedsResolution (exit 4)" "exit $APP_EXIT; $(cat "$out-6a.out")"
  fi
  local run_4400 refund_output
  run_4400=$(printf '%s' "$stopped" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run"])')
  refund_output=$(printf '%s' "$stopped" | python3 -c 'import json,sys; print(json.dumps({"output": json.load(sys.stdin)["output"]}))')
  # The resolver is held to the declaration too. `refund-large.toml` names
  # `amount_cents` in require_equal, so an amount that is not the one the intent
  # recorded is refused and nothing is written: a typo cannot become the run's
  # own history. A tenth of the real amount is the typo a dropped zero makes.
  echo "  == first, a resolution with a dropped zero in the amount =="
  local typo_output typo_status
  typo_output=$(printf '%s' "$stopped" | python3 -c '
import json, sys

stop = json.load(sys.stdin)
output = dict(stop["output"])
output["amount_cents"] = int(output["amount_cents"]) // 10
print(json.dumps({"output": output}))')
  echo "  curl -X POST $BASE/v1/runs/$run_4400/resolve -d '$typo_output'"
  typo_status=$(curl -sS -o "$out-6-typo.json" -w '%{http_code}' \
    -X POST "$BASE/v1/runs/$run_4400/resolve" \
    -H 'content-type: application/json' -d "$typo_output") \
    || die "the mistyped resolve to reach the server" "curl exited nonzero"
  echo "  $typo_status $(cat "$out-6-typo.json")"

  echo "  == now the amount the intent actually recorded =="
  echo "  curl -X POST $BASE/v1/runs/$run_4400/resolve -d '$refund_output'"
  local resolved
  resolved=$(curl -sS -X POST "$BASE/v1/runs/$run_4400/resolve" \
    -H 'content-type: application/json' -d "$refund_output") \
    || die "the resolve to reach the server" "curl exited nonzero"
  echo "  $resolved"
  desk "$out-6b.out" --thread orders-4400 --ask "$ASK_4400"
  if want "$(count_matching "$out-6a.out" "trust_completion = false")" "1" \
          "the stop to name the declaration that caused it" \
     && want "$(count_lines "$large")" "1" "1 line in the large-refunds ledger" \
     && want "$typo_status" "400" "the mistyped resolution to be refused with 400" \
     && want "$(count_matching "$out-6-typo.json" "amount_cents")" "1" \
             "the refusal to name the field it would not let through" \
     && want "$(count_matching "$out-6-typo.json" '"code":"bad_request"')" "1" \
             "the refusal to carry the bad_request code" \
     && want "$(grep -c -F -- '"resolved":true' <<<"$resolved" || true)" "1" "the resolve to be accepted" \
     && want "$APP_EXIT" "0" "the invoke after the resolve to exit 0" \
     && want "$(field "$out-6b.out" 'TOOL BODIES')" "0" \
             "the resolved call to replay rather than run again" \
     && want "$(field "$out-6b.out" MARKERS)" "replayed@1,replayed@5,live@9" \
             "the resolved position to replay and only the closing turn to be live" \
     && want "$(count_lines "$large")" "1" "the large-refunds ledger still at 1 line"; then
    echo "PROOF: $DESK_LANG, a \`trust_completion = false\` refund stopped for a person; a resolution with the wrong amount was refused 400 naming \`amount_cents\`, the right one was recorded over HTTP, and the next invoke replayed it with the tool body not run."
  fi

  # -- 7. finishing the thread ----------------------------------------------
  echo
  echo "-- 7. finishing the thread, and what an invoke gets afterwards --"
  desk "$out-7a.out" --thread orders-7781 --finish
  if [[ "$APP_EXIT" != "0" ]]; then
    die "finishThread to exit 0" "$(cat "$out-7a.out")"
  fi
  desk "$out-7b.out" --thread orders-7781 --ask "$ASK_7781"
  if want "$APP_EXIT" "3" "an invoke of a finished thread to be refused and exit 3" \
     && want "$(count_matching "$out-7b.out" "REFUSED thread_finished")" "1" \
             "the refusal to name thread_finished" \
     && want "$(grep -c "REFUSED thread_finished.*orders-7781" "$out-7b.out" || true)" "1" \
             "the refusal to name the thread it refused"; then
    echo "PROOF: $DESK_LANG, the thread was closed at $(field "$out-7a.out" FINISHED), and the next invoke was refused by name, naming the thread."
  fi

  # -- 8. the same refund twice in one turn ---------------------------------
  #
  # Proof 3 is about the same call retried. This is about two calls, at two
  # positions, that are the same refund. A positional key would call them two
  # refunds and the desk would move the money twice; `refund-order.toml` names
  # `idempotency_key = ["order_id"]`, so both intents derive one key, the second
  # finds the first already settled, and the desk performs nothing for it.
  echo
  echo "-- 8. the same refund asked for twice in one turn --"
  desk "$out-8.out" --thread orders-5150 --ask "$ASK_5150"
  local run_5150
  run_5150=$(field "$out-8.out" RUN)
  if want "$APP_EXIT" "0" "the duplicated-refund invoke to exit 0" \
     && want "$(field "$out-8.out" 'MODEL CALLS')" "2" \
             "2 model calls: the turn that asked twice, and the one that closed out" \
     && want "$(field "$out-8.out" 'TOOL BODIES')" "1" \
             "exactly one refund body to run for the two calls" \
     && want "$(count_matching "$refunds" ORD-5150)" "1" \
             "1 line for ORD-5150 in the refunds ledger" \
     && want "$(tool_facts "$STORE" "$run_5150" refund_order | sed 's/ keys=.*//')" \
             "intents=2 completions=2 dedup=1 distinct_keys=1" \
             "two refund intents under one key, both completed, one of them copied"; then
    echo "PROOF: $DESK_LANG, the same refund asked for twice in one turn: two intents sharing one derived key, the second settled from the first without the desk running its body, and one line in the ledger. $(tool_facts "$STORE" "$run_5150" refund_order)"
  fi

  echo
  echo "== the recorded log of thread orders-7781 =="
  "$SALVOR" --store "$STORE" history "$run_7781" | sed 's/^/  /'

  kill "$SERVE_PID" 2>/dev/null || true
  wait "$SERVE_PID" 2>/dev/null || true
  SERVE_PID=""
}

# --- TypeScript: the packages the example needs -----------------------------

echo "############################################################"
echo "# setting up"
echo "############################################################"
NODE_VERSION=$("$NODE" --version 2>/dev/null) || {
  echo "no \`$NODE\` on PATH. Node 22.18 or newer runs app.ts directly, types and all." >&2
  echo "Point SALVOR_EXAMPLE_NODE at one if it lives somewhere else." >&2
  exit 1
}
echo "node $NODE_VERSION"
if ! "$NODE" -e 'const [major, minor] = process.versions.node.split(".").map(Number); process.exit(major > 22 || (major === 22 && minor >= 18) ? 0 : 1);'; then
  echo "app.ts is TypeScript run directly, which needs Node 22.18 or newer (this is $NODE_VERSION)." >&2
  echo "Point SALVOR_EXAMPLE_NODE at a newer one." >&2
  exit 1
fi
if [[ ! -d "$HERE/node_modules" ]]; then
  echo "== installing the example's node packages =="
  # --omit=optional leaves `@langchain/anthropic` out, since the key-free path
  # never loads it.
  (cd "$HERE" && "$NPM" install --omit=optional --no-audit --no-fund)
fi

# --- Python: the SDK with its LangChain extra -------------------------------

if [[ -n "${SALVOR_EXAMPLE_PYTHON:-}" ]]; then
  # An interpreter that already has `salvor[langchain]` in it, so a run does not
  # reinstall what is already there.
  PYTHON="$SALVOR_EXAMPLE_PYTHON"
else
  PYVENV="${SALVOR_EXAMPLE_PYVENV:-$SCRATCH/salvor-langchain-venv}"
  if [[ ! -x "$PYVENV/bin/python" ]]; then
    echo "== creating a venv for the Python desk at $PYVENV =="
    python3 -m venv "$PYVENV"
    "$PYVENV/bin/pip" install --quiet --upgrade pip
  fi
  echo "== installing salvor with its LangChain extra =="
  "$PYVENV/bin/pip" install --quiet 'salvor[langchain]>=0.10.0,<0.11'
  PYTHON="$PYVENV/bin/python"
fi
if ! "$PYTHON" -c 'import salvor.langchain' 2>/dev/null; then
  echo "\`import salvor.langchain\` fails under $PYTHON." >&2
  echo "Install salvor with its extra:" >&2
  echo "  $PYTHON -m pip install 'salvor[langchain]>=0.10.0,<0.11'" >&2
  exit 1
fi
echo "python $("$PYTHON" --version 2>&1), with salvor.langchain importable"

run_language typescript "$TS_PORT" ts
run_language python "$PY_PORT" py

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held =="
else
  echo "== FAILURES above =="
  exit 1
fi
