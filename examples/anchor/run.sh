#!/usr/bin/env bash
# What an anchor catches that the store on its own cannot, proved end to end
# and offline.
#
#   anchor two runs -> verify (intact) -> let one run grow -> verify (still
#       intact, and the growth is named) -> forge one run in the database,
#       recomputing every hash the way SECURITY.md publishes it -> the store
#       reads the forgery as history and says nothing -> verify against the
#       anchor names the run `rewritten`, with both hashes -> the same for a
#       run cut short -> the two vacuous checks are refused rather than passed
#       -> re-anchoring over the anchor of a rewritten store is refused
#
# The claim being tested is the one SECURITY.md states plainly: the chain is
# unkeyed, so everything the verifier uses sits in the database beside the rows.
# Somebody who can write the file can rewrite a run from its first event and
# recompute every hash and the recorded head, and the store then reads clean,
# because every value its own check compares was rewritten too. The forgery
# below uses no salvor code at all: python's stdlib sqlite3, sha256, and the
# chain definition as it is published. The anchor is the one statement about
# those heads that no recomputation inside the database can reach.
#
# Usage, from anywhere:
#     bash examples/anchor/run.sh
#
# No API key, no network, no port, and no server: every command here is driven
# by `--store` against a SQLite file. It drives the binary a checkout builds,
# preferring `target/debug` and falling back to `target/release`, so either
# `cargo build` or `cargo build --release` is enough. `SALVOR_BIN` overrides
# that path outright, which is how an already-installed CLI drives this
# instead. The scratch paths are overridable too; see the block below.
set -euo pipefail

# Repository root, two levels up from this script. The claim run below is the
# checked-in `examples/hero` fixture, whose agent names its MCP server by a path
# relative to the directory salvor is invoked from, so everything runs from the
# root.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

SCRATCH="${SALVOR_EXAMPLE_SCRATCH:-${TMPDIR:-/tmp}}"
# The store this example anchors, and the copies the two forgeries are made on,
# so the honest store is still there to compare against at the end.
STORE="${SALVOR_EXAMPLE_STORE:-$SCRATCH/salvor-anchor.db}"
REWRITTEN="$SCRATCH/salvor-anchor-rewritten.db"
SHORTENED="$SCRATCH/salvor-anchor-shortened.db"
EMPTY_STORE="$SCRATCH/salvor-anchor-empty.db"
# A path with no database at it, and there is a proof below that this file is
# still not there afterwards.
TYPO_STORE="$SCRATCH/salvor-anchor-typo.db"
# The anchor, and a copy taken the moment it is written, so "the anchor was not
# touched" is a byte comparison rather than a claim.
ANCHOR="$SCRATCH/salvor-anchor-heads.json"
ANCHOR_KEPT="$SCRATCH/salvor-anchor-heads.kept.json"
EMPTY_ANCHOR="$SCRATCH/salvor-anchor-empty.json"
TYPO_ANCHOR="$SCRATCH/salvor-anchor-typo.json"
# The hero fixture's tool appends one line here per real execution. It is the
# fixture's ledger, not this example's evidence, but it has to land in the
# scratch directory rather than in the repository.
CLAIMS="$SCRATCH/salvor-anchor-claims.txt"
OUT="$SCRATCH/salvor-anchor"

GRAPH="examples/anchor/sign-off.json"

mkdir -p "$SCRATCH"

