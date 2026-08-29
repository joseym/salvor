#!/usr/bin/env bash
#
# prose-lint.sh: a deterministic linter for AI writing tells in shipped prose.
#
# It reads the named files, or standard input when given none, and greps each
# line for the patterns collected in the labeled arrays below. Any match in a
# blocking category prints the file, line number, offending text, and a short
# reason, and the script exits 1. The rhetorical-question check only warns, so
# it never changes the exit status. Extend a category by adding one array line.

set -u

# ---------------------------------------------------------------------------
# Pattern lists. Each is a plain array so adding a rule is a one-line edit.
# ---------------------------------------------------------------------------

# Em dash and en dash, anywhere on a line.
DASH_PATTERN='—|–'

# Stock vocabulary. Matched case-insensitively on a leading word boundary; the
# optional suffix groups fold plurals and inflections into one entry.
STOCK_WORDS=(
  'seamless(ly)?'
  'robust'
  'leverage(s|d|ing)?'
  'delve'
  'crucial'
  'pivotal'
  'comprehensive'
  'notably'
  'ultimately'
  'empower(s|ing)?'
  'streamline(d|s)?'
  'game.chang'
  'effortless'
)

# Negation pivots: the "not X, it's Y" reframing shape.
NEGATION_PIVOTS=(
  "isn't just"
  'not just a'
  "it's not [a-z].* it's"
  'no ordinary'
)

# Attribution lines and commit/PR footers that should never ship. 'Generated
# with' is anchored on a leading word boundary so it cannot match mid-word
# inside "regenerated with" (a legitimate instruction to rerun a snapshot
# tool), while still catching a capitalized or lowercase "generated with".
ATTRIBUTION=(
  '\bGenerated with'
  'Co-Authored-By'
  'claude.ai/code'
  '🤖'
)

# Process narration: how the change was authored, not what it does. The bare
# product term "adversarial review" is intentionally absent so it never trips;
# only the authoring phrasings below match (e.g. "review pass" catches
# "adversarial review pass").
PROCESS_NARRATION=(
  'review gate'
  'review pass'
  'subagent'
  're-reviewed'
  'happy-path testing'
  'Fable'
)

# Meta-discourse: text about the text. An opening that only describes the
# shape of what follows (a count, a promise of a breakdown) instead of saying
# anything about the subject. The fix is deletion, so every hit blocks.
META_DISCOURSE=(
  '\b(failures?|errors?|the code|the tests?) (speak|tell|admit|confess)'
  '(two|three|four|several|a few) things (land|here|to note|going on)'
  'this (pr|commit|change|patch) does (two|three|four|several)'
  "here's what (changed|happened|this does)"
  'let me break (this|it) down'
  'in this pr[,:]'
)

