#!/usr/bin/env bash
#
# publish-crates.sh: publish the salvor family to crates.io, bottom-up.
#
# The order below is the dependency topology, and it is not negotiable: crates.io resolves a new
# crate's dependencies against the index at upload time, so a crate whose sibling is not published
# yet is rejected. `cargo publish` waits for the index to carry each upload before returning, so a
# sequential run is enough; no sleeps between crates are needed for correctness.
#
# Idempotent. A version already on the registry is skipped rather than treated as a failure, so a
# re-run after an interruption picks up where it stopped, and a CI re-run of the same tag is safe.
#
# Authentication comes from the environment: CARGO_REGISTRY_TOKEN in CI (trusted publishing mints a
# short-lived one), or ~/.cargo/credentials.toml for a local run.
#
# Usage:
#   scripts/publish-crates.sh            # publish
#   scripts/publish-crates.sh --dry-run  # package everything, upload nothing

set -uo pipefail

DRY_RUN=""
case "${1:-}" in
  --dry-run) DRY_RUN="--dry-run" ;;
  "") ;;
  *)
    echo "usage: $0 [--dry-run]" >&2
    exit 2
    ;;
esac

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Bottom-up. `salvor` is last: its `=` pins refuse to resolve until every sibling is on the index at
# the same version, which is exactly the check that catches a partial run.
ORDER=(
  salvor-replay
  salvor-core
  salvor-graph
  salvor-llm
  salvor-store
  salvor-store-conformance
  salvor-tools-macros
  salvor-tools
  salvor-runtime
  salvor-engine
  salvor-wasm
  salvor-server
  salvor-cli-core
  salvor-cli
  salvor-replay-wasm
  salvor-cli-wasm
  salvor
)

VERSION=$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -1)
echo "publishing the salvor family at ${VERSION}${DRY_RUN:+ (dry run)}"

published=0
skipped=0

for crate in "${ORDER[@]}"; do
  # Ask the index directly rather than parsing a failure: "already published" is a normal state
  # here, not an error to recover from.
  if curl -sf -m 20 "https://index.crates.io/$(echo "$crate" | cut -c1-2)/$(echo "$crate" | cut -c3-4)/$crate" 2>/dev/null |
    grep -q "\"vers\":\"$VERSION\""; then
    echo "  $crate $VERSION already on crates.io, skipping"
    skipped=$((skipped + 1))
    continue
  fi

  echo "  publishing $crate"
  for attempt in 1 2 3; do
    if out=$(cargo publish -p "$crate" --no-verify $DRY_RUN 2>&1); then
      published=$((published + 1))
      break
    fi
    if printf '%s' "$out" | grep -q "already exists\|already uploaded"; then
      echo "    already on crates.io (raced), continuing"
      skipped=$((skipped + 1))
      break
    fi
    # New versions of an existing crate are rate limited far more loosely than new crates, but a
    # 15-crate run can still trip it.
    if printf '%s' "$out" | grep -qi "too many\|rate limit\|429"; then
      echo "    rate limited, waiting 60s (attempt $attempt)"
      sleep 60
      continue
    fi
    echo "    FAILED:"
    printf '%s\n' "$out" | tail -20
    echo "  stopping: the crates after this one depend on it"
    exit 1
  done
done

echo "done: $published published, $skipped already present"