# The first build of `$1` this checkout has, debug before release. A release
# build is a real build: telling somebody who ran `cargo build --release` to run
# `cargo build` is asking them to compile the same code a second time.
built_bin() {
  local candidate
  for candidate in "$ROOT/target/debug/$1" "$ROOT/target/release/$1"; do
    if [[ -x "$candidate" ]]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

SALVOR="${SALVOR_BIN:-$(built_bin salvor || true)}"
if [[ -z "$SALVOR" ]]; then
  echo "missing the \`salvor\` binary: no target/debug/salvor and no target/release/salvor." >&2
  echo "Build it with:  cargo build      (or: cargo build --release)" >&2
  echo "Or point SALVOR_BIN at one you already have." >&2
  exit 1
fi
if [[ ! -x "$SALVOR" ]]; then
  echo "SALVOR_BIN names \`$SALVOR\`, which is not an executable file." >&2
  exit 1
fi

# Everything starts from nothing, so the event counts below mean what they say.
# Only files this script owns, by exact path, are removed.
rm -f "$STORE" "$STORE-wal" "$STORE-shm" \
      "$REWRITTEN" "$REWRITTEN-wal" "$REWRITTEN-shm" \
      "$SHORTENED" "$SHORTENED-wal" "$SHORTENED-shm" \
      "$EMPTY_STORE" "$EMPTY_STORE-wal" "$EMPTY_STORE-shm" \
      "$TYPO_STORE" "$TYPO_STORE-wal" "$TYPO_STORE-shm" \
      "$ANCHOR" "$ANCHOR_KEPT" "$EMPTY_ANCHOR" "$TYPO_ANCHOR" \
      "$CLAIMS" "$OUT"-*.out

# The fixture's tool writes here, by ordinary environment inheritance through
# salvor to the tool's child process, so the repository stays clean.
export SALVOR_HERO_CLAIMS="$CLAIMS"

# The reports are the story; the runtime's own event lines are not, and this
# example prints the recorded logs itself where they matter. Only when the
# caller has not already chosen a filter.
export RUST_LOG="${RUST_LOG:-warn}"

# NOTHING IS SPAWNED IN THE BACKGROUND BY THIS SCRIPT. There is no server, no
# port, and no pid to record: `run`, `graph run`, `resume`, `anchor` and
# `verify` each open the store, do their work, and exit.

FAIL=0

# --- Saying what failed, every time ---
#
# Every check below names itself when it fails. A bare `grep` or `[[ ... ]]`
# under `set -euo pipefail` exits 1 having printed NOTHING, and a proof script
# that can die silently is worse than one that proves less: a reader cannot tell
# a run that checked everything from one that stopped before it got there.

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

# check <actual> <expected> <the numbered proof this states> <what was expected>
check() {
  if [[ "$1" == "$2" ]]; then
    echo "PROOF $3"
  else
    fail "$4" "$1"
  fi
}

# Runs a command that is expected to fail sometimes, keeping its exit code in
# STATUS and its output (both streams) in the named file. Exit codes are the
# whole answer here: 0, 1 and 2 are three different sentences, so every one of
# them below is asserted exactly rather than checked for "nonzero".
STATUS=0
capture() {
  local out="$1"
  shift
  set +e
  "$@" >"$out" 2>&1
  STATUS=$?
  set -e
}

# The number of events in a run's recorded log, as one bare number.
count_events() {
  local lines
  lines=$("$SALVOR" --store "$1" history "$2") \
    || die "run $2's log in $1 to read back" "\`salvor history\` exited nonzero"
  grep -c . <<<"$lines" || true
}

# The hash an anchor file records for one run, read out of the JSON rather than
# out of any output being checked.
anchored_hash() {
  python3 - "$1" "$2" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    document = json.load(handle)
for entry in document["runs"]:
    if entry["run"] == sys.argv[2]:
        print(entry["hash"])
        break
PY
}

# A copy of a store, journal files included, so the copy is the same database
# rather than most of one.
copy_store() {
  local from="$1" to="$2" suffix
  cp "$from" "$to"
  for suffix in -wal -shm; do
    rm -f "$to$suffix"
    [[ -f "$from$suffix" ]] && cp "$from$suffix" "$to$suffix"
  done
  return 0
}

# --- The forgery, with nothing from salvor ---
#
# SECURITY.md publishes everything this needs: the preimage
#
#     salvor.chain.v1 \n prev \n run_id \n seq \n envelope_json
#
# hashed with SHA-256 and rendered as 64 lowercase hex characters, with 64 zeros
# as `prev` for a run's first row; the `events` table, whose `envelope` column
# holds the exact recorded bytes that are hashed and whose `chain_idx` is the
# row's position in its run's append order; and `chain_heads`, which holds one
# recorded head per run as `chain_len` and `head_hash`. The append-only triggers
# on `events` are a locked door on a building with windows: whoever can write
# the file can take them off and put them back, which is what happens here.
#
#     forge rewrite  <db> <run-id> <old-text> <new-text>
#     forge truncate <db> <run-id>
forge() {
  python3 - "$@" <<'PY'
import hashlib
import sqlite3
import sys

SPEC = "salvor.chain.v1"
GENESIS = "0" * 64


def row_hash(prev, run_id, seq, envelope):
    preimage = f"{SPEC}\n{prev}\n{run_id}\n{seq}\n{envelope}"
    return hashlib.sha256(preimage.encode("utf-8")).hexdigest()


mode, database, run_id = sys.argv[1], sys.argv[2], sys.argv[3]
conn = sqlite3.connect(database)
conn.executescript(
    "DROP TRIGGER IF EXISTS events_refuse_update;"
    "DROP TRIGGER IF EXISTS events_refuse_delete;"
)

rows = conn.execute(
    "SELECT seq, envelope FROM events WHERE run_id = ? ORDER BY chain_idx",
    (run_id,),
).fetchall()
if not rows:
    raise SystemExit(f"no rows for run {run_id} in {database}")

if mode == "rewrite":
    old, new = sys.argv[4], sys.argv[5]
    # Every row of the run, from its first event forward: the recorded bytes
    # rewritten, then the whole chain rebuilt over them and the recorded head
    # moved to the end of it.
    prev = GENESIS
    for seq, envelope in rows:
        envelope = envelope.replace(old, new)
        current = row_hash(prev, run_id, seq, envelope)
        conn.execute(
            "UPDATE events SET envelope = ?, prev_hash = ?, row_hash = ? "
            "WHERE run_id = ? AND seq = ?",
            (envelope, prev, current, run_id, seq),
        )
        prev = current
    conn.execute(
        "UPDATE chain_heads SET chain_len = ?, head_hash = ? WHERE run_id = ?",
        (len(rows), prev, run_id),
    )
elif mode == "truncate":
    # The last row dropped and the recorded head walked back onto the row
    # before it. Nothing else has to change: every remaining link still holds.
    kept = rows[:-1]
    if not kept:
        raise SystemExit(f"run {run_id} has one row; there is nothing to cut back to")
    conn.execute(
        "DELETE FROM events WHERE run_id = ? AND seq = ?", (run_id, rows[-1][0])
    )
    head = conn.execute(
        "SELECT row_hash FROM events WHERE run_id = ? AND seq = ?",
        (run_id, kept[-1][0]),
    ).fetchone()[0]
    conn.execute(
        "UPDATE chain_heads SET chain_len = ?, head_hash = ? WHERE run_id = ?",
        (len(kept), head, run_id),
    )
else:
    raise SystemExit(f"unknown mode {mode}")

conn.executescript(
    "CREATE TRIGGER IF NOT EXISTS events_refuse_update BEFORE UPDATE ON events "
    "BEGIN SELECT RAISE(ABORT, 'salvor: events is append-only, UPDATE refused'); END;"
    "CREATE TRIGGER IF NOT EXISTS events_refuse_delete BEFORE DELETE ON events "
    "BEGIN SELECT RAISE(ABORT, 'salvor: events is append-only, DELETE refused'); END;"
)
conn.commit()
conn.close()
PY
}

echo "############################################"
echo "# 1. Two runs, one anchor, and a check that passes"
echo "############################################"
# The claim run is the checked-in hero fixture: ten events, offline, no key.
capture "$OUT-claim.out" "$SALVOR" --store "$STORE" run --fixture examples/hero
if [[ "$STATUS" != "0" ]]; then
  cat "$OUT-claim.out"
  die "the claim run to complete (exit 0)" "it exited $STATUS; its output is above"
fi
CLAIM_ID=$(grep -oE 'run [0-9a-f-]{36}' "$OUT-claim.out" | head -1 | awk '{print $2}')
[[ -n "$CLAIM_ID" ]] || die "the claim run to print a \`run <id>\` line" "$(cat "$OUT-claim.out")"

# The second run is a one-node graph: a gate, which parks the run for a person
# and gives this example a run that can still honestly grow after the anchor.
capture "$OUT-sign.out" "$SALVOR" --store "$STORE" graph run "$GRAPH" \
  --input '{"quarter":"2026-Q3"}'
if [[ "$STATUS" != "0" ]]; then
  cat "$OUT-sign.out"
  die "the sign-off run to park at its gate (exit 0)" "it exited $STATUS; its output is above"
fi
SIGN_ID=$(grep -oE 'run [0-9a-f-]{36}' "$OUT-sign.out" | head -1 | awk '{print $2}')
[[ -n "$SIGN_ID" ]] || die "the sign-off run to print a \`run <id>\` line" "$(cat "$OUT-sign.out")"

echo "the store now holds two runs:"
"$SALVOR" --store "$STORE" list || die "\`salvor list\` to succeed" "it exited nonzero"

CLAIM_LEN=$(count_events "$STORE" "$CLAIM_ID")
SIGN_LEN=$(count_events "$STORE" "$SIGN_ID")

echo
# The anchor lands in the scratch directory, beside the store, because a script
# cannot put a file on another machine. That is the one thing this example
# cannot demonstrate honestly, and salvor says so itself in the warning below.
# Nothing here depends on it: no step rewrites the anchor, and the last proof
# checks it byte for byte.
capture "$OUT-anchor.out" "$SALVOR" --store "$STORE" anchor --out "$ANCHOR"
cat "$OUT-anchor.out"
if [[ "$STATUS" != "0" ]]; then
  die "\`salvor anchor\` to take the anchor (exit 0)" "it exited $STATUS; its output is above"
fi
[[ -f "$ANCHOR" ]] || die "the anchor file at $ANCHOR" "it is not there"
cp "$ANCHOR" "$ANCHOR_KEPT"
echo
echo "the anchor, which is the whole file:"
cat "$ANCHOR"

if grep -q "anchored 2 run(s)" "$OUT-anchor.out" \
   && grep -q "$CLAIM_ID" "$ANCHOR" && grep -q "$SIGN_ID" "$ANCHOR"; then
  echo "PROOF 1a: the anchor records both runs, one head hash and one length each, and nothing else."
else
  fail "an anchor over 2 runs naming $CLAIM_ID and $SIGN_ID" "$(cat "$OUT-anchor.out"; cat "$ANCHOR")"
fi
if grep -q "is in the same directory as $STORE" "$OUT-anchor.out"; then
  echo "PROOF 1b: an anchor written beside the store it describes is called out as answering nothing, because whoever can rewrite the store can rewrite it too. That is custody, and a shell script cannot fix it; on a real box the file goes somewhere the store's writer cannot reach."
else
  fail "the anchor to warn that $ANCHOR sits in the same directory as $STORE" \
       "$(cat "$OUT-anchor.out")"
fi

echo
capture "$OUT-verify-1.out" "$SALVOR" --store "$STORE" verify --against "$ANCHOR"
cat "$OUT-verify-1.out"
check "$STATUS" "0" \
  "1c: verifying an untouched store against its anchor exits 0, the only pass there is." \
  "\`salvor verify\` to exit 0 over an untouched store"
if grep -q "run $CLAIM_ID: intact at $CLAIM_LEN event(s)" "$OUT-verify-1.out" \
   && grep -q "run $SIGN_ID: intact at $SIGN_LEN event(s)" "$OUT-verify-1.out" \
   && grep -q "2 run(s) anchored, 2 intact, 0 failed, 0 new since the anchor" "$OUT-verify-1.out"; then
  echo "PROOF 1d: both runs come back intact by name, at $CLAIM_LEN and $SIGN_LEN events, and the summary closes: 2 anchored, 2 intact, 0 failed."
else
  fail "both runs intact at $CLAIM_LEN and $SIGN_LEN events, and a closing summary" \
       "$(cat "$OUT-verify-1.out")"
fi

echo
echo "############################################"
echo "# 2. The run grows, and the anchor still holds: it commits to the prefix"
echo "############################################"
capture "$OUT-resume.out" "$SALVOR" --store "$STORE" resume "$SIGN_ID" \
  --graph "$GRAPH" --input '{"approved":true,"by":"the claims desk"}'
cat "$OUT-resume.out"
if [[ "$STATUS" != "0" ]]; then
  die "the gate to be answered and the run to finish (exit 0)" "it exited $STATUS; its output is above"
fi
SIGN_GROWN=$(count_events "$STORE" "$SIGN_ID")
SINCE=$((SIGN_GROWN - SIGN_LEN))
if (( SINCE > 0 )); then
  echo "PROOF 2a: the anchored run grew honestly: $SIGN_LEN events when it was anchored, $SIGN_GROWN now."
else
  fail "the sign-off run to hold more than $SIGN_LEN events after the resume" "$SIGN_GROWN"
fi

echo
capture "$OUT-verify-2.out" "$SALVOR" --store "$STORE" verify --against "$ANCHOR"
cat "$OUT-verify-2.out"
check "$STATUS" "0" \
  "2b: a store that grew since its anchor still exits 0: the anchor commits to the prefix it recorded, not to the size of the run." \
  "\`salvor verify\` to exit 0 over a store that grew"
if grep -q "run $SIGN_ID: intact: $SIGN_GROWN event(s), anchored at $SIGN_LEN, $SINCE recorded since" \
     "$OUT-verify-2.out"; then
  echo "PROOF 2c: the report names the growth rather than hiding it: intact at $SIGN_GROWN events, anchored at $SIGN_LEN, $SINCE recorded since. Those $SINCE events are what this anchor says nothing about."
else
  fail "an intact line naming $SIGN_GROWN events, anchored at $SIGN_LEN, $SINCE recorded since" \
       "$(cat "$OUT-verify-2.out")"
fi

echo
echo "############################################"
echo "# 3. The forgery the store cannot detect"
echo "############################################"
copy_store "$STORE" "$REWRITTEN"
echo "a copy of the store at $REWRITTEN, forged in place with sqlite and sha256:"
forge rewrite "$REWRITTEN" "$CLAIM_ID" "ss-waratah" "ss-cumberland" \
  || die "the forgery to run" "the python rewrite exited nonzero"

capture "$OUT-forged-list.out" "$SALVOR" --store "$REWRITTEN" list
cat "$OUT-forged-list.out"
check "$STATUS" "0" \
  "3a: \`salvor list\` reads the forged store without complaint: two runs, the same statuses, exit 0." \
  "\`salvor list\` to exit 0 over the forged store"

capture "$OUT-forged-history.out" "$SALVOR" --store "$REWRITTEN" history "$CLAIM_ID"
cat "$OUT-forged-history.out"
check "$STATUS" "0" \
  "3b: \`salvor history\` reads the forged run back as history: every link recomputes, so the store's own check passes." \
  "\`salvor history\` to exit 0 over the forged run"
if grep -q "ss-cumberland" "$OUT-forged-history.out" \
   && ! grep -q "ss-waratah" "$OUT-forged-history.out"; then
  echo "PROOF 3c: the recorded log now says the claim was for ss-cumberland, start to finish, and the wreck it was really recorded against appears nowhere in it."
else
  fail "the forged log to name ss-cumberland throughout and ss-waratah nowhere" \
       "$(cat "$OUT-forged-history.out")"
fi

echo
CLAIM_ANCHORED_HASH=$(anchored_hash "$ANCHOR" "$CLAIM_ID")
[[ -n "$CLAIM_ANCHORED_HASH" ]] || die "the anchor to record a hash for $CLAIM_ID" "$(cat "$ANCHOR")"
capture "$OUT-verify-3.out" "$SALVOR" --store "$REWRITTEN" verify --against "$ANCHOR"
cat "$OUT-verify-3.out"
check "$STATUS" "1" \
  "3d: the check the store cannot do exits 1: the anchor is outside the database, so recomputing everything inside it changes nothing here." \
  "\`salvor verify\` to exit 1 over the forged store"
FOUND_HASH=$(grep -oE 'this store holds  [0-9a-f]{64}' "$OUT-verify-3.out" | awk '{print $NF}' || true)
if grep -q "run $CLAIM_ID: rewritten at seq $((CLAIM_LEN - 1))" "$OUT-verify-3.out" \
   && grep -q "the anchor recorded $CLAIM_ANCHORED_HASH" "$OUT-verify-3.out" \
   && [[ -n "$FOUND_HASH" && "$FOUND_HASH" != "$CLAIM_ANCHORED_HASH" ]]; then
  echo "PROOF 3e: the report names the run \`rewritten\`, at the seq to go and read, and prints both hashes: the anchor recorded $CLAIM_ANCHORED_HASH and this store holds $FOUND_HASH."
else
  fail "a \`rewritten\` finding for $CLAIM_ID carrying the anchored hash and a different found one" \
       "$(cat "$OUT-verify-3.out")"
fi
if grep -q "run $SIGN_ID: intact" "$OUT-verify-3.out" \
   && grep -q "2 run(s) anchored, 1 intact, 1 failed" "$OUT-verify-3.out"; then
  echo "PROOF 3f: the finding is one run wide: the other anchored run is still reported intact by name, so the report says what was touched and what was not."
else
  fail "the sign-off run to still come back intact beside the failure" "$(cat "$OUT-verify-3.out")"
fi

echo
echo "############################################"
echo "# 4. A run cut short, on a fresh copy of the honest store"
echo "############################################"
copy_store "$STORE" "$SHORTENED"
forge truncate "$SHORTENED" "$CLAIM_ID" \
  || die "the truncation to run" "the python truncate exited nonzero"

capture "$OUT-short-history.out" "$SALVOR" --store "$SHORTENED" history "$CLAIM_ID"
cat "$OUT-short-history.out"
check "$STATUS" "0" \
  "4a: the shortened store reads clean too: with the recorded head walked back one row, nothing inside the database is inconsistent." \
  "\`salvor history\` to exit 0 over the shortened run"
SHORT_LEN=$(count_events "$SHORTENED" "$CLAIM_ID")
check "$SHORT_LEN" "$((CLAIM_LEN - 1))" \
  "4b: the run now holds $((CLAIM_LEN - 1)) events where it held $CLAIM_LEN, and its last recorded event, the completion, is gone." \
  "$((CLAIM_LEN - 1)) events in the shortened run"

echo
capture "$OUT-verify-4.out" "$SALVOR" --store "$SHORTENED" verify --against "$ANCHOR"
cat "$OUT-verify-4.out"
check "$STATUS" "1" \
  "4c: a run cut short exits 1 as well, and the anchor is again the only record of what was there." \
  "\`salvor verify\` to exit 1 over the shortened store"
if grep -q "run $CLAIM_ID: shortened. The anchor recorded $CLAIM_LEN event(s); this store holds $SHORT_LEN" \
     "$OUT-verify-4.out"; then
  echo "PROOF 4d: the finding is \`shortened\`, and it says which number is which: the anchor recorded $CLAIM_LEN events and this store holds $SHORT_LEN."
else
  fail "a \`shortened\` finding naming $CLAIM_LEN anchored events and $SHORT_LEN held" \
       "$(cat "$OUT-verify-4.out")"
fi

echo
echo "############################################"
echo "# 5. The two checks that would mean nothing are refused, not passed"
echo "############################################"
# A store that exists and holds no runs. A sweep for due timers opens the store
# for writing, so it creates the file and finds nothing in it; `anchor` and
# `verify` never create one, which is the proof after next.
"$SALVOR" --store "$EMPTY_STORE" wake --dry-run >"$OUT-empty-open.out" 2>&1 \
  || die "an empty store to be created and read" "$(cat "$OUT-empty-open.out")"

capture "$OUT-anchor-empty.out" "$SALVOR" --store "$EMPTY_STORE" anchor --out "$EMPTY_ANCHOR"
cat "$OUT-anchor-empty.out"
check "$STATUS" "2" \
  "5a: anchoring a store that holds no runs is refused (exit 2): nothing ran, which is neither a pass nor a finding." \
  "\`salvor anchor\` to exit 2 over an empty store"
if [[ -f "$EMPTY_ANCHOR" ]]; then
  fail "nothing written at $EMPTY_ANCHOR by a refused anchor" "the file is there"
fi

# Taken deliberately, the way an operator would when a store is empty on
# purpose and a file still has to appear on schedule.
capture "$OUT-anchor-empty-ok.out" "$SALVOR" --store "$EMPTY_STORE" anchor \
  --allow-empty --out "$EMPTY_ANCHOR"
cat "$OUT-anchor-empty-ok.out"
[[ "$STATUS" == "0" ]] || die "\`anchor --allow-empty\` to write an anchor over zero runs" \
  "it exited $STATUS: $(cat "$OUT-anchor-empty-ok.out")"

echo
capture "$OUT-verify-empty.out" "$SALVOR" --store "$STORE" verify --against "$EMPTY_ANCHOR"
cat "$OUT-verify-empty.out"
check "$STATUS" "2" \
  "5b: verifying the real store against an anchor of zero runs exits 2 rather than 0: a pass over nothing prints exactly like a pass over everything, so it is refused." \
  "\`salvor verify\` to exit 2 against an anchor committing to no runs"
if grep -q "commits to nothing" "$OUT-verify-empty.out" \
   && grep -q -- "--allow-empty" "$OUT-verify-empty.out"; then
  echo "PROOF 5c: the refusal says the anchor commits to nothing and names the flag that accepts one deliberately."
else
  fail "the refusal to say the anchor commits to nothing and name --allow-empty" \
       "$(cat "$OUT-verify-empty.out")"
fi

echo
capture "$OUT-anchor-typo.out" "$SALVOR" --store "$TYPO_STORE" anchor --out "$TYPO_ANCHOR"
cat "$OUT-anchor-typo.out"
check "$STATUS" "2" \
  "5d: a mistyped --store is refused (exit 2) rather than being created and anchored empty." \
  "\`salvor anchor\` to exit 2 on a store path that does not exist"
if [[ ! -e "$TYPO_STORE" && ! -e "$TYPO_ANCHOR" ]]; then
  echo "PROOF 5e: nothing was created: no database at the mistyped path, and no anchor beside it. The one thing worse than no anchor is an anchor over a store the typo just made."
else
  fail "neither $TYPO_STORE nor $TYPO_ANCHOR to exist after the refusal" \
       "store: $([[ -e "$TYPO_STORE" ]] && echo present || echo absent); anchor: $([[ -e "$TYPO_ANCHOR" ]] && echo present || echo absent)"
fi

echo
echo "############################################"
echo "# 6. Re-anchoring over the evidence is refused"
echo "############################################"
capture "$OUT-anchor-again.out" "$SALVOR" --store "$REWRITTEN" anchor --out "$ANCHOR"
cat "$OUT-anchor-again.out"
check "$STATUS" "1" \
  "6a: taking a fresh anchor from the forged store, onto the file that would catch it, is refused (exit 1)." \
  "\`salvor anchor --out\` to exit 1 over an anchor this store no longer verifies against"
if grep -q "no longer verifies against the anchor already at $ANCHOR" "$OUT-anchor-again.out" \
   && grep -q "1 of 2 anchored run(s) failed" "$OUT-anchor-again.out"; then
  echo "PROOF 6b: the refusal says why in the operator's own terms, counts the runs that failed, and prints the verify line to run."
else
  fail "a refusal naming $ANCHOR and counting 1 of 2 anchored runs failed" \
       "$(cat "$OUT-anchor-again.out")"
fi
if cmp -s "$ANCHOR" "$ANCHOR_KEPT"; then
  echo "PROOF 6c: the anchor file is byte for byte what it was when it was written, so the only record of what the heads used to be survived the attempt."
else
  fail "$ANCHOR to be unchanged after the refusal" "it differs from the copy taken when it was written"
fi

echo
if [[ "$FAIL" == "0" ]]; then
  echo "== all proofs held =="
else
  echo "== FAILURES above =="
  exit 1
fi