# Punchline shapes: the setup-punchline rhythm, aphoristic closers, and
# personified verbs on non-human subjects (code, tests, files) that read as
# a rhetorical flourish rather than a plain statement of fact. Each entry
# below was tuned against a 264-line regression corpus of real findings: it
# must flag at least one real instance there and zero lines in this repo's
# current docs. Several tempting shapes (a bare "which is why", a trailing
# ", not just/only/merely ...", "refuses to", "does not care", "is honest
# about") were tried and dropped because this codebase's legitimate
# technical prose uses them constantly; blocking them would flag good
# writing, not just AI tells. The "point"/"reason" entries require the
# clause to end shortly after the trigger word with no colon or comma in
# between, which is what separates an aphoristic closer ("... is the whole
# point.") from an ordinary topic sentence that goes on to explain itself
# ("... is the point of the function: it does X").
PUNCHLINE=(
  'is the (whole )?point[^:.,]*\.'
  'is the whole reason[^:.,]*\.'
  'not (a bug|an accident|a formality)\b'
  'not only [a-z].{0,70}[;,] it '
  '\bearns the\b'
  '\bknows or cares\b'
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

usage() {
  cat <<'EOF'
Usage: prose-lint.sh [FILE...]

Scan the given files (or standard input when none are named) for AI writing
tells: em and en dashes, stock vocabulary, negation pivots, attribution
footers, process narration, meta-discourse openers, and punchline rhythm
(aphoristic closers like "... is the whole point.", dismissals like "not a
bug", and personified verbs on non-human subjects). Each hit prints its
location and reason, and any hit exits 1; a leading short-phrase question
only warns. Extend a check by adding one line to the labeled arrays near the
top of this script.
EOF
}

# Join array elements with '|' to build a grep -E alternation.
join_alt() {
  local out=$1
  shift
  local part
  for part in "$@"; do
    out="$out|$part"
  done
  printf '%s' "$out"
}

fail=0

# Run one blocking check across all inputs. Args: reason, grep-flags, regex.
# grep-flags is passed unquoted so callers can add -i; the regex is a single
# extended-regex string.
check() {
  local reason=$1 flags=$2 regex=$3 hits
  # shellcheck disable=SC2086
  hits=$(grep -HnE $flags -e "$regex" -- "${FILES[@]}" 2>/dev/null)
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits" | while IFS= read -r line; do
      printf '%s  [%s]\n' "$line" "$reason"
    done
    fail=1
  fi
}

# ---------------------------------------------------------------------------
# Argument handling
# ---------------------------------------------------------------------------

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
esac

STDIN_TMP=""
FILES=("$@")

if [ "${#FILES[@]}" -eq 0 ]; then
  STDIN_TMP=$(mktemp "${TMPDIR:-/tmp}/prose-lint.XXXXXX")
  trap 'rm -f "$STDIN_TMP"' EXIT
  cat >"$STDIN_TMP"
  FILES=("$STDIN_TMP")
fi

# A missing or unreadable file must be an error, never a silent pass; a
# typo'd path in a caller would otherwise report clean forever.
for f in "${FILES[@]}"; do
  if [ ! -r "$f" ]; then
    echo "prose-lint: cannot read $f" >&2
    exit 2
  fi
done

# ---------------------------------------------------------------------------
# Blocking checks
# ---------------------------------------------------------------------------

check 'em or en dash' '' "$DASH_PATTERN"
check 'stock vocabulary' '-i' "\\b($(join_alt "${STOCK_WORDS[@]}"))"
check 'negation pivot' '-i' "$(join_alt "${NEGATION_PIVOTS[@]}")"
check 'attribution footer' '-i' "$(join_alt "${ATTRIBUTION[@]}")"
check 'process narration' '-i' "$(join_alt "${PROCESS_NARRATION[@]}")"
check 'meta-discourse opener' '-i' "$(join_alt "${META_DISCOURSE[@]}")"
check 'punchline rhythm' '-i' "$(join_alt "${PUNCHLINE[@]}")"

# ---------------------------------------------------------------------------
# Warning-only check: a line that opens with one to four words then a '?'.
# Too fuzzy to block, so it prints to stderr and leaves the exit status alone.
# ---------------------------------------------------------------------------

rhetorical=$(grep -HnE \
  '^[[:space:]]*[[:alnum:]]+([[:space:]]+[[:alnum:]]+){0,3}[[:space:]]*\?' \
  -- "${FILES[@]}" 2>/dev/null)
if [ -n "$rhetorical" ]; then
  printf '%s\n' "$rhetorical" | while IFS= read -r line; do
    printf 'WARN %s  [possible rhetorical-question opener]\n' "$line" >&2
  done
fi

# Rewrite the temp-file name back to a readable label in any output the caller
# captured is not possible after the fact, so note it here for stdin runs.
if [ -n "$STDIN_TMP" ] && [ "$fail" -ne 0 ]; then
  printf '(paths shown as %s refer to standard input)\n' "$STDIN_TMP" >&2
fi

exit "$fail"
