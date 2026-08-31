#!/usr/bin/env bash
#
# commit-lint.sh: check the commit messages in a revision range.
#
# For every commit in `git rev-list --reverse --no-merges <range>` it checks the subject
# shape and length, the body width, stray trailer lines, the AI writing tells
# that prose-lint.sh already knows about, and a short list of before/after
# narrative and process talk that only belongs in a chat log. A failing commit
# prints its short sha and subject once, then one indented line per hit.
#
# Exit status: 0 clean, 1 on any hit, 2 on a bad range or a missing linter.

set -u

usage() {
  cat <<'EOF'
Usage: commit-lint.sh <range>

Lint every commit message in a revision range, for example main..HEAD or
origin/main..my-branch. Prints each failing commit followed by its hits and
exits 1 if there are any.
EOF
}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PROSE_LINT="$SCRIPT_DIR/prose-lint.sh"

# ---------------------------------------------------------------------------
# Pattern lists. Each is a plain array so adding a rule is a one-line edit.
# These apply to the body only. "gate" is scoped to the review-process senses
# because this project's graph has a legitimate gate node type.
# ---------------------------------------------------------------------------

NARRATIVE=(
  '\bused to\b'
  '\bpreviously\b'
  '\balong the way\b'
  '\bbefore this (change|commit|fix)\b'
  '\bthis (commit|change|pr) (also )?(adds|fixes|introduces|makes)\b'
  '\bwe (now|also|no longer)\b'
  '\bno longer\b'
  '\bround [0-9]\b'
  '\bpersona\b'
  '\b(QA|full|final) gate\b'
)

SUBJECT_RE='^(feat|fix|docs|test|refactor|perf|build|ci|chore|style)(\([a-z0-9-]+\))?: [a-z]'

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
  '')
    usage >&2
    exit 2
    ;;
esac

RANGE=$1

if [ ! -r "$PROSE_LINT" ]; then
  echo "commit-lint: cannot read $PROSE_LINT" >&2
  exit 2
fi

if ! COMMITS=$(git rev-list --reverse --no-merges "$RANGE" 2>/dev/null); then
  echo "commit-lint: not a revision range: $RANGE" >&2
  exit 2
fi

[ -n "$COMMITS" ] || exit 0

TMPDIR_RUN=$(mktemp -d "${TMPDIR:-/tmp}/commit-lint.XXXXXX")
trap 'rm -rf "$TMPDIR_RUN"' EXIT

fail=0

for sha in $COMMITS; do
  short=$(git rev-parse --short=7 "$sha")
  msg_file="$TMPDIR_RUN/$short.msg"
  git log -1 --format=%B "$sha" >"$msg_file"

  subject=$(head -n 1 "$msg_file")
  body_file="$TMPDIR_RUN/$short.body"
  tail -n +2 "$msg_file" >"$body_file"

  hits_file="$TMPDIR_RUN/$short.hits"
  : >"$hits_file"

  # Subject length.
  if [ "${#subject}" -gt 72 ]; then
    printf '  subject is %d characters  [subject over 72]\n' \
      "${#subject}" >>"$hits_file"
  fi

  # Subject shape.
  if ! printf '%s\n' "$subject" | grep -qE "$SUBJECT_RE"; then
    printf '  %s  [subject must be type(scope): lowercase summary]\n' \
      "$subject" >>"$hits_file"
  fi
  case "$subject" in
    *.)
      printf '  %s  [subject ends in a period]\n' "$subject" >>"$hits_file"
      ;;
  esac

  # Body width.
  awk 'length($0) > 72 {
    printf "  body line %d is %d characters  [body line over 72]\n", NR, length($0)
  }' "$body_file" >>"$hits_file"

  # Trailer lines in the final block of the body: walk back over trailing
  # blank lines, then read the last contiguous run of non-blank lines. A block
  # counts as trailers only when every one of its lines is a "Key: value" line
  # with a capitalized key, which is what Co-Authored-By, Signed-off-by and
  # Claude-Session look like. A body paragraph that opens "anchor: ..." and
  # runs on in prose is left alone.
  awk '
    { lines[NR] = $0 }
    END {
      n = NR
      while (n > 0 && lines[n] ~ /^[ \t]*$/) n--
      if (n == 0) exit
      s = n
      while (s > 1 && lines[s - 1] !~ /^[ \t]*$/) s--
      for (i = s; i <= n; i++)
        if (lines[i] !~ /^[A-Z][A-Za-z0-9-]*: /) exit
      for (i = s; i <= n; i++)
        printf "  %s  [trailer line at the end of the body]\n", lines[i]
    }' "$body_file" >>"$hits_file"

  # AI writing tells across the whole message.
  prose=$(bash "$PROSE_LINT" "$msg_file" 2>/dev/null)
  if [ -n "$prose" ]; then
    printf '%s\n' "$prose" | sed "s#^$msg_file:#  message line #" \
      >>"$hits_file"
  fi

  # Before/after narrative and process talk, body only.
  for pat in "${NARRATIVE[@]}"; do
    narr=$(grep -nEi -e "$pat" "$body_file" 2>/dev/null)
    if [ -n "$narr" ]; then
      printf '%s\n' "$narr" | while IFS= read -r line; do
        printf '  body line %s  [narrative or process talk: %s]\n' \
          "$line" "$pat"
      done >>"$hits_file"
    fi
  done

  if [ -s "$hits_file" ]; then
    printf '%s %s\n' "$short" "$subject"
    cat "$hits_file"
    fail=1
  fi
done

exit "$fail"
